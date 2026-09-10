//! Stable camera inventory without opening capture devices.
use serde::Serialize;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, Serialize)]
pub struct CameraDevice {
    pub id: String,
    pub name: String,
    pub motorized: bool,
}

pub fn cameras() -> std::io::Result<Vec<CameraDevice>> {
    inventory(Path::new("/sys/class/video4linux"), Path::new("/dev"))
}

fn inventory(sys: &Path, dev: &Path) -> std::io::Result<Vec<CameraDevice>> {
    let mut stable = BTreeMap::<PathBuf, PathBuf>::new();
    // Prefer hardware identity; use physical USB port identity as a fallback.
    for directory in ["v4l/by-id", "v4l/by-path"] {
        if let Ok(entries) = std::fs::read_dir(dev.join(directory)) {
            for entry in entries.flatten() {
                if let Ok(target) = entry.path().canonicalize() {
                    stable.entry(target).or_insert(entry.path());
                }
            }
        }
    }
    let entries = match std::fs::read_dir(sys) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut result = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        // UVC index 1 is generally metadata, not the primary image interface.
        if std::fs::read_to_string(path.join("index"))
            .unwrap_or_default()
            .trim()
            != "0"
        {
            continue;
        }
        // Virtual loopback nodes must not become camera inputs (feedback loop).
        if !path.join("device").exists() {
            continue;
        }
        let device = dev.join(entry.file_name());
        let target = device.canonicalize().unwrap_or_else(|_| device.clone());
        let id = stable
            .get(&target)
            .unwrap_or(&device)
            .to_string_lossy()
            .into_owned();
        let name = std::fs::read_to_string(path.join("name"))?
            .trim()
            .to_owned();
        let motorized = name.to_ascii_lowercase().contains("obsbot tiny 2");
        result.push(CameraDevice {
            id,
            name,
            motorized,
        });
    }
    result.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(result)
}

pub fn apply_camera(config: &mut crate::config::Config, id: &str) {
    use crate::config::{CameraAdapter, VideoSource};
    if id.starts_with("file://") {
        config.video.source = VideoSource::File;
        config.video.input_device = id.into();
        config.camera.adapter = CameraAdapter::Mock;
    } else if id.is_empty() {
        config.video.source = VideoSource::Test;
        config.camera.adapter = CameraAdapter::Mock;
    } else {
        config.video.source = VideoSource::Camera;
        config.video.input_device = id.into();
        config.camera.control_device = id.into();
        // Only the supported OBSBOT model receives vendor-specific commands.
        // Other physical cameras still expose their standard V4L2 image controls.
        config.camera.adapter = if cameras()
            .unwrap_or_default()
            .iter()
            .any(|c| c.id == id && c.motorized)
        {
            CameraAdapter::ObsbotTiny2
        } else {
            CameraAdapter::V4l2
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn file_source_uses_mock_camera_controls_and_retains_its_uri() {
        let mut config = crate::config::Config::default();
        apply_camera(&mut config, "file:///tmp/clip%20with%20spaces.mp4");
        assert_eq!(config.video.source, crate::config::VideoSource::File);
        assert_eq!(config.camera.adapter, crate::config::CameraAdapter::Mock);
        assert_eq!(config.video.input_device, "file:///tmp/clip%20with%20spaces.mp4");
        config.validate().unwrap();
        config.video.input_device = "https://example.com/clip.mp4".into();
        assert!(config.validate().is_err());
        apply_camera(&mut config, "");
        assert_eq!(config.video.source, crate::config::VideoSource::Test);
    }

    #[test]
    fn stable_identity_survives_renumbering_and_excludes_metadata_and_loopback() {
        let root = std::env::temp_dir().join(format!(
            "tarsier-devices-{}-{}",
            std::process::id(),
            crate::model::unix_ms()
        ));
        let sys = root.join("sys");
        let dev = root.join("dev");
        std::fs::create_dir_all(dev.join("v4l/by-id")).unwrap();
        for (name, index, physical) in [
            ("video1", "0", true),
            ("video2", "1", true),
            ("video42", "0", false),
        ] {
            let path = sys.join(name);
            std::fs::create_dir_all(&path).unwrap();
            if physical {
                std::fs::create_dir(path.join("device")).unwrap();
            }
            std::fs::write(path.join("index"), index).unwrap();
            std::fs::write(path.join("name"), "OBSBOT Tiny 2").unwrap();
            std::fs::write(dev.join(name), "").unwrap();
        }
        let id = dev.join("v4l/by-id/usb-camera-video-index0");
        symlink(dev.join("video1"), &id).unwrap();
        let first = inventory(&sys, &dev).unwrap();
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].id, id.to_str().unwrap());
        assert!(first[0].motorized);
        std::fs::rename(sys.join("video1"), sys.join("video9")).unwrap();
        std::fs::rename(dev.join("video1"), dev.join("video9")).unwrap();
        std::fs::remove_file(&id).unwrap();
        symlink(dev.join("video9"), &id).unwrap();
        assert_eq!(inventory(&sys, &dev).unwrap()[0].id, first[0].id);
        std::fs::remove_dir_all(root).unwrap();
    }
}
