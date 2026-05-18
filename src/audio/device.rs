use alsa::pcm::HwParams;
use alsa::{Direction, PCM};
use serde::Serialize;
use tracing::{debug, info, warn};

use crate::error::{AppError, Result};

#[derive(Debug, Clone, Serialize)]
pub struct AudioDeviceInfo {
    pub name: String,
    pub description: String,
    pub card_index: i32,
    pub device_index: i32,
    pub sample_rates: Vec<u32>,
    pub channels: Vec<u32>,
    pub is_capture: bool,
    pub is_hdmi: bool,
    pub usb_bus: Option<String>,
}

fn get_usb_bus_info(card_index: i32) -> Option<String> {
    if card_index < 0 {
        return None;
    }

    let device_path = format!("/sys/class/sound/card{}/device", card_index);
    let link_target = std::fs::read_link(&device_path).ok()?;
    let link_str = link_target.to_string_lossy();

    for component in link_str.split('/') {
        if component.contains('-') && !component.contains(':') {
            if component
                .chars()
                .next()
                .map(|c| c.is_ascii_digit())
                .unwrap_or(false)
            {
                return Some(component.to_string());
            }
        }
    }

    None
}

fn get_pcm_description(card_index: i32, device_index: i32) -> Option<String> {
    let pcm_list = std::fs::read_to_string("/proc/asound/pcm").ok()?;
    let prefix = format!("{:02}-{:02}:", card_index, device_index);

    pcm_list
        .lines()
        .find_map(|line| line.strip_prefix(&prefix))
        .map(str::trim)
        .and_then(|rest| rest.split(" : ").next())
        .map(str::trim)
        .filter(|desc| !desc.is_empty())
        .map(str::to_string)
}

fn is_aml_audio_card(card_longname: &str) -> bool {
    let card_lower = card_longname.to_lowercase();
    card_lower.contains("aml-augesound") || card_lower.contains("amlogic")
}

fn is_hdmi_rx_description(description: &str) -> bool {
    let desc_lower = description.to_lowercase();
    desc_lower.contains("hdmirx")
        || desc_lower.contains("hdmi-rx")
        || desc_lower.contains("i2s4hdmirx")
}

fn is_hdmi_capture_device(card_longname: &str, pcm_description: Option<&str>) -> bool {
    let card_lower = card_longname.to_lowercase();
    let pcm_lower = pcm_description.unwrap_or_default().to_lowercase();

    is_hdmi_rx_description(&pcm_lower)
        || pcm_lower.contains("usb")
        || (pcm_lower.contains("hdmi") && !pcm_lower.contains("spdif"))
        || card_lower.contains("usb")
}

fn is_hdmi_rx_device(device: &AudioDeviceInfo) -> bool {
    is_hdmi_rx_description(&device.description)
        || device.description.to_lowercase().contains("hdmi in audio")
}

fn should_expose_capture_device(
    card_longname: &str,
    pcm_description: Option<&str>,
    usb_bus: Option<&str>,
) -> bool {
    if usb_bus.is_some() {
        return true;
    }

    if pcm_description.map(is_hdmi_rx_description).unwrap_or(false) {
        return true;
    }

    // AML-AUGESOUND exposes several SoC-internal capture endpoints (SPDIF, PDM,
    // TDM, HDMI loopback). They are not useful as KVM input sources and confuse
    // the setup UI, so only the real HDMI RX endpoint is shown.
    !is_aml_audio_card(card_longname)
}

fn friendly_audio_description(
    card_longname: &str,
    device_name: &str,
    pcm_description: Option<&str>,
    usb_bus: Option<&str>,
) -> String {
    if pcm_description.map(is_hdmi_rx_description).unwrap_or(false) {
        return format!("HDMI IN Audio ({})", device_name);
    }

    if let Some(bus) = usb_bus {
        return format!("USB Audio Capture {} ({})", bus, device_name);
    }

    if let Some(pcm_description) = pcm_description {
        return format!("{} - {} ({})", card_longname, pcm_description, device_name);
    }

    format!("{} ({})", card_longname, device_name)
}

pub fn enumerate_audio_devices() -> Result<Vec<AudioDeviceInfo>> {
    enumerate_audio_devices_with_current(None)
}

pub fn enumerate_audio_devices_with_current(
    current_device: Option<&str>,
) -> Result<Vec<AudioDeviceInfo>> {
    let mut devices = Vec::new();

    let cards = alsa::card::Iter::new();

    for card_result in cards {
        let card = match card_result {
            Ok(c) => c,
            Err(e) => {
                debug!("Error iterating card: {}", e);
                continue;
            }
        };

        let card_index = card.get_index();
        let card_name = card.get_name().unwrap_or_else(|_| "Unknown".to_string());
        let card_longname = card.get_longname().unwrap_or_else(|_| card_name.clone());

        debug!("Found audio card {}: {}", card_index, card_longname);

        let usb_bus = get_usb_bus_info(card_index);

        for device_index in 0..=8 {
            let device_name = format!("hw:{},{}", card_index, device_index);
            let is_current_device = current_device == Some(device_name.as_str());
            let pcm_description = get_pcm_description(card_index, device_index);
            if !is_current_device
                && !should_expose_capture_device(
                    &card_longname,
                    pcm_description.as_deref(),
                    usb_bus.as_deref(),
                )
            {
                debug!(
                    "Skipping non-KVM audio endpoint {}: {}",
                    device_name,
                    pcm_description.as_deref().unwrap_or("unknown")
                );
                continue;
            }

            let is_hdmi = is_hdmi_capture_device(&card_longname, pcm_description.as_deref());
            let description = friendly_audio_description(
                &card_longname,
                &device_name,
                pcm_description.as_deref(),
                usb_bus.as_deref(),
            );

            let mut push_info =
                |sample_rates: Vec<u32>, channels: Vec<u32>, description: String| {
                    devices.push(AudioDeviceInfo {
                        name: device_name.clone(),
                        description,
                        card_index,
                        device_index,
                        sample_rates,
                        channels,
                        is_capture: true,
                        is_hdmi,
                        usb_bus: usb_bus.clone(),
                    });
                };

            match PCM::new(&device_name, Direction::Capture, false) {
                Ok(pcm) => {
                    let (sample_rates, channels) = query_device_caps(&pcm);

                    if !sample_rates.is_empty() && !channels.is_empty() {
                        push_info(sample_rates, channels, description.clone());
                    }
                }
                Err(_) => {
                    if is_current_device {
                        debug!(
                            "Device {} is busy (in use by us), adding with default caps",
                            device_name
                        );
                        push_info(
                            vec![44100, 48000],
                            vec![2],
                            format!("{} (in use)", description),
                        );
                    }
                }
            }
        }
    }

    info!("Found {} audio capture devices", devices.len());
    Ok(devices)
}

fn query_device_caps(pcm: &PCM) -> (Vec<u32>, Vec<u32>) {
    let hwp = match HwParams::any(pcm) {
        Ok(h) => h,
        Err(_) => return (vec![], vec![]),
    };

    let common_rates = [8000, 16000, 22050, 44100, 48000, 96000];
    let mut supported_rates = Vec::new();

    for rate in &common_rates {
        if hwp.test_rate(*rate).is_ok() {
            supported_rates.push(*rate);
        }
    }

    let mut supported_channels = Vec::new();
    for ch in 1..=8 {
        if hwp.test_channels(ch).is_ok() {
            supported_channels.push(ch);
        }
    }

    (supported_rates, supported_channels)
}

pub fn find_best_audio_device() -> Result<AudioDeviceInfo> {
    let devices = enumerate_audio_devices()?;

    if devices.is_empty() {
        return Err(AppError::AudioError(
            "No audio capture devices found".to_string(),
        ));
    }

    let mut first_48k_stereo: Option<&AudioDeviceInfo> = None;
    let mut first_hdmi_stereo: Option<&AudioDeviceInfo> = None;
    for device in &devices {
        if !device.sample_rates.contains(&48000) || !device.channels.contains(&2) {
            continue;
        }
        if is_hdmi_rx_device(device) {
            info!("Selected HDMI RX audio device: {}", device.description);
            return Ok(device.clone());
        }
        if device.is_hdmi {
            first_hdmi_stereo.get_or_insert(device);
            continue;
        }
        if first_48k_stereo.is_none() {
            first_48k_stereo = Some(device);
        }
    }
    if let Some(device) = first_hdmi_stereo {
        info!("Selected HDMI audio device: {}", device.description);
        return Ok(device.clone());
    }
    if let Some(device) = first_48k_stereo {
        info!("Selected audio device: {}", device.description);
        return Ok(device.clone());
    }

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
    fn test_hdmi_rx_description_is_friendly() {
        let description = friendly_audio_description(
            "AML-AUGESOUND",
            "hw:0,0",
            Some("TDM-B-dummy-alsaPORT-i2s-i2s4hdmirx soc:dummy-0"),
            None,
        );

        assert_eq!(description, "HDMI IN Audio (hw:0,0)");
    }

    #[test]
    fn test_aml_internal_endpoint_is_hidden() {
        assert!(!should_expose_capture_device(
            "AML-AUGESOUND",
            Some("SPDIF-dummy-alsaPORT-spdif soc:dummy-1"),
            None,
        ));
    }

    #[test]
    fn test_usb_capture_endpoint_is_visible() {
        assert!(should_expose_capture_device(
            "USB Audio",
            Some("USB Audio Capture"),
            Some("1-1"),
        ));
    }

    #[test]
    fn test_enumerate_devices() {
        let result = enumerate_audio_devices();
        println!("Audio devices: {:?}", result);
        assert!(result.is_ok());
    }
}
