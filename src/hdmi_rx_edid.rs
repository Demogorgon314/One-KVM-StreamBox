//! HDMI RX EDID synchronization for StreamBox AML boards.

use std::{io, process::Command};

use tokio::time::{sleep, Duration};
use tracing::{debug, info, warn};

const EDID_WITH_PORT_PATH: &str = "/sys/class/hdmirx/hdmirx0/edid_with_port";
const EDID_SELECT_PATH: &str = "/sys/class/hdmirx/hdmirx0/edid_select";
const HDMI_RX_DEBUG_PATH: &str = "/sys/class/hdmirx/hdmirx0/debug";
const HDMI_EDID_TEMPLATE_DIRS: [&str; 2] = ["/vendor/etc/tvconfig/hdmi", "/etc/tvconfig/hdmi"];

const EDID_BLOCK: usize = 128;
const EDID_SIZE: usize = 256;
const HDMI_RX_PORTS: u8 = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EdidMode {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
}

#[derive(Debug, Clone, Copy)]
struct DetailedTiming {
    pixel_clock_10khz: u32,
    h_active: u32,
    h_blank: u32,
    h_front: u32,
    h_sync: u32,
    v_active: u32,
    v_blank: u32,
    v_front: u32,
    v_sync: u32,
    flags: u8,
}

impl EdidMode {
    pub fn new(width: u32, height: u32, fps: u32) -> Self {
        Self { width, height, fps }
    }

    fn normalized(self) -> Self {
        let fps = match self.fps {
            0..=30 => 60,
            value => value,
        };

        Self {
            width: self.width.clamp(640, 3840),
            height: self.height.clamp(480, 2160),
            fps: fps.clamp(24, 240),
        }
    }
}

pub async fn sync_from_video_config(width: u32, height: u32, fps: u32) {
    let mode = EdidMode::new(width, height, fps).normalized();
    if let Err(err) = apply_mode(mode).await {
        warn!(
            "Failed to sync HDMI RX EDID for {}x{}@{}: {}",
            mode.width, mode.height, mode.fps, err
        );
    }
}

async fn apply_mode(mode: EdidMode) -> io::Result<()> {
    if tokio::fs::metadata(EDID_WITH_PORT_PATH).await.is_err() {
        debug!("HDMI RX EDID sysfs is unavailable, skipping EDID sync");
        return Ok(());
    }

    for port in 1..=HDMI_RX_PORTS {
        let edid = build_edid_for_port(mode, port).await;
        let mut payload = Vec::with_capacity(1 + EDID_SIZE * 2);
        payload.push(port & 0x0f);
        payload.extend_from_slice(&edid);
        payload.extend_from_slice(&edid);
        tokio::fs::write(EDID_WITH_PORT_PATH, payload).await?;
    }

    tokio::fs::write(EDID_SELECT_PATH, b"1111").await?;
    pulse_hpd().await?;
    restart_streambox_tv();
    info!(
        "Synced HDMI RX EDID preferred mode to {}x{}@{}",
        mode.width, mode.height, mode.fps
    );
    Ok(())
}

async fn build_edid_for_port(mode: EdidMode, port: u8) -> [u8; EDID_SIZE] {
    if let Some(mut edid) = read_template_edid(port).await {
        let timing = timing_for_mode(mode);
        write_dtd(&mut edid[54..72], timing);
        update_checksum(&mut edid[0..EDID_BLOCK]);
        debug!(
            "Using HDMI 2.0 template EDID for port {} with preferred mode {}x{}@{}",
            port, mode.width, mode.height, mode.fps
        );
        return edid;
    }

    debug!(
        "No HDMI 2.0 template EDID found for port {}, using generated fallback",
        port
    );
    build_fallback_edid(mode)
}

async fn read_template_edid(port: u8) -> Option<[u8; EDID_SIZE]> {
    for dir in HDMI_EDID_TEMPLATE_DIRS {
        for name in [
            format!("{dir}/port{port}_20.bin"),
            format!("{dir}/port1_20.bin"),
            format!("{dir}/port_20.bin"),
        ] {
            let Ok(data) = tokio::fs::read(&name).await else {
                continue;
            };
            if data.len() < EDID_SIZE {
                continue;
            }

            let mut edid = [0u8; EDID_SIZE];
            edid.copy_from_slice(&data[..EDID_SIZE]);
            return Some(edid);
        }
    }

    None
}

fn restart_streambox_tv() {
    match Command::new("systemctl")
        .args(["try-restart", "streambox-tv.service"])
        .status()
    {
        Ok(status) if status.success() => {
            debug!("Requested streambox-tv restart after HDMI RX EDID sync");
        }
        Ok(status) => {
            debug!(
                "streambox-tv try-restart after HDMI RX EDID sync exited with {}",
                status
            );
        }
        Err(err) => {
            debug!(
                "Unable to try-restart streambox-tv after HDMI RX EDID sync: {}",
                err
            );
        }
    }
}

async fn pulse_hpd() -> io::Result<()> {
    let _ = tokio::fs::write(HDMI_RX_DEBUG_PATH, b"reset4").await;
    tokio::fs::write(HDMI_RX_DEBUG_PATH, b"hpd0").await?;
    sleep(Duration::from_millis(1200)).await;
    tokio::fs::write(HDMI_RX_DEBUG_PATH, b"hpd1").await?;
    sleep(Duration::from_millis(300)).await;
    Ok(())
}

fn build_fallback_edid(mode: EdidMode) -> [u8; EDID_SIZE] {
    let timing = timing_for_mode(mode);
    let mut edid = [0u8; EDID_SIZE];

    edid[0..8].copy_from_slice(&[0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x00]);
    edid[8..10].copy_from_slice(&manufacturer_id("SKV"));
    edid[10..12].copy_from_slice(&0x0001u16.to_le_bytes());
    edid[12..16].copy_from_slice(&0x00000001u32.to_le_bytes());
    edid[16] = 1;
    edid[17] = 36;
    edid[18] = 1;
    edid[19] = 4;
    edid[20] = 0xa5;
    edid[21] = 80;
    edid[22] = 45;
    edid[23] = 0x78;
    edid[24] = 0x0a;
    edid[25] = 0xcf;
    edid[26] = 0x74;
    edid[27] = 0xa3;
    edid[28] = 0x57;
    edid[29] = 0x4c;
    edid[30] = 0xb0;
    edid[31] = 0x23;
    edid[32] = 0x09;
    edid[33] = 0x48;
    edid[34] = 0x4c;
    edid[35] = 0x21;
    edid[36] = 0x08;
    edid[38] = 0x81;
    edid[39] = 0x80;
    edid[40] = 0x81;
    edid[41] = 0xc0;
    edid[42] = 0x95;
    edid[43] = 0x00;
    edid[44] = 0xa9;
    edid[45] = 0x40;
    edid[46] = 0xb3;
    edid[47] = 0x00;
    edid[48] = 0xd1;
    edid[49] = 0xc0;

    write_dtd(&mut edid[54..72], timing);
    write_range_descriptor(&mut edid[72..90], mode);
    write_ascii_descriptor(&mut edid[90..108], 0xfc, "One-KVM");
    write_ascii_descriptor(&mut edid[108..126], 0xfe, "StreamBox");

    edid[126] = 1;
    update_checksum(&mut edid[0..EDID_BLOCK]);

    build_cea_extension(&mut edid[EDID_BLOCK..EDID_SIZE], mode, timing);
    edid
}

fn build_cea_extension(block: &mut [u8], mode: EdidMode, timing: DetailedTiming) {
    block[0] = 0x02;
    block[1] = 0x03;
    block[3] = 0x00;

    let mut video_modes = vec![16, 4, 31, 19, 5, 20];
    if mode.width == 3840 && mode.height == 2160 {
        video_modes.insert(0, if mode.fps >= 60 { 97 } else { 95 });
    } else if mode.width == 1920 && mode.height == 1080 {
        video_modes.insert(0, if mode.fps > 60 { 63 } else { 16 });
    } else if mode.width == 1280 && mode.height == 720 {
        video_modes.insert(0, 4);
    }
    video_modes.dedup();

    block[4] = 0x40 | (video_modes.len() as u8);
    for (idx, vic) in video_modes.iter().enumerate() {
        block[5 + idx] = if idx == 0 { 0x80 | *vic } else { *vic };
    }

    block[2] = (5 + video_modes.len()) as u8;
    let dtd_offset = block[2] as usize;
    write_dtd(&mut block[dtd_offset..dtd_offset + 18], timing);
    update_checksum(block);
}

fn timing_for_mode(mode: EdidMode) -> DetailedTiming {
    match (mode.width, mode.height, nearest_refresh(mode.fps)) {
        (1280, 720, _) => DetailedTiming {
            pixel_clock_10khz: 7425,
            h_active: 1280,
            h_blank: 370,
            h_front: 110,
            h_sync: 40,
            v_active: 720,
            v_blank: 30,
            v_front: 5,
            v_sync: 5,
            flags: 0x1e,
        },
        (1920, 1080, 120) => DetailedTiming {
            pixel_clock_10khz: 29700,
            h_active: 1920,
            h_blank: 280,
            h_front: 88,
            h_sync: 44,
            v_active: 1080,
            v_blank: 45,
            v_front: 4,
            v_sync: 5,
            flags: 0x1e,
        },
        (2560, 1440, refresh) if refresh >= 100 => DetailedTiming {
            pixel_clock_10khz: 48300,
            h_active: 2560,
            h_blank: 160,
            h_front: 48,
            h_sync: 32,
            v_active: 1440,
            v_blank: 41,
            v_front: 3,
            v_sync: 5,
            flags: 0x1e,
        },
        (2560, 1440, _) => DetailedTiming {
            pixel_clock_10khz: 24150,
            h_active: 2560,
            h_blank: 160,
            h_front: 48,
            h_sync: 32,
            v_active: 1440,
            v_blank: 41,
            v_front: 3,
            v_sync: 5,
            flags: 0x1e,
        },
        (3840, 2160, refresh) if refresh >= 50 => DetailedTiming {
            pixel_clock_10khz: 59400,
            h_active: 3840,
            h_blank: 560,
            h_front: 176,
            h_sync: 88,
            v_active: 2160,
            v_blank: 90,
            v_front: 8,
            v_sync: 10,
            flags: 0x1e,
        },
        (3840, 2160, _) => DetailedTiming {
            pixel_clock_10khz: 29700,
            h_active: 3840,
            h_blank: 560,
            h_front: 176,
            h_sync: 88,
            v_active: 2160,
            v_blank: 90,
            v_front: 8,
            v_sync: 10,
            flags: 0x1e,
        },
        (1024, 768, _) => DetailedTiming {
            pixel_clock_10khz: 6500,
            h_active: 1024,
            h_blank: 320,
            h_front: 24,
            h_sync: 136,
            v_active: 768,
            v_blank: 38,
            v_front: 3,
            v_sync: 6,
            flags: 0x18,
        },
        _ => DetailedTiming {
            pixel_clock_10khz: 14850,
            h_active: 1920,
            h_blank: 280,
            h_front: 88,
            h_sync: 44,
            v_active: 1080,
            v_blank: 45,
            v_front: 4,
            v_sync: 5,
            flags: 0x1e,
        },
    }
}

fn nearest_refresh(fps: u32) -> u32 {
    if fps >= 100 {
        120
    } else if fps >= 50 {
        60
    } else {
        30
    }
}

fn write_dtd(dst: &mut [u8], t: DetailedTiming) {
    dst.fill(0);
    dst[0] = (t.pixel_clock_10khz & 0xff) as u8;
    dst[1] = ((t.pixel_clock_10khz >> 8) & 0xff) as u8;
    dst[2] = (t.h_active & 0xff) as u8;
    dst[3] = (t.h_blank & 0xff) as u8;
    dst[4] = (((t.h_active >> 8) & 0x0f) << 4 | ((t.h_blank >> 8) & 0x0f)) as u8;
    dst[5] = (t.v_active & 0xff) as u8;
    dst[6] = (t.v_blank & 0xff) as u8;
    dst[7] = (((t.v_active >> 8) & 0x0f) << 4 | ((t.v_blank >> 8) & 0x0f)) as u8;
    dst[8] = (t.h_front & 0xff) as u8;
    dst[9] = (t.h_sync & 0xff) as u8;
    dst[10] = (((t.v_front & 0x0f) << 4) | (t.v_sync & 0x0f)) as u8;
    dst[11] = ((((t.h_front >> 8) & 0x03) << 6)
        | (((t.h_sync >> 8) & 0x03) << 4)
        | (((t.v_front >> 4) & 0x03) << 2)
        | ((t.v_sync >> 4) & 0x03)) as u8;
    dst[12] = 0x20;
    dst[13] = 0xc2;
    dst[14] = 0x31;
    dst[17] = t.flags;
}

fn write_range_descriptor(dst: &mut [u8], mode: EdidMode) {
    dst.fill(0x20);
    dst[0..5].copy_from_slice(&[0x00, 0x00, 0x00, 0xfd, 0x00]);
    dst[5] = 24;
    dst[6] = mode.fps.min(240) as u8;
    dst[7] = 30;
    dst[8] = 135;
    dst[9] = 0x1e;
    dst[10] = 0x00;
}

fn write_ascii_descriptor(dst: &mut [u8], tag: u8, text: &str) {
    dst.fill(0x20);
    dst[0..5].copy_from_slice(&[0x00, 0x00, 0x00, tag, 0x00]);
    for (idx, byte) in text.bytes().take(12).enumerate() {
        dst[5 + idx] = byte;
    }
    let term = 5 + text.len().min(12);
    if term < 18 {
        dst[term] = b'\n';
    }
}

fn manufacturer_id(code: &str) -> [u8; 2] {
    let mut value = 0u16;
    for ch in code.bytes().take(3) {
        value = (value << 5) | u16::from(ch.to_ascii_uppercase() - b'A' + 1);
    }
    value.to_be_bytes()
}

fn update_checksum(block: &mut [u8]) {
    let checksum_index = block.len() - 1;
    block[checksum_index] = 0;
    let sum = block[..checksum_index]
        .iter()
        .fold(0u8, |acc, value| acc.wrapping_add(*value));
    block[checksum_index] = (0u8).wrapping_sub(sum);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_edid_blocks_have_valid_checksums() {
        let edid = build_fallback_edid(EdidMode::new(1920, 1080, 60));
        assert_eq!(edid[0..128].iter().fold(0u8, |a, b| a.wrapping_add(*b)), 0);
        assert_eq!(
            edid[128..256].iter().fold(0u8, |a, b| a.wrapping_add(*b)),
            0
        );
    }

    #[test]
    fn preferred_mode_is_written_to_base_dtd() {
        let edid = build_fallback_edid(EdidMode::new(2560, 1440, 60));
        assert_eq!(edid[56], 0x00);
        assert_eq!(edid[58] >> 4, 0x0a);
        assert_eq!(edid[59], 0xa0);
        assert_eq!(edid[61] >> 4, 0x05);
    }
}
