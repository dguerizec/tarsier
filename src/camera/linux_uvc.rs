use std::{
    fs::{File, OpenOptions},
    os::{fd::AsRawFd, unix::fs::OpenOptionsExt},
};

use anyhow::{Context, Result};
use nix::{errno::Errno, libc};

const UVC_SET_CUR: u8 = 0x01;
const UVC_GET_CUR: u8 = 0x81;

#[repr(C)]
struct UvcXuControlQuery {
    unit: u8,
    selector: u8,
    query: u8,
    size: u16,
    data: *mut u8,
}

nix::ioctl_readwrite!(uvc_ctrl_query, b'u', 0x21, UvcXuControlQuery);

pub trait XuTransport: Send {
    fn set(&mut self, selector: u8, data: &mut [u8]) -> Result<()>;
    fn get(&mut self, selector: u8, data: &mut [u8]) -> Result<()>;
}

pub struct LinuxUvcTransport {
    path: String,
    file: File,
    unit: u8,
}

impl LinuxUvcTransport {
    pub fn open(path: &str, unit: u8) -> Result<Self> {
        let file = open_device(path)?;
        Ok(Self {
            path: path.to_owned(),
            file,
            unit,
        })
    }

    fn reopen(&mut self) -> Result<()> {
        self.file = open_device(&self.path)?;
        Ok(())
    }

    fn raw_query(
        &self,
        selector: u8,
        query: u8,
        data: &mut [u8],
    ) -> std::result::Result<(), Errno> {
        let mut request = UvcXuControlQuery {
            unit: self.unit,
            selector,
            query,
            size: data.len().try_into().map_err(|_| Errno::EOVERFLOW)?,
            data: data.as_mut_ptr(),
        };
        // SAFETY: request points to a live writable slice for the duration of the
        // ioctl, and the kernel ABI is defined by linux/uvcvideo.h.
        unsafe { uvc_ctrl_query(self.file.as_raw_fd(), &mut request) }?;
        Ok(())
    }

    fn query(&mut self, selector: u8, query: u8, data: &mut [u8]) -> Result<()> {
        match self.raw_query(selector, query, data) {
            Ok(()) => Ok(()),
            Err(error) if is_disconnected(error) => {
                self.reopen().with_context(|| {
                    format!("failed to reopen camera control device {}", self.path)
                })?;
                self.raw_query(selector, query, data).with_context(|| {
                    format!("UVC XU query failed after reopening for selector {selector}")
                })
            }
            Err(error) => {
                Err(error).with_context(|| format!("UVC XU query failed for selector {selector}"))
            }
        }
    }
}

fn open_device(path: &str) -> Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)
        .with_context(|| format!("failed to open camera control device {path}"))
}

fn is_disconnected(error: Errno) -> bool {
    matches!(error, Errno::ENODEV | Errno::EBADF | Errno::ENXIO)
}

impl XuTransport for LinuxUvcTransport {
    fn set(&mut self, selector: u8, data: &mut [u8]) -> Result<()> {
        self.query(selector, UVC_SET_CUR, data)
    }

    fn get(&mut self, selector: u8, data: &mut [u8]) -> Result<()> {
        self.query(selector, UVC_GET_CUR, data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_definite_disconnect_errors_are_retried() {
        assert!(is_disconnected(Errno::ENODEV));
        assert!(is_disconnected(Errno::EBADF));
        assert!(is_disconnected(Errno::ENXIO));
        assert!(!is_disconnected(Errno::EIO));
        assert!(!is_disconnected(Errno::EINVAL));
    }
}
