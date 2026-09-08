//! Hold V4L2 buffer ownership without streaming. An open fd alone is not a lock.
use anyhow::{Context, Result, ensure};

#[repr(C)]
#[derive(Default)]
struct RequestBuffers {
    count: u32,
    buffer_type: u32,
    memory: u32,
    capabilities: u32,
    flags: u8,
    reserved: [u8; 3],
}

nix::ioctl_readwrite!(request_buffers, b'V', 8, RequestBuffers);

// The caller owns the GStreamer source, which must remain in READY throughout
// the reservation. Its fd stays open; no buffers are queued and STREAMON is
// never called here. Current capture supports single-planar MJPEG devices.
pub(crate) fn set_reserved(fd: i32, reserved: bool) -> Result<()> {
    ensure!(fd >= 0, "camera device is not open");
    let mut request = RequestBuffers {
        count: u32::from(reserved),
        buffer_type: 1, // V4L2_BUF_TYPE_VIDEO_CAPTURE
        memory: 1, // V4L2_MEMORY_MMAP
        ..Default::default()
    };
    // SAFETY: fd is owned by the caller and request matches the Linux UAPI.
    unsafe { request_buffers(fd, &mut request) }
        .context("failed to change camera buffer reservation")?;
    ensure!(!reserved || request.count > 0, "driver did not reserve any camera buffers");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_buffers_matches_linux_abi() {
        assert_eq!(std::mem::size_of::<RequestBuffers>(), 20);
        assert!(set_reserved(-1, true).is_err());
    }
}
