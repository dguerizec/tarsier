use std::{
    fs::{File, OpenOptions},
    os::{fd::AsRawFd, unix::fs::OpenOptionsExt},
};

use anyhow::{Context, Result, bail};
use nix::{errno::Errno, libc};

use crate::model::{
    CameraImageControl, CameraImageControlKind, CameraImageControlOption, CameraImageControlState,
    unix_ms,
};

const UVC_SET_CUR: u8 = 0x01;
const UVC_GET_CUR: u8 = 0x81;
const V4L2_CID_ZOOM_ABSOLUTE: u32 = 0x009a_090d;
const V4L2_CID_PAN_SPEED: u32 = 0x009a_0920;
const V4L2_CID_TILT_SPEED: u32 = 0x009a_0921;
const V4L2_CID_BRIGHTNESS: u32 = 0x0098_0900;
const V4L2_CID_CONTRAST: u32 = 0x0098_0901;
const V4L2_CID_SATURATION: u32 = 0x0098_0902;
const V4L2_CID_HUE: u32 = 0x0098_0903;
const V4L2_CID_AUTO_WHITE_BALANCE: u32 = 0x0098_090c;
const V4L2_CID_GAMMA: u32 = 0x0098_0910;
const V4L2_CID_RED_BALANCE: u32 = 0x0098_090e;
const V4L2_CID_BLUE_BALANCE: u32 = 0x0098_090f;
const V4L2_CID_GAIN: u32 = 0x0098_0913;
const V4L2_CID_POWER_LINE_FREQUENCY: u32 = 0x0098_0918;
const V4L2_CID_WHITE_BALANCE_TEMPERATURE: u32 = 0x0098_091a;
const V4L2_CID_SHARPNESS: u32 = 0x0098_091b;
const V4L2_CID_BACKLIGHT_COMPENSATION: u32 = 0x0098_091c;
const V4L2_CID_EXPOSURE_AUTO: u32 = 0x009a_0901;
const V4L2_CID_EXPOSURE_ABSOLUTE: u32 = 0x009a_0902;
const V4L2_CID_EXPOSURE_AUTO_PRIORITY: u32 = 0x009a_0903;
const V4L2_CID_FOCUS_ABSOLUTE: u32 = 0x009a_090a;
const V4L2_CID_FOCUS_AUTO: u32 = 0x009a_090c;
const V4L2_CTRL_CLASS_CAMERA: u32 = 0x009a_0000;
const V4L2_CTRL_FLAG_DISABLED: u32 = 0x0000_0001;
const V4L2_CTRL_FLAG_READ_ONLY: u32 = 0x0000_0004;
const V4L2_CTRL_FLAG_INACTIVE: u32 = 0x0000_0010;

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

#[repr(C)]
union V4l2ExtControlValue {
    value: i32,
    value64: i64,
}

#[repr(C, packed)]
struct V4l2ExtControl {
    id: u32,
    size: u32,
    reserved2: [u32; 1],
    value: V4l2ExtControlValue,
}

#[repr(C)]
struct V4l2ExtControls {
    which: u32,
    count: u32,
    error_idx: u32,
    request_fd: i32,
    reserved: [u32; 1],
    controls: *mut V4l2ExtControl,
}

nix::ioctl_readwrite!(v4l2_query_control, b'V', 36, V4l2QueryControl);
#[repr(C, packed)]
struct V4l2QueryMenu {
    id: u32,
    index: u32,
    name: [u8; 32],
    reserved: u32,
}
nix::ioctl_readwrite!(v4l2_query_menu, b'V', 37, V4l2QueryMenu);
nix::ioctl_readwrite!(v4l2_get_control, b'V', 27, V4l2Control);
nix::ioctl_readwrite!(v4l2_set_control, b'V', 28, V4l2Control);
nix::ioctl_readwrite!(v4l2_set_ext_controls, b'V', 72, V4l2ExtControls);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ZoomControl {
    pub minimum: i32,
    pub maximum: i32,
    pub step: i32,
    pub value: i32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PanTiltSpeedControls {
    pub pan: ZoomControl,
    pub tilt: ZoomControl,
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
    fn pan_tilt_speed_controls(&mut self) -> Result<PanTiltSpeedControls> {
        bail!("pan and tilt speed are not supported by this camera transport")
    }
    fn set_pan_tilt_speed_units(&mut self, _pan: i32, _tilt: i32) -> Result<()> {
        bail!("pan and tilt speed are not supported by this camera transport")
    }
    fn image_controls(&mut self) -> Vec<CameraImageControlState> {
        CameraImageControl::STANDARD
            .into_iter()
            .map(|control| self.image_control(control))
            .collect()
    }
    fn image_control(&mut self, control: CameraImageControl) -> CameraImageControlState {
        unavailable_image_control(control, "image controls are not supported")
    }
    fn set_image_control(
        &mut self,
        _control: CameraImageControl,
        _value: i32,
    ) -> Result<CameraImageControlState> {
        bail!("image controls are not supported by this camera transport")
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

    fn raw_standard_control(&self, id: u32) -> std::result::Result<ZoomControl, Errno> {
        let mut query = V4l2QueryControl {
            id,
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

        let mut control = V4l2Control { id, value: 0 };
        // SAFETY: control is a correctly sized writable v4l2_control value.
        unsafe { v4l2_get_control(self.file.as_raw_fd(), &mut control) }?;
        Ok(ZoomControl {
            minimum: query.minimum,
            maximum: query.maximum,
            step: query.step,
            value: control.value,
        })
    }

    fn raw_image_control(
        &self,
        control: CameraImageControl,
    ) -> std::result::Result<CameraImageControlState, Errno> {
        let id = image_control_id(control).ok_or(Errno::ENOTTY)?;
        let mut query = V4l2QueryControl {
            id,
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
        let mut current = V4l2Control { id, value: 0 };
        // SAFETY: current is a correctly sized writable v4l2_control value.
        unsafe { v4l2_get_control(self.file.as_raw_fd(), &mut current) }?;
        Ok(CameraImageControlState {
            control,
            kind: image_control_kind(control),
            available: true,
            active: query.flags & V4L2_CTRL_FLAG_INACTIVE == 0,
            read_only: query.flags & V4L2_CTRL_FLAG_READ_ONLY != 0,
            value: Some(current.value),
            minimum: Some(query.minimum),
            maximum: Some(query.maximum),
            step: Some(query.step),
            default_value: Some(query.default_value),
            options: image_control_options(control)
                .into_iter()
                .filter(|option| (query.minimum..=query.maximum).contains(&option.value))
                .filter(|option| {
                    let mut menu = V4l2QueryMenu {
                        id,
                        index: option.value as u32,
                        name: [0; 32],
                        reserved: 0,
                    };
                    // SAFETY: menu matches the packed v4l2_querymenu ABI.
                    unsafe { v4l2_query_menu(self.file.as_raw_fd(), &mut menu) }.is_ok()
                })
                .collect(),
            sample_at_ms: Some(unix_ms()),
            error: None,
        })
    }

    fn image_control(&mut self, control: CameraImageControl) -> CameraImageControlState {
        match self.raw_image_control(control) {
            Ok(state) => state,
            Err(error) if is_disconnected(error) => match self.reopen() {
                Ok(()) => self.raw_image_control(control).unwrap_or_else(|error| {
                    unavailable_image_control(
                        control,
                        format!("V4L2 control query failed after reopening: {error}"),
                    )
                }),
                Err(error) => unavailable_image_control(
                    control,
                    format!("failed to reopen camera control device: {error}"),
                ),
            },
            Err(error) => {
                unavailable_image_control(control, format!("V4L2 control query failed: {error}"))
            }
        }
    }

    fn image_controls(&mut self) -> Vec<CameraImageControlState> {
        CameraImageControl::STANDARD
            .into_iter()
            .map(|control| self.image_control(control))
            .collect()
    }

    fn raw_zoom_control(&self) -> std::result::Result<ZoomControl, Errno> {
        self.raw_standard_control(V4L2_CID_ZOOM_ABSOLUTE)
    }

    fn raw_pan_tilt_speed_controls(&self) -> std::result::Result<PanTiltSpeedControls, Errno> {
        Ok(PanTiltSpeedControls {
            pan: self.raw_standard_control(V4L2_CID_PAN_SPEED)?,
            tilt: self.raw_standard_control(V4L2_CID_TILT_SPEED)?,
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

    fn raw_set_image_control(
        &self,
        control: CameraImageControl,
        value: i32,
    ) -> std::result::Result<(), Errno> {
        let mut value = V4l2Control {
            id: image_control_id(control).ok_or(Errno::ENOTTY)?,
            value,
        };
        // SAFETY: value is a correctly sized writable v4l2_control value.
        unsafe { v4l2_set_control(self.file.as_raw_fd(), &mut value) }?;
        Ok(())
    }

    fn set_image_control(
        &mut self,
        control: CameraImageControl,
        value: i32,
    ) -> Result<CameraImageControlState> {
        let before = self.image_control(control);
        validate_image_control_value(&before, value)?;
        match self.raw_set_image_control(control, value) {
            Ok(()) => {}
            Err(error) if is_disconnected(error) => {
                self.reopen().with_context(|| {
                    format!("failed to reopen camera control device {}", self.path)
                })?;
                self.raw_set_image_control(control, value)
                    .context("V4L2 image-control write failed after reopening")?;
            }
            Err(error) => return Err(error).context("V4L2 image-control write failed"),
        }
        let after = self.image_control(control);
        if after.value != Some(value) {
            bail!(
                "camera image-control readback mismatch: requested {value}, got {:?}",
                after.value
            );
        }
        Ok(after)
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

    fn pan_tilt_speed_controls(&mut self) -> Result<PanTiltSpeedControls> {
        match self.raw_pan_tilt_speed_controls() {
            Ok(controls) => Ok(controls),
            Err(error) if is_disconnected(error) => {
                self.reopen().with_context(|| {
                    format!("failed to reopen camera control device {}", self.path)
                })?;
                self.raw_pan_tilt_speed_controls()
                    .context("V4L2 pan/tilt speed query failed after reopening")
            }
            Err(error) => Err(error).context("V4L2 pan/tilt speed query failed"),
        }
    }

    fn raw_set_pan_tilt_speed_units(&self, pan: i32, tilt: i32) -> std::result::Result<(), Errno> {
        let mut controls = [
            V4l2ExtControl {
                id: V4L2_CID_PAN_SPEED,
                size: 0,
                reserved2: [0; 1],
                value: V4l2ExtControlValue { value: pan },
            },
            V4l2ExtControl {
                id: V4L2_CID_TILT_SPEED,
                size: 0,
                reserved2: [0; 1],
                value: V4l2ExtControlValue { value: tilt },
            },
        ];
        let mut request = V4l2ExtControls {
            which: V4L2_CTRL_CLASS_CAMERA,
            count: controls.len() as u32,
            error_idx: 0,
            request_fd: 0,
            reserved: [0; 1],
            controls: controls.as_mut_ptr(),
        };
        // SAFETY: request and its control array match the V4L2 extended-control ABI
        // and remain live and writable for the duration of the ioctl.
        unsafe { v4l2_set_ext_controls(self.file.as_raw_fd(), &mut request) }?;
        Ok(())
    }

    fn set_pan_tilt_speed_units(&mut self, pan: i32, tilt: i32) -> Result<()> {
        match self.raw_set_pan_tilt_speed_units(pan, tilt) {
            Ok(()) => Ok(()),
            Err(error) if is_disconnected(error) => {
                self.reopen().with_context(|| {
                    format!("failed to reopen camera control device {}", self.path)
                })?;
                self.raw_set_pan_tilt_speed_units(pan, tilt)
                    .context("V4L2 pan/tilt speed write failed after reopening")
            }
            Err(error) => Err(error).context("V4L2 pan/tilt speed write failed"),
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

    fn pan_tilt_speed_controls(&mut self) -> Result<PanTiltSpeedControls> {
        LinuxUvcTransport::pan_tilt_speed_controls(self)
    }

    fn set_pan_tilt_speed_units(&mut self, pan: i32, tilt: i32) -> Result<()> {
        LinuxUvcTransport::set_pan_tilt_speed_units(self, pan, tilt)
    }

    fn image_controls(&mut self) -> Vec<CameraImageControlState> {
        LinuxUvcTransport::image_controls(self)
    }

    fn image_control(&mut self, control: CameraImageControl) -> CameraImageControlState {
        LinuxUvcTransport::image_control(self, control)
    }

    fn set_image_control(
        &mut self,
        control: CameraImageControl,
        value: i32,
    ) -> Result<CameraImageControlState> {
        LinuxUvcTransport::set_image_control(self, control, value)
    }
}

fn image_control_id(control: CameraImageControl) -> Option<u32> {
    Some(match control {
        CameraImageControl::Brightness => V4L2_CID_BRIGHTNESS,
        CameraImageControl::Contrast => V4L2_CID_CONTRAST,
        CameraImageControl::Saturation => V4L2_CID_SATURATION,
        CameraImageControl::Hue => V4L2_CID_HUE,
        CameraImageControl::Gamma => V4L2_CID_GAMMA,
        CameraImageControl::Gain => V4L2_CID_GAIN,
        CameraImageControl::BacklightCompensation => V4L2_CID_BACKLIGHT_COMPENSATION,
        CameraImageControl::PowerLineFrequency => V4L2_CID_POWER_LINE_FREQUENCY,
        CameraImageControl::WhiteBalanceAutomatic => V4L2_CID_AUTO_WHITE_BALANCE,
        CameraImageControl::WhiteBalanceTemperature => V4L2_CID_WHITE_BALANCE_TEMPERATURE,
        CameraImageControl::RedBalance => V4L2_CID_RED_BALANCE,
        CameraImageControl::BlueBalance => V4L2_CID_BLUE_BALANCE,
        CameraImageControl::Sharpness => V4L2_CID_SHARPNESS,
        CameraImageControl::AutoExposure => V4L2_CID_EXPOSURE_AUTO,
        CameraImageControl::ExposureTimeAbsolute => V4L2_CID_EXPOSURE_ABSOLUTE,
        CameraImageControl::ExposureDynamicFramerate => V4L2_CID_EXPOSURE_AUTO_PRIORITY,
        CameraImageControl::FocusAbsolute => V4L2_CID_FOCUS_ABSOLUTE,
        CameraImageControl::FocusAutomaticContinuous => V4L2_CID_FOCUS_AUTO,
        CameraImageControl::FacePriorityAutoExposure => return None,
    })
}

pub(crate) fn image_control_kind(control: CameraImageControl) -> CameraImageControlKind {
    match control {
        CameraImageControl::PowerLineFrequency | CameraImageControl::AutoExposure => {
            CameraImageControlKind::Menu
        }
        CameraImageControl::WhiteBalanceAutomatic
        | CameraImageControl::ExposureDynamicFramerate
        | CameraImageControl::FocusAutomaticContinuous
        | CameraImageControl::FacePriorityAutoExposure => CameraImageControlKind::Boolean,
        _ => CameraImageControlKind::Integer,
    }
}

pub(crate) fn image_control_options(control: CameraImageControl) -> Vec<CameraImageControlOption> {
    let options: &[(i32, &str)] = match control {
        CameraImageControl::PowerLineFrequency => {
            &[(0, "Disabled"), (1, "50 Hz"), (2, "60 Hz"), (3, "Auto")]
        }
        CameraImageControl::AutoExposure => &[(0, "Auto"), (1, "Manual"), (3, "Aperture priority")],
        _ => &[],
    };
    options
        .iter()
        .map(|(value, label)| CameraImageControlOption {
            value: *value,
            label: (*label).to_owned(),
        })
        .collect()
}

pub(crate) fn unavailable_image_control(
    control: CameraImageControl,
    error: impl Into<String>,
) -> CameraImageControlState {
    CameraImageControlState {
        control,
        kind: image_control_kind(control),
        available: false,
        active: false,
        read_only: false,
        value: None,
        minimum: None,
        maximum: None,
        step: None,
        default_value: None,
        options: image_control_options(control),
        sample_at_ms: Some(unix_ms()),
        error: Some(error.into()),
    }
}

pub(crate) fn validate_image_control_value(
    state: &CameraImageControlState,
    value: i32,
) -> Result<()> {
    if !state.available {
        bail!("camera image control is unavailable");
    }
    if !state.active {
        bail!("camera image control is inactive in the current mode");
    }
    if state.read_only {
        bail!("camera image control is read-only");
    }
    let (Some(minimum), Some(maximum), Some(step)) = (state.minimum, state.maximum, state.step)
    else {
        bail!("camera image-control range is unavailable");
    };
    if value < minimum || value > maximum || step <= 0 || (value - minimum) % step != 0 {
        bail!(
            "camera image-control value must be between {minimum} and {maximum} in steps of {step}"
        );
    }
    if state.kind == CameraImageControlKind::Menu
        && !state.options.iter().any(|option| option.value == value)
    {
        bail!("camera image-control menu value is unsupported");
    }
    Ok(())
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
        assert_eq!(std::mem::size_of::<V4l2ExtControl>(), 20);
        assert_eq!(std::mem::size_of::<V4l2ExtControls>(), 32);
    }

    #[test]
    fn every_standard_image_control_has_a_v4l2_identifier() {
        for control in CameraImageControl::STANDARD {
            assert!(image_control_id(control).is_some(), "missing {control:?}");
        }
        assert!(image_control_id(CameraImageControl::FacePriorityAutoExposure).is_none());
    }

    #[test]
    fn image_control_validation_rejects_inactive_and_sparse_menu_values() {
        let mut state = CameraImageControlState {
            control: CameraImageControl::AutoExposure,
            kind: CameraImageControlKind::Menu,
            available: true,
            active: true,
            read_only: false,
            value: Some(0),
            minimum: Some(0),
            maximum: Some(3),
            step: Some(1),
            default_value: Some(0),
            options: image_control_options(CameraImageControl::AutoExposure),
            sample_at_ms: Some(0),
            error: None,
        };
        assert!(validate_image_control_value(&state, 1).is_ok());
        assert!(validate_image_control_value(&state, 2).is_err());
        state.active = false;
        assert!(validate_image_control_value(&state, 1).is_err());
    }
}
