use std::{
    fs::{File, OpenOptions},
    os::{fd::AsRawFd, unix::fs::OpenOptionsExt},
};

use anyhow::{Context, Result};
use nix::libc;

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
    file: File,
    unit: u8,
}

impl LinuxUvcTransport {
    pub fn open(path: &str, unit: u8) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(path)
            .with_context(|| format!("failed to open camera control device {path}"))?;
        Ok(Self { file, unit })
    }

    fn query(&mut self, selector: u8, query: u8, data: &mut [u8]) -> Result<()> {
        let mut request = UvcXuControlQuery {
            unit: self.unit,
            selector,
            query,
            size: data.len().try_into().context("XU buffer is too large")?,
            data: data.as_mut_ptr(),
        };
        // SAFETY: request points to a live writable slice for the duration of the
        // ioctl, and the kernel ABI is defined by linux/uvcvideo.h.
        unsafe { uvc_ctrl_query(self.file.as_raw_fd(), &mut request) }
            .with_context(|| format!("UVC XU query failed for selector {selector}"))?;
        Ok(())
    }
}

impl XuTransport for LinuxUvcTransport {
    fn set(&mut self, selector: u8, data: &mut [u8]) -> Result<()> {
        self.query(selector, UVC_SET_CUR, data)
    }

    fn get(&mut self, selector: u8, data: &mut [u8]) -> Result<()> {
        self.query(selector, UVC_GET_CUR, data)
    }
}
