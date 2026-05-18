use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use super::format::{PixelFormat, Resolution};
use crate::error::{AppError, Result};

const AML_DEVICE_PATH: &str = "/dev/video_cap";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VideoDeviceInfo {
    pub path: PathBuf,
    pub name: String,
    pub driver: String,
    pub bus_info: String,
    pub card: String,
    pub formats: Vec<FormatInfo>,
    pub capabilities: DeviceCapabilities,
    pub is_capture_card: bool,
    pub priority: u32,
    pub has_signal: bool,
    pub subdev_path: Option<PathBuf>,
    pub bridge_kind: Option<String>,
}

#[derive(Debug, Clone)]
pub struct VideoDeviceRecoveryHint {
    pub path: PathBuf,
    pub name: String,
    pub driver: String,
    pub bus_info: String,
    pub card: String,
    pub is_capture_card: bool,
}

impl From<&VideoDeviceInfo> for VideoDeviceRecoveryHint {
    fn from(device: &VideoDeviceInfo) -> Self {
        Self {
            path: device.path.clone(),
            name: device.name.clone(),
            driver: device.driver.clone(),
            bus_info: device.bus_info.clone(),
            card: device.card.clone(),
            is_capture_card: device.is_capture_card,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FormatInfo {
    pub format: PixelFormat,
    pub resolutions: Vec<ResolutionInfo>,
    pub description: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResolutionInfo {
    pub width: u32,
    pub height: u32,
    pub fps: Vec<f64>,
}

impl ResolutionInfo {
    pub fn new(width: u32, height: u32, fps: Vec<f64>) -> Self {
        Self { width, height, fps }
    }

    pub fn resolution(&self) -> Resolution {
        Resolution::new(self.width, self.height)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DeviceCapabilities {
    pub video_capture: bool,
    pub video_capture_mplane: bool,
    pub video_output: bool,
    pub streaming: bool,
    pub read_write: bool,
}

pub struct VideoDevice {
    pub path: PathBuf,
}

impl VideoDevice {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Self::open_readonly(path)
    }

    pub fn open_readonly(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        if !path.exists() {
            return Err(AppError::VideoError(format!(
                "Video device not found: {}",
                path.display()
            )));
        }
        Ok(Self { path })
    }

    pub fn info(&self) -> Result<VideoDeviceInfo> {
        aml_video_device_info(self.path.clone())
    }
}

fn aml_video_device_info(path: PathBuf) -> Result<VideoDeviceInfo> {
    if !path.exists() {
        return Err(AppError::VideoError(format!(
            "AML capture device not found: {}",
            path.display()
        )));
    }

    Ok(build_aml_video_device_info(path))
}

fn build_aml_video_device_info(path: PathBuf) -> VideoDeviceInfo {
    VideoDeviceInfo {
        path,
        name: "Amlogic StreamBox Capture".to_string(),
        driver: "aml_vfmcap".to_string(),
        bus_info: "platform:vfmcap".to_string(),
        card: "Amlogic HDMI RX".to_string(),
        formats: vec![FormatInfo {
            format: PixelFormat::Nv12,
            resolutions: vec![
                ResolutionInfo::new(1920, 1080, vec![30.0, 60.0, 120.0, 240.0]),
                ResolutionInfo::new(3840, 2160, vec![30.0, 60.0, 120.0, 240.0]),
            ],
            description: "AML VFM Capture NV12".to_string(),
        }],
        capabilities: DeviceCapabilities {
            video_capture: true,
            video_capture_mplane: false,
            video_output: false,
            streaming: true,
            read_write: false,
        },
        is_capture_card: true,
        priority: 1000,
        has_signal: true,
        subdev_path: None,
        bridge_kind: None,
    }
}

pub fn enumerate_devices() -> Result<Vec<VideoDeviceInfo>> {
    Ok(if Path::new(AML_DEVICE_PATH).exists() {
        vec![aml_video_device_info(PathBuf::from(AML_DEVICE_PATH))?]
    } else {
        Vec::new()
    })
}

pub fn find_best_device() -> Result<VideoDeviceInfo> {
    enumerate_devices()?
        .into_iter()
        .next()
        .ok_or_else(|| AppError::VideoError("No AML capture device available".to_string()))
}

pub fn select_recovery_device(
    devices: &[VideoDeviceInfo],
    hint: &VideoDeviceRecoveryHint,
) -> Option<VideoDeviceInfo> {
    devices
        .iter()
        .find(|device| device.path == hint.path)
        .or_else(|| devices.iter().find(|device| device.driver == hint.driver))
        .cloned()
}

#[cfg(test)]
mod tests {
    use super::{aml_video_device_info, build_aml_video_device_info, ResolutionInfo};
    use crate::video::format::{PixelFormat, Resolution};
    use tempfile::tempdir;

    #[test]
    fn resolution_info_exposes_resolution() {
        let info = ResolutionInfo::new(1920, 1080, vec![30.0, 60.0]);

        assert_eq!(info.resolution(), Resolution::new(1920, 1080));
    }

    #[test]
    fn builds_expected_aml_device_metadata() {
        let info = build_aml_video_device_info("/dev/video_cap".into());

        assert_eq!(info.name, "Amlogic StreamBox Capture");
        assert_eq!(info.driver, "aml_vfmcap");
        assert_eq!(info.bus_info, "platform:vfmcap");
        assert!(info.capabilities.video_capture);
        assert!(info.capabilities.streaming);
        assert_eq!(info.formats.len(), 1);
        assert_eq!(info.formats[0].format, PixelFormat::Nv12);
        assert_eq!(info.formats[0].resolutions.len(), 2);
        assert_eq!(info.formats[0].resolutions[0].fps, vec![30.0, 60.0]);
    }

    #[test]
    fn aml_device_info_requires_existing_device_path() {
        let temp = tempdir().unwrap();
        let missing = temp.path().join("missing-video-cap");

        let err = aml_video_device_info(missing.clone()).unwrap_err();

        assert!(err.to_string().contains(&missing.display().to_string()));
    }
}
