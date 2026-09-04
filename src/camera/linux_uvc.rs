use std::{
    fs::{File, OpenOptions},
    os::{fd::AsRawFd, unix::fs::OpenOptionsExt},
};

use anyhow::{Context, Result, bail};
use nix::{errno::Errno, libc};

const UVC_SET_CUR: u8 = 0x01;
const UVC_GET_CUR: u8 = 0x81;
const V4L2_CID_ZOOM_ABSOLUTE: u32 = 0x009a_090d;
const V4L2_CTRL_FLAG_DISABLED: u32 = 0x0000_0001;

#[repr(C)]
struct UvcXuControlQuery {
    unit: u8,
    selector: u8,
    query: u8,
    size: u16,
    data: *mut u8,
}

nix::ioctl_readwrite!(uvc_ctrl_query, b'u', 0x21, UvcXuControlQuery);

#[repr(C)]
struct V4l2Control {
    id: u32,
    value: i32,
}

#[repr(C)]
struct V4l2QueryControl {
    id: u32,
    kind: u32,
    name: [u8; 32],
    minimum: i32,
    maximum: i32,
    step: i32,
    default_value: i32,
    flags: u32,
    reserved: [u32; 2],
}

nix::ioctl_readwrite!(v4l2_query_control, b'V', 36, V4l2QueryControl);
nix::ioctl_readwrite!(v4l2_get_control, b'V', 27, V4l2Control);
nix::ioctl_readwrite!(v4l2_set_control, b'V', 28, V4l2Control);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ZoomControl {
    pub minimum: i32,
    pub maximum: i32,
    pub step: i32,
    pub value: i32,
}

pub trait XuTransport: Send {
    fn set(&mut self, selector: u8, data: &mut [u8]) -> Result<()>;
    fn get(&mut self, selector: u8, data: &mut [u8]) -> Result<()>;
    fn zoom_control(&mut self) -> Result<ZoomControl> {
        bail!("absolute zoom is not supported by this camera transport")
    }
    fn set_zoom_units(&mut self, _units: i32) -> Result<()> {
        bail!("absolute zoom is not supported by this camera transport")
    }
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

    fn raw_zoom_control(&self) -> std::result::Result<ZoomControl, Errno> {
        let mut query = V4l2QueryControl {
            id: V4L2_CID_ZOOM_ABSOLUTE,
            kind: 0,
            name: [0; 32],
            minimum: 0,
            maximum: 0,
            step: 0,
            default_value: 0,
            flags: 0,
            reserved: [0; 2],
        };
        // SAFETY: query is a correctly sized writable v4l2_queryctrl value.
        unsafe { v4l2_query_control(self.file.as_raw_fd(), &mut query) }?;
        if query.flags & V4L2_CTRL_FLAG_DISABLED != 0 {
            return Err(Errno::ENOTTY);
        }

        let mut control = V4l2Control {
            id: V4L2_CID_ZOOM_ABSOLUTE,
            value: 0,
        };
        // SAFETY: control is a correctly sized writable v4l2_control value.
        unsafe { v4l2_get_control(self.file.as_raw_fd(), &mut control) }?;
        Ok(ZoomControl {
            minimum: query.minimum,
            maximum: query.maximum,
            step: query.step,
            value: control.value,
        })
    }

    fn zoom_control(&mut self) -> Result<ZoomControl> {
        match self.raw_zoom_control() {
            Ok(control) => Ok(control),
            Err(error) if is_disconnected(error) => {
                self.reopen().with_context(|| {
                    format!("failed to reopen camera control device {}", self.path)
                })?;
                self.raw_zoom_control()
                    .context("V4L2 absolute zoom query failed after reopening")
            }
            Err(error) => Err(error).context("V4L2 absolute zoom query failed"),
        }
    }

    fn raw_set_zoom_units(&self, units: i32) -> std::result::Result<(), Errno> {
        let mut control = V4l2Control {
            id: V4L2_CID_ZOOM_ABSOLUTE,
            value: units,
        };
        // SAFETY: control is a correctly sized writable v4l2_control value.
        unsafe { v4l2_set_control(self.file.as_raw_fd(), &mut control) }?;
        Ok(())
    }

    fn set_zoom_units(&mut self, units: i32) -> Result<()> {
        match self.raw_set_zoom_units(units) {
            Ok(()) => Ok(()),
            Err(error) if is_disconnected(error) => {
                self.reopen().with_context(|| {
                    format!("failed to reopen camera control device {}", self.path)
                })?;
                self.raw_set_zoom_units(units)
                    .context("V4L2 absolute zoom write failed after reopening")
            }
            Err(error) => Err(error).context("V4L2 absolute zoom write failed"),
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

    fn zoom_control(&mut self) -> Result<ZoomControl> {
        LinuxUvcTransport::zoom_control(self)
    }

    fn set_zoom_units(&mut self, units: i32) -> Result<()> {
        LinuxUvcTransport::set_zoom_units(self, units)
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

    #[test]
    fn v4l2_control_structures_match_the_linux_abi() {
        assert_eq!(std::mem::size_of::<V4l2Control>(), 8);
        assert_eq!(std::mem::size_of::<V4l2QueryControl>(), 68);
    }
}
