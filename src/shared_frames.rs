//! Latest-frame IPC backed by an anonymous Linux memfd. Pixels never enter a codec.
//!
//! A short flock protects publication and the reader's private copy. The producer
//! drops a frame rather than blocking capture. Abstract datagrams are wakeups only;
//! the sequence in the buffer is authoritative, so a slow reader skips old frames.
use anyhow::{Context, Result, bail};
use fs2::FileExt;
use nix::libc;
use std::{
    ffi::CString,
    fs::File,
    os::{
        fd::FromRawFd,
        linux::net::SocketAddrExt,
        unix::net::{SocketAddr, UnixDatagram},
    },
    ptr::NonNull,
    sync::Mutex,
};

const HEADER: usize = 64;
const MAGIC: &[u8; 8] = b"TARSFRM1";
const MAX_BYTES: usize = 64 * 1024 * 1024;

struct Mapping {
    file: File,
    data: NonNull<u8>,
    len: usize,
    sequence: u64,
}
// Access to this mapping is serialized by SharedFrames::mapping and by flock
// across processes. Readers map read-only and copy before releasing their lock.
unsafe impl Send for Mapping {}
impl Drop for Mapping {
    fn drop(&mut self) {
        unsafe {
            libc::munmap(self.data.as_ptr().cast(), self.len);
        }
    }
}

pub struct SharedFrames {
    mapping: Mutex<Mapping>,
    width: u32,
    height: u32,
    stride: u32,
    source: String,
    notification: UnixDatagram,
    address: SocketAddr,
}

impl SharedFrames {
    pub fn new(width: u32, height: u32) -> Result<Self> {
        let stride = (width as usize)
            .checked_mul(3)
            .and_then(|n| n.checked_add(3))
            .map(|n| n & !3usize)
            .context("invalid shared frame width")?;
        let pixels = stride
            .checked_mul(height as usize)
            .filter(|n| *n > 0 && *n <= MAX_BYTES)
            .context("invalid shared frame dimensions")?;
        let name = CString::new("tarsier-perception")?;
        let fd = unsafe {
            libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING)
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error()).context("create perception memfd");
        }
        let file = unsafe { File::from_raw_fd(fd) };
        file.set_permissions(std::os::unix::fs::PermissionsExt::from_mode(0o600))?;
        let len = HEADER + pixels;
        file.set_len(len as u64)?;
        // A reader must never observe a resized mapping, including during shutdown.
        if unsafe {
            libc::fcntl(
                fd,
                libc::F_ADD_SEALS,
                libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_SEAL,
            )
        } < 0
        {
            return Err(std::io::Error::last_os_error()).context("seal perception buffer size");
        }
        let data = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                0,
            )
        };
        if data == libc::MAP_FAILED {
            return Err(std::io::Error::last_os_error()).context("map perception buffer");
        }
        let mapping = Mapping {
            file,
            data: NonNull::new(data.cast()).unwrap(),
            len,
            sequence: 0,
        };
        let nonce = format!("{:032x}", rand::random::<u128>());
        let source = format!("shm:///proc/{}/fd/{fd}#{nonce}", std::process::id());
        let address = SocketAddr::from_abstract_name(format!("tarsier-{nonce}"))?;
        let notification = UnixDatagram::unbound()?;
        notification.set_nonblocking(true)?;
        let result = Self {
            mapping: Mutex::new(mapping),
            width,
            height,
            stride: stride as u32,
            source,
            notification,
            address,
        };
        result.clear();
        Ok(result)
    }

    pub fn source(&self) -> &str {
        &self.source
    }
    pub fn stride(&self) -> usize {
        self.stride as usize
    }
    pub fn dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    pub fn publish(
        &self,
        pixels: &[u8],
        frame_id: u64,
        captured_at_ms: u64,
        rotation: u16,
    ) -> Result<bool> {
        if pixels.len() != self.stride as usize * self.height as usize {
            bail!("unexpected BGR frame stride or size");
        }
        if ![0, 90, 180, 270].contains(&rotation) {
            bail!("invalid shared frame rotation");
        }
        self.write(Some(pixels), frame_id, captured_at_ms, rotation, false)
    }

    pub fn clear(&self) {
        if let Err(error) = self.write(None, 0, 0, 0, true) {
            tracing::warn!(%error, "failed to invalidate shared perception frame");
        }
    }

    fn write(
        &self,
        pixels: Option<&[u8]>,
        frame_id: u64,
        captured_at_ms: u64,
        rotation: u16,
        wait: bool,
    ) -> Result<bool> {
        let mut map = self.mapping.lock().unwrap();
        if wait {
            FileExt::lock_exclusive(&map.file)?;
        } else if let Err(error) = FileExt::try_lock_exclusive(&map.file) {
            if error.kind() == std::io::ErrorKind::WouldBlock {
                return Ok(false);
            }
            return Err(error.into());
        }
        map.sequence = map.sequence.wrapping_add(1);
        let mut header = [0u8; HEADER];
        header[..8].copy_from_slice(MAGIC);
        for (offset, value) in [
            (8, self.width),
            (12, self.height),
            (16, self.stride),
            (20, rotation as u32),
        ] {
            header[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
        }
        for (offset, value) in [
            (24, map.sequence),
            (32, frame_id),
            (40, captured_at_ms),
            (48, pixels.map_or(0, |p| p.len() as u64)),
        ] {
            header[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
        }
        // The exclusive file lock spans both header and pixels, including clear.
        unsafe {
            if let Some(pixels) = pixels {
                std::ptr::copy_nonoverlapping(
                    pixels.as_ptr(),
                    map.data.as_ptr().add(HEADER),
                    pixels.len(),
                );
            }
            std::ptr::copy_nonoverlapping(header.as_ptr(), map.data.as_ptr(), HEADER);
        }
        FileExt::unlock(&map.file)?;
        // No receiver, a full notification queue, or an exiting worker is normal.
        let _ = self.notification.send_to_addr(&[1], &self.address);
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::FileExt as _;

    #[test]
    fn shared_frames_preserve_bytes_provenance_and_drop_on_reader_contention() {
        let frames = SharedFrames::new(3, 2).unwrap();
        let path = frames
            .source()
            .strip_prefix("shm://")
            .unwrap()
            .split('#')
            .next()
            .unwrap();
        let reader = File::open(path).unwrap();
        let pixels: Vec<u8> = (0..24).collect();
        assert!(frames.publish(&pixels, 7, 123456, 90).unwrap());
        let mut data = [0; HEADER + 24];
        reader.read_exact_at(&mut data, 0).unwrap();
        assert_eq!(&data[..8], MAGIC);
        assert_eq!(u64::from_le_bytes(data[32..40].try_into().unwrap()), 7);
        assert_eq!(u64::from_le_bytes(data[40..48].try_into().unwrap()), 123456);
        assert_eq!(&data[HEADER..], &pixels);
        FileExt::lock_shared(&reader).unwrap();
        assert!(!frames.publish(&pixels, 8, 123457, 0).unwrap());
        FileExt::unlock(&reader).unwrap();
        frames.clear();
        reader.read_exact_at(&mut data, 0).unwrap();
        assert_eq!(&data[32..56], &[0; 24]);
        assert!(frames.publish(&pixels, 1, 123458, 270).unwrap());
        assert!(frames.publish(&pixels[..20], 2, 123459, 0).is_err());
        let source = frames.source().to_owned();
        drop(frames);
        // Other tests may reuse the closed descriptor number immediately.
        // A surviving reader keeps the original inode alive for comparison.
        use std::os::unix::fs::MetadataExt;
        if let Ok(reused) = File::open(
            source.strip_prefix("shm://").unwrap().split('#').next().unwrap(),
        ) {
            let original = reader.metadata().unwrap();
            let current = reused.metadata().unwrap();
            assert_ne!((original.dev(), original.ino()), (current.dev(), current.ino()));
        }
    }
}
