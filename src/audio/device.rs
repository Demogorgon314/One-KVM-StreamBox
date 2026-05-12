//! Audio device discovery for the Amlogic audio routes.

use serde::Serialize;
use tracing::{info, warn};

use crate::error::{AppError, Result};

const HDMI_DEVICE_ID: &str = "hdmi";
const LINE_IN_DEVICE_ID: &str = "line_in";
const HDMI_HDMITX_CONNECTED_ALSA: &str = "hw:0,6";
const HDMI_HDMITX_DISCONNECTED_ALSA: &str = "hw:0,2";
const LINE_IN_ALSA: &str = "hw:0,0";

const HDMITX_HPD_STATE: &str = "/sys/class/amhdmitx/amhdmitx0/hpd_state";
const HDMITX_EXTCON_STATE: &str = "/sys/class/extcon/extcon0/state";
const HDMITX_DRM_STATUS: &str = "/sys/class/drm/card0-HDMI-A-1/status";

/// Audio device information
#[derive(Debug, Clone, Serialize)]
pub struct AudioDeviceInfo {
    /// Logical device name (e.g., "hdmi" or "line_in")
    pub name: String,
    /// Human-readable description
    pub description: String,
    /// Card index
    pub card_index: i32,
    /// Device index
    pub device_index: i32,
    /// Supported sample rates
    pub sample_rates: Vec<u32>,
    /// Supported channel counts
    pub channels: Vec<u32>,
    /// Is this a capture device
    pub is_capture: bool,
    /// Is this an HDMI audio device (likely from capture card)
    pub is_hdmi: bool,
    /// USB bus info for matching with video devices (e.g., "1-1" from USB path)
    pub usb_bus: Option<String>,
}

impl AudioDeviceInfo {
    /// Get ALSA device name
    pub fn alsa_name(&self) -> String {
        format!("hw:{},{}", self.card_index, self.device_index)
    }
}

fn read_trimmed(path: &str) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|value| value.trim().to_string())
}

fn hdmitx_connected() -> bool {
    if let Some(value) = read_trimmed(HDMITX_HPD_STATE) {
        match value.as_str() {
            "1" => return true,
            "0" => return false,
            _ => {}
        }
    }

    if let Some(value) = read_trimmed(HDMITX_EXTCON_STATE) {
        if value.lines().any(|line| line.trim() == "HDMI=1") {
            return true;
        }
        if value.lines().any(|line| line.trim() == "HDMI=0") {
            return false;
        }
    }

    read_trimmed(HDMITX_DRM_STATUS)
        .map(|value| value.eq_ignore_ascii_case("connected"))
        .unwrap_or(false)
}

fn hdmi_alsa_device() -> &'static str {
    if hdmitx_connected() {
        HDMI_HDMITX_CONNECTED_ALSA
    } else {
        HDMI_HDMITX_DISCONNECTED_ALSA
    }
}

fn normalized_device_id(device: &str) -> String {
    device.trim().to_ascii_lowercase().replace(['-', ' '], "_")
}

/// Normalize an audio selection to the logical device IDs exposed by discovery.
pub fn normalize_audio_device_selection(device: &str) -> String {
    match normalized_device_id(device).as_str() {
        "" | "default" | "hdmi" | "hdmitx" | "hw0:2" | "hw0:6" | "hw:0,2" | "hw:0,6" => {
            HDMI_DEVICE_ID.to_string()
        }
        "line_in" | "linein" | "hw0:0" | "hw:0,0" => LINE_IN_DEVICE_ID.to_string(),
        _ => device.trim().to_string(),
    }
}

/// Resolve a logical audio selection to the ALSA device opened by capture.
pub fn resolve_audio_device_name(device: &str) -> String {
    match normalize_audio_device_selection(device).as_str() {
        HDMI_DEVICE_ID => hdmi_alsa_device().to_string(),
        LINE_IN_DEVICE_ID => LINE_IN_ALSA.to_string(),
        _ => device.trim().to_string(),
    }
}

fn hdmi_device_index() -> i32 {
    if hdmitx_connected() {
        6
    } else {
        2
    }
}

/// Enumerate available audio capture devices
pub fn enumerate_audio_devices() -> Result<Vec<AudioDeviceInfo>> {
    enumerate_audio_devices_with_current(None)
}

/// Return fixed audio capture choices without opening ALSA devices.
pub fn enumerate_audio_devices_with_current(
    _current_device: Option<&str>,
) -> Result<Vec<AudioDeviceInfo>> {
    let hdmi_device_index = hdmi_device_index();
    let hdmi_alsa = hdmi_alsa_device();
    let devices = vec![
        AudioDeviceInfo {
            name: HDMI_DEVICE_ID.to_string(),
            description: "HDMI".to_string(),
            card_index: 0,
            device_index: hdmi_device_index,
            sample_rates: vec![48000],
            channels: vec![2],
            is_capture: true,
            is_hdmi: true,
            usb_bus: None,
        },
        AudioDeviceInfo {
            name: LINE_IN_DEVICE_ID.to_string(),
            description: "Line In".to_string(),
            card_index: 0,
            device_index: 0,
            sample_rates: vec![48000],
            channels: vec![2],
            is_capture: true,
            is_hdmi: false,
            usb_bus: None,
        },
    ];

    info!(
        "Using fixed audio capture devices: HDMI -> {}, Line In -> {}",
        hdmi_alsa, LINE_IN_ALSA
    );
    Ok(devices)
}

/// Find the best audio device for capture
/// Prefers HDMI/capture devices over built-in microphones
pub fn find_best_audio_device() -> Result<AudioDeviceInfo> {
    let devices = enumerate_audio_devices()?;

    if devices.is_empty() {
        return Err(AppError::AudioError(
            "No audio capture devices found".to_string(),
        ));
    }

    // First, look for HDMI/capture card devices that support 48kHz stereo
    for device in &devices {
        if device.is_hdmi && device.sample_rates.contains(&48000) && device.channels.contains(&2) {
            info!("Selected HDMI audio device: {}", device.description);
            return Ok(device.clone());
        }
    }

    // Then look for any device supporting 48kHz stereo
    for device in &devices {
        if device.sample_rates.contains(&48000) && device.channels.contains(&2) {
            info!("Selected audio device: {}", device.description);
            return Ok(device.clone());
        }
    }

    // Fall back to first device
    let device = devices.into_iter().next().unwrap();
    warn!(
        "Using fallback audio device: {} (may not support optimal settings)",
        device.description
    );
    Ok(device)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_enumerate_devices() {
        // This test may not find devices in CI environment
        let result = enumerate_audio_devices();
        println!("Audio devices: {:?}", result);
        // Just verify it doesn't panic
        assert!(result.is_ok());
    }
}
