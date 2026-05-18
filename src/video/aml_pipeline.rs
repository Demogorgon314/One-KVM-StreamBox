use bytes::Bytes;
use parking_lot::RwLock as ParkingRwLock;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::watch;
use tracing::{error, info, trace, warn};

use crate::error::{AppError, Result};
use crate::ffi::multienc::VlImgFormat;
use crate::ffi::vfmcap::{VfmcapColorMode, VfmcapOutputFmt, VfmcapSignalInfo};
use crate::utils::LogThrottler;
use crate::video::capture_trait::{CaptureStream, DmaBufFrame, FrameData};
use crate::video::encoder::aml_venc::AmlVencEncoder;
use crate::video::encoder::registry::VideoEncoderType;
use crate::video::format::Resolution;
use crate::video::vfmcap_capture::AmlVfmcapCaptureStream;

use super::EncodedVideoFrame;

const AUTO_STOP_GRACE_PERIOD_SECS: u64 = 3;
const CAPTURE_TIMEOUT_STOP_THRESHOLD: u32 = 60;
const CAPTURE_VULKAN_ERROR_STOP_THRESHOLD: u32 = 5;
const ENCODE_ERROR_THROTTLE_SECS: u64 = 5;
const VFMCAP_BUFFER_COUNT: u32 = 12;
const SIGNAL_INFO_SYSFS: &str = "/sys/class/video4linux/video0/signal_info";
const HDMIRX_INFO_SYSFS: &str = "/sys/class/hdmirx/hdmirx0/info";

#[derive(Debug, Clone)]
struct SignalMetadata {
    width: u32,
    height: u32,
    bitdepth: u32,
    signal_type: u32,
    hdr_status: u32,
    hdr_eotf: String,
}

struct CaptureConfig {
    capture: AmlVfmcapCaptureStream,
    resolution: Resolution,
    fps: u32,
    img_format: VlImgFormat,
    signal: SignalMetadata,
}

#[derive(Debug, Clone, Copy)]
struct EncodeTask {
    index: u32,
    dmabuf_fd: i32,
    dmabuf_fd2: i32,
    width: u32,
    height: u32,
    bytesperline: u32,
    size: u32,
    format: u32,
}

fn sysfs_read_resolution() -> Option<(u32, u32)> {
    let s = std::fs::read_to_string(SIGNAL_INFO_SYSFS).ok()?;
    let mut w: u32 = 0;
    let mut h: u32 = 0;
    for line in s.lines() {
        let line = line.trim();
        if let Some(val) = line.strip_prefix("width:") {
            w = val.trim().parse().ok()?;
        } else if let Some(val) = line.strip_prefix("height:") {
            h = val.trim().parse().ok()?;
        }
    }
    if w > 0 && h > 0 {
        Some((w, h))
    } else {
        None
    }
}

pub fn sysfs_current_resolution() -> Option<Resolution> {
    sysfs_read_resolution().map(|(width, height)| Resolution::new(width, height))
}

fn sysfs_read_signal() -> SignalMetadata {
    let mut width: u32 = 0;
    let mut height: u32 = 0;
    let mut bitdepth: u32 = 8;
    let mut signal_type: u32 = 0;
    let mut hdr_status: u32 = 0;
    let mut hdr_eotf = String::from("unknown");

    if let Ok(s) = std::fs::read_to_string(SIGNAL_INFO_SYSFS) {
        for line in s.lines() {
            let line = line.trim();
            if let Some(val) = line.strip_prefix("width:") {
                width = val.trim().parse().unwrap_or(width);
            } else if let Some(val) = line.strip_prefix("height:") {
                height = val.trim().parse().unwrap_or(height);
            } else if let Some(val) = line.strip_prefix("bitdepth:") {
                bitdepth = val.trim().parse().unwrap_or(8);
            } else if let Some(val) = line.strip_prefix("signal_type:") {
                signal_type =
                    u32::from_str_radix(val.trim().trim_start_matches("0x"), 16).unwrap_or(0);
            }
        }
    }

    if let Ok(s) = std::fs::read_to_string(HDMIRX_INFO_SYSFS) {
        for line in s.lines() {
            let line = line.trim();
            if let Some(val) = line.strip_prefix("Color Depth:") {
                bitdepth = val.trim().parse().unwrap_or(bitdepth);
            } else if let Some(val) = line.strip_prefix("Hactive:") {
                width = val.trim().parse().unwrap_or(width);
            } else if let Some(val) = line.strip_prefix("Vactive:") {
                height = val.trim().parse().unwrap_or(height);
            } else if let Some(val) = line.strip_prefix("HDR EOTF:") {
                hdr_eotf = val.trim().to_string();
            }
        }

        let eotf = hdr_eotf.to_ascii_uppercase();
        hdr_status = if eotf.contains("HLG") {
            2
        } else if eotf.contains("HDR10+") || eotf.contains("HDR10PLUS") {
            3
        } else if eotf.contains("2084")
            || eotf.contains("2048")
            || eotf.contains("HDR10")
            || eotf.contains("PQ")
        {
            1
        } else {
            0
        };
    }

    SignalMetadata {
        width,
        height,
        bitdepth,
        signal_type,
        hdr_status,
        hdr_eotf,
    }
}

fn signal_metadata_changed(initial: &SignalMetadata, current: &SignalMetadata) -> bool {
    current.hdr_status != initial.hdr_status
        || current.bitdepth != initial.bitdepth
        || current.signal_type != initial.signal_type
        || (current.width > 0
            && current.height > 0
            && initial.width > 0
            && initial.height > 0
            && (current.width != initial.width || current.height != initial.height))
}

fn info_signal_change(initial: &SignalMetadata, current: &SignalMetadata) {
    info!(
        "AML pipeline: HDMI signal changed, stopping for rebuild ({}x{} -> {}x{}, hdr {}->{}, bitdepth {}->{}, signal_type {:#x}->{:#x}, eotf {}->{})",
        initial.width,
        initial.height,
        current.width,
        current.height,
        initial.hdr_status,
        current.hdr_status,
        initial.bitdepth,
        current.bitdepth,
        initial.signal_type,
        current.signal_type,
        initial.hdr_eotf,
        current.hdr_eotf
    );
}

fn resolve_capture_params(
    hdr_mode: crate::config::HdrMode,
    signal_info: &VfmcapSignalInfo,
) -> (VfmcapColorMode, VfmcapOutputFmt) {
    let is_hdr = signal_info.hdr_status != 0;
    match hdr_mode {
        crate::config::HdrMode::Auto => {
            if is_hdr {
                (detect_hdr_color_mode(signal_info), VfmcapOutputFmt::Nv12)
            } else {
                (VfmcapColorMode::Passthrough, VfmcapOutputFmt::Nv12)
            }
        }
        crate::config::HdrMode::SdrOnly => {
            let color_mode = detect_hdr_color_mode(signal_info);
            (color_mode, VfmcapOutputFmt::Nv12)
        }
        crate::config::HdrMode::Passthrough => {
            if is_hdr {
                (VfmcapColorMode::Passthrough, VfmcapOutputFmt::P010)
            } else {
                (VfmcapColorMode::Passthrough, VfmcapOutputFmt::Nv12)
            }
        }
    }
}

fn open_capture_for_resolution(
    config: &AmlPipelineConfig,
) -> std::result::Result<CaptureConfig, AppError> {
    let signal = sysfs_read_signal();
    let fake_signal_info = VfmcapSignalInfo {
        width: 0,
        height: 0,
        fps: 0,
        pixelformat: 0,
        signal_type: signal.signal_type,
        hdr_status: signal.hdr_status,
        is_interlaced: 0,
        status: 0,
        bitdepth: signal.bitdepth,
    };

    let (color_mode, output_fmt) = resolve_capture_params(config.hdr_mode, &fake_signal_info);
    let img_format = if output_fmt == VfmcapOutputFmt::P010 {
        VlImgFormat::P010
    } else {
        VlImgFormat::Nv12
    };

    info!(
        "AML pipeline: hdr_mode={:?}, hdmi_eotf={} bitdepth={} hdr_status={} signal_type={:#x} -> color_mode={:?} output={:?} img_format={:?}",
        config.hdr_mode, signal.hdr_eotf, signal.bitdepth, signal.hdr_status, signal.signal_type,
        color_mode, output_fmt, img_format
    );

    let (source_w, source_h) = sysfs_read_resolution().unwrap_or((0, 0));
    let should_scale = source_w > 0
        && source_h > 0
        && config.max_width > 0
        && config.max_height > 0
        && (source_w > config.max_width || source_h > config.max_height);
    let target_w = if should_scale { config.max_width } else { 0 };
    let target_h = if should_scale { config.max_height } else { 0 };

    let mut capture = AmlVfmcapCaptureStream::open(
        &config.device,
        output_fmt,
        target_w,
        target_h,
        config.max_fps,
        color_mode,
        VFMCAP_BUFFER_COUNT,
    )?;

    let resolution = capture.resolution();
    let fps = match capture.signal_info() {
        Ok(info) if info.fps > 0 => {
            let raw_fps = if info.fps > 1000 {
                info.fps / 1000
            } else {
                info.fps
            };
            if config.max_fps > 0.0 {
                raw_fps.min(config.max_fps as u32)
            } else {
                raw_fps
            }
        }
        _ => {
            if config.max_fps > 0.0 {
                config.max_fps as u32
            } else {
                30
            }
        }
    };

    info!(
        "AML pipeline: capture opened {}x{} @ {}fps img_format={:?}",
        resolution.width, resolution.height, fps, img_format
    );

    Ok(CaptureConfig {
        capture,
        resolution,
        fps,
        img_format,
        signal,
    })
}

fn create_encoder(
    config: &AmlPipelineConfig,
    width: i32,
    height: i32,
    fps: i32,
    img_format: VlImgFormat,
) -> std::result::Result<AmlVencEncoder, AppError> {
    match config.output_codec {
        VideoEncoderType::H265 => AmlVencEncoder::new_h265(
            width,
            height,
            fps,
            config.bitrate_kbps as i32,
            config.gop,
            config.gop_pattern,
            config.rc_mode,
            img_format,
        ),
        VideoEncoderType::H264 => AmlVencEncoder::new_h264(
            width,
            height,
            fps,
            config.bitrate_kbps as i32,
            config.gop,
            config.gop_pattern,
            config.rc_mode,
            img_format,
        ),
        _ => Err(AppError::VideoError(
            "AML pipeline only supports H264/H265".to_string(),
        )),
    }
}

pub struct AmlPipelineConfig {
    pub max_width: u32,
    pub max_height: u32,
    pub max_fps: f32,
    pub hdr_mode: crate::config::HdrMode,
    pub output_codec: VideoEncoderType,
    pub bitrate_kbps: u32,
    pub gop: i32,
    pub gop_pattern: i32,
    pub rc_mode: i32,
    pub device: String,
}

impl Default for AmlPipelineConfig {
    fn default() -> Self {
        Self {
            max_width: 3840,
            max_height: 2160,
            max_fps: 240.0,
            hdr_mode: crate::config::HdrMode::Auto,
            output_codec: VideoEncoderType::H265,
            bitrate_kbps: 8000,
            gop: 30,
            gop_pattern: 5,
            rc_mode: 1,
            device: "/dev/video_cap".to_string(),
        }
    }
}

pub struct AmlPipeline {
    config: AmlPipelineConfig,
    running_rx: watch::Receiver<bool>,
    running: watch::Sender<bool>,
    running_flag: AtomicBool,
    thread_done: AtomicBool,
    sequence: AtomicU64,
    pipeline_start_time_ms: AtomicI64,
    keyframe_requested: AtomicBool,
    latest_subscribers:
        Arc<ParkingRwLock<Vec<tokio::sync::mpsc::UnboundedSender<Arc<EncodedVideoFrame>>>>>,
    stats: Arc<tokio::sync::Mutex<PipelineStats>>,
}

#[derive(Default)]
struct PipelineStats {
    current_fps: f32,
}

impl AmlPipeline {
    pub fn new(config: AmlPipelineConfig) -> Self {
        let (running_tx, running_rx) = watch::channel(false);
        Self {
            config,
            running_rx,
            running: running_tx,
            running_flag: AtomicBool::new(false),
            thread_done: AtomicBool::new(false),
            sequence: AtomicU64::new(0),
            pipeline_start_time_ms: AtomicI64::new(0),
            keyframe_requested: AtomicBool::new(false),
            latest_subscribers: Arc::new(ParkingRwLock::new(Vec::new())),
            stats: Arc::new(tokio::sync::Mutex::new(PipelineStats::default())),
        }
    }

    pub fn is_running(&self) -> bool {
        self.running_flag.load(Ordering::Acquire)
    }

    pub fn subscribe(&self) -> watch::Receiver<bool> {
        self.running_rx.clone()
    }

    pub fn request_keyframe(&self) {
        self.keyframe_requested.store(true, Ordering::Release);
    }

    pub fn add_subscriber(&self, tx: tokio::sync::mpsc::UnboundedSender<Arc<EncodedVideoFrame>>) {
        self.latest_subscribers.write().push(tx);
    }

    fn subscriber_count(&self) -> usize {
        self.latest_subscribers.read().len()
    }

    fn broadcast_encoded(&self, frame: Arc<EncodedVideoFrame>) {
        let mut subs = self.latest_subscribers.write();
        subs.retain(|tx| tx.send(frame.clone()).is_ok());
    }

    pub async fn start(self: &Arc<Self>) -> Result<()> {
        if *self.running_rx.borrow() {
            warn!("AML pipeline already running");
            return Ok(());
        }

        let _ = self.running.send(true);
        self.running_flag.store(true, Ordering::Release);
        self.sequence.store(0, Ordering::Relaxed);
        self.pipeline_start_time_ms.store(0, Ordering::Relaxed);

        self.thread_done.store(false, Ordering::Release);

        let pipeline = self.clone();

        std::thread::spawn(move || {
            pipeline.clone().run_capture_encode_loop();
            pipeline.thread_done.store(true, Ordering::Release);
        });

        Ok(())
    }

    pub fn stop(&self) {
        if *self.running_rx.borrow() {
            let _ = self.running.send(false);
            self.running_flag.store(false, Ordering::Release);
            info!("Stopping AML pipeline");
        }
    }

    pub fn wait_for_stop(&self, timeout: Duration) -> bool {
        let start = std::time::Instant::now();
        while start.elapsed() < timeout {
            if self.thread_done.load(Ordering::Acquire) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        false
    }

    fn run_encode_thread(
        pipeline: Arc<AmlPipeline>,
        mut encoder: AmlVencEncoder,
        frame_rx: std::sync::mpsc::Receiver<EncodeTask>,
        release_tx: std::sync::mpsc::Sender<u32>,
        encode_active: Arc<AtomicBool>,
        codec: VideoEncoderType,
        fps: u32,
    ) {
        let encode_error_throttler = LogThrottler::with_secs(ENCODE_ERROR_THROTTLE_SECS);

        while pipeline.running_flag.load(Ordering::Acquire) && encode_active.load(Ordering::Acquire)
        {
            if pipeline.keyframe_requested.swap(false, Ordering::AcqRel) {
                encoder.request_keyframe();
            }

            let task = match frame_rx.recv_timeout(Duration::from_millis(100)) {
                Ok(task) => task,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            };

            if !pipeline.running_flag.load(Ordering::Acquire)
                || !encode_active.load(Ordering::Acquire)
            {
                let _ = release_tx.send(task.index);
                while let Ok(queued) = frame_rx.try_recv() {
                    let _ = release_tx.send(queued.index);
                }
                break;
            }

            let num_planes = if task.dmabuf_fd2 >= 0 { 2 } else { 1 };
            let stride = match encoder.img_format() {
                VlImgFormat::P010 => encoder.width() * 2,
                _ => encoder.width(),
            };

            match encoder.encode_dma(task.dmabuf_fd, task.dmabuf_fd2, num_planes, stride) {
                Ok(encoded) => {
                    if !encoded.is_delay {
                        let sequence = pipeline.sequence.fetch_add(1, Ordering::Relaxed) + 1;
                        let pts_ms = encoded.pts_us as i64 / 1000;

                        let frame = Arc::new(EncodedVideoFrame {
                            data: Bytes::from(encoded.data),
                            pts_ms,
                            is_keyframe: encoded.is_keyframe,
                            sequence,
                            duration: Duration::from_millis(1000 / fps as u64),
                            codec,
                        });

                        if sequence <= 5 || sequence % 60 == 0 {
                            info!(
                                "AML pipeline: encoded frame (seq={}, key={}, size={})",
                                sequence,
                                frame.is_keyframe,
                                frame.data.len()
                            );
                        }
                        pipeline.broadcast_encoded(frame);
                    }
                }
                Err(e) => {
                    if encode_error_throttler.should_log("aml_encode") {
                        error!(
                            "AML encode error for {}x{} bpl={} size={} fmt={:#x}: {}",
                            task.width, task.height, task.bytesperline, task.size, task.format, e
                        );
                    }
                }
            }

            let _ = release_tx.send(task.index);
        }
    }

    fn run_capture_encode_loop(self: Arc<Self>) {
        let config = &self.config;

        'outer: while self.running_flag.load(Ordering::Acquire) {
            let CaptureConfig {
                mut capture,
                resolution: actual_resolution,
                fps: actual_fps,
                img_format,
                signal: initial_signal,
            } = match open_capture_for_resolution(config) {
                Ok(capture_config) => capture_config,
                Err(e) => {
                    error!("Failed to open vfmcap: {}", e);
                    break 'outer;
                }
            };

            let mut encoder = match create_encoder(
                config,
                actual_resolution.width as i32,
                actual_resolution.height as i32,
                actual_fps as i32,
                img_format,
            ) {
                Ok(e) => e,
                Err(e) => {
                    error!("Failed to create AML encoder: {}", e);
                    break 'outer;
                }
            };

            info!(
                "AML pipeline: encoder created {}x{} @ {}fps codec={:?} img_format={:?}",
                actual_resolution.width,
                actual_resolution.height,
                actual_fps,
                config.output_codec,
                img_format
            );
            let encoder_width = encoder.width();
            let encoder_height = encoder.height();

            match encoder.generate_header() {
                Ok(header) => {
                    info!("AML pipeline: generated header ({} bytes)", header.len());
                    let header_frame = Arc::new(EncodedVideoFrame {
                        data: Bytes::from(header),
                        pts_ms: 0,
                        is_keyframe: true,
                        sequence: self.sequence.fetch_add(1, Ordering::Relaxed),
                        duration: Duration::from_millis(0),
                        codec: config.output_codec,
                    });
                    self.broadcast_encoded(header_frame);
                }
                Err(e) => {
                    warn!("Failed to generate encoder header: {}", e);
                }
            }

            let mut consecutive_timeouts: u32 = 0;
            let mut consecutive_vulkan_errors: u32 = 0;
            let mut frame_count: u64 = 0;
            let mut fps_frame_count: u64 = 0;
            let mut last_fps_time = Instant::now();
            let mut last_signal_check = Instant::now();
            let mut should_recreate = false;

            let (frame_tx, frame_rx) = std::sync::mpsc::sync_channel::<EncodeTask>(8);
            let (release_tx, release_rx) = std::sync::mpsc::channel::<u32>();
            let encode_active = Arc::new(AtomicBool::new(true));
            let mut pending_releases: HashMap<u32, DmaBufFrame> = HashMap::new();
            let encode_handle = {
                let pipeline = self.clone();
                let encode_active = encode_active.clone();
                let codec = config.output_codec;
                std::thread::spawn(move || {
                    Self::run_encode_thread(
                        pipeline,
                        encoder,
                        frame_rx,
                        release_tx,
                        encode_active,
                        codec,
                        actual_fps,
                    );
                })
            };

            let mut last_subscriber_log = Instant::now();
            while self.running_flag.load(Ordering::Acquire) {
                while let Ok(index) = release_rx.try_recv() {
                    if let Some(dma) = pending_releases.remove(&index) {
                        capture.release_frame(&FrameData::DmaBuf(dma));
                    }
                }

                let sub_count = self.subscriber_count();
                let has_subscribers = sub_count > 0;
                if !has_subscribers && last_subscriber_log.elapsed() >= Duration::from_secs(5) {
                    info!("AML pipeline: no subscribers, capturing but not encoding");
                    last_subscriber_log = Instant::now();
                }

                let event = capture.poll_event(0);
                if event == crate::ffi::vfmcap::VFMCAP_EVENT_SOURCE_CHANGE {
                    info!("AML pipeline: source change event detected");
                    should_recreate = true;
                    break;
                } else if event == crate::ffi::vfmcap::VFMCAP_EVENT_NOSIG {
                    info!("AML pipeline: signal lost/unstable event detected");
                    should_recreate = true;
                    break;
                }

                if last_signal_check.elapsed() >= Duration::from_millis(250) {
                    last_signal_check = Instant::now();
                    let current_signal = sysfs_read_signal();
                    if signal_metadata_changed(&initial_signal, &current_signal) {
                        info_signal_change(&initial_signal, &current_signal);
                        should_recreate = true;
                        break;
                    }
                }

                let capture_result = match capture.next_frame() {
                    Ok(r) => {
                        consecutive_timeouts = 0;
                        consecutive_vulkan_errors = 0;
                        r
                    }
                    Err(e) => {
                        if e.kind() == std::io::ErrorKind::TimedOut {
                            consecutive_timeouts += 1;
                            if consecutive_timeouts >= CAPTURE_TIMEOUT_STOP_THRESHOLD {
                                warn!(
                                    "AML capture timed out {} consecutive times, stopping",
                                    consecutive_timeouts
                                );
                                break;
                            }
                        } else if e.kind() == std::io::ErrorKind::NotConnected {
                            info!("AML pipeline: signal lost during capture, rebuilding after stable signal");
                            should_recreate = true;
                            break;
                        } else if e.to_string().contains("Vulkan")
                            || e.to_string()
                                .contains("vfmcap_acquire_frame failed (rc=-5)")
                        {
                            consecutive_vulkan_errors += 1;
                            if consecutive_vulkan_errors >= CAPTURE_VULKAN_ERROR_STOP_THRESHOLD {
                                warn!(
                                    "AML capture hit {} consecutive vfmcap/Vulkan errors, rebuilding after stable signal: {}",
                                    consecutive_vulkan_errors, e
                                );
                                should_recreate = true;
                                break;
                            }
                            error!("AML capture error: {}", e);
                        } else {
                            error!("AML capture error: {}", e);
                        }
                        continue;
                    }
                };

                if frame_count == 0 {
                    if let FrameData::DmaBuf(dma) = &capture_result.frame {
                        info!(
                            "AML pipeline: first frame dma fd={} fd2={} w={} h={} bpl={} size={} fmt={:#x} enc={}x{}",
                            dma.dmabuf_fd, dma.dmabuf_fd2, dma.width, dma.height,
                            dma.bytesperline, dma.size, dma.format,
                            encoder_width, encoder_height
                        );
                    }
                }

                if last_signal_check.elapsed() >= Duration::from_millis(500) {
                    last_signal_check = Instant::now();
                    let current_signal = sysfs_read_signal();
                    if signal_metadata_changed(&initial_signal, &current_signal) {
                        info_signal_change(&initial_signal, &current_signal);
                        capture.release_frame(&capture_result.frame);
                        should_recreate = true;
                        break;
                    }
                }

                if capture_result.reconfigured {
                    let new_res = capture_result.resolution;
                    info!(
                        "AML pipeline: source reconfigured to {}x{}, rebuilding capture+encoder",
                        new_res.width, new_res.height
                    );
                    capture.release_frame(&capture_result.frame);
                    should_recreate = true;
                    break;
                }

                if has_subscribers {
                    match capture_result.frame {
                        FrameData::DmaBuf(dma) => {
                            let task = EncodeTask {
                                index: dma.index,
                                dmabuf_fd: dma.dmabuf_fd,
                                dmabuf_fd2: dma.dmabuf_fd2,
                                width: dma.width,
                                height: dma.height,
                                bytesperline: dma.bytesperline,
                                size: dma.size,
                                format: dma.format,
                            };
                            pending_releases.insert(task.index, dma);
                            if let Err(e) = frame_tx.send(task) {
                                warn!("AML encode thread stopped while queueing frame: {}", e);
                                if let Some(dma) = pending_releases.remove(&task.index) {
                                    capture.release_frame(&FrameData::DmaBuf(dma));
                                }
                                break;
                            }
                            frame_count += 1;
                            fps_frame_count += 1;
                        }
                        FrameData::Mapped { .. } => {
                            error!("AML pipeline received Mapped frame, expected DmaBuf");
                            continue;
                        }
                    }
                } else {
                    std::thread::sleep(Duration::from_millis(33));
                    capture.release_frame(&capture_result.frame);
                }

                let elapsed = last_fps_time.elapsed();
                if elapsed >= Duration::from_secs(1) {
                    let current_fps = fps_frame_count as f32 / elapsed.as_secs_f32();
                    fps_frame_count = 0;
                    last_fps_time = Instant::now();

                    let mut s = self.stats.blocking_lock();
                    s.current_fps = current_fps;
                }
            }

            encode_active.store(false, Ordering::Release);
            drop(frame_tx);
            let _ = encode_handle.join();

            while let Ok(index) = release_rx.try_recv() {
                if let Some(dma) = pending_releases.remove(&index) {
                    capture.release_frame(&FrameData::DmaBuf(dma));
                }
            }
            for (_, dma) in pending_releases.drain() {
                capture.release_frame(&FrameData::DmaBuf(dma));
            }

            drop(capture);
            info!(
                "AML pipeline: capture closed (Vulkan resources released), encoded {} frames",
                frame_count
            );

            if !should_recreate {
                break 'outer;
            }

            let target_res = match wait_for_stable_signal_sysfs(config, &self.running_flag) {
                Some(res) => res,
                None => {
                    if self.running_flag.load(Ordering::Acquire) {
                        error!("AML pipeline: signal never stabilized after reconfiguration");
                    }
                    break 'outer;
                }
            };

            info!(
                "AML pipeline: rebuilding for {}x{}",
                target_res.width, target_res.height
            );
        }

        self.running_flag.store(false, Ordering::Release);
        let _ = self.running.send(false);
        info!("AML pipeline stopped");
    }
}

fn wait_for_stable_signal_sysfs(
    config: &AmlPipelineConfig,
    running_flag: &AtomicBool,
) -> Option<Resolution> {
    const STABILITY_CHECK_COUNT: u32 = 3;
    const STABILITY_CHECK_INTERVAL_MS: u64 = 500;
    const MAX_WAIT_SECS: u64 = 30;

    let start = Instant::now();
    let mut stable_count: u32 = 0;
    let mut last_w: u32 = 0;
    let mut last_h: u32 = 0;

    info!(
        "AML pipeline: waiting for stable HDMI signal via sysfs (up to {}s, need {} identical readings)",
        MAX_WAIT_SECS, STABILITY_CHECK_COUNT
    );

    loop {
        if !running_flag.load(Ordering::Acquire) {
            info!("AML pipeline: stopping during signal stability wait");
            return None;
        }

        if start.elapsed().as_secs() >= MAX_WAIT_SECS {
            warn!(
                "AML pipeline: signal stability timed out after {}s",
                MAX_WAIT_SECS
            );
            return None;
        }

        match sysfs_read_resolution() {
            Some((w, h)) => {
                if w == last_w && h == last_h {
                    stable_count += 1;
                    info!(
                        "AML pipeline: signal stable {}/{} ({}x{})",
                        stable_count, STABILITY_CHECK_COUNT, w, h
                    );
                } else {
                    if last_w > 0 {
                        info!(
                            "AML pipeline: signal changed {}x{} -> {}x{}, resetting stability counter",
                            last_w, last_h, w, h
                        );
                    }
                    last_w = w;
                    last_h = h;
                    stable_count = 1;
                }

                if stable_count >= STABILITY_CHECK_COUNT {
                    let target_w = if config.max_width > 0 && last_w > config.max_width {
                        config.max_width
                    } else {
                        last_w
                    };
                    let target_h = if config.max_height > 0 && last_h > config.max_height {
                        config.max_height
                    } else {
                        last_h
                    };
                    info!(
                        "AML pipeline: signal stable at {}x{} (target {}x{}) after {:.1}s",
                        last_w,
                        last_h,
                        target_w,
                        target_h,
                        start.elapsed().as_secs_f32()
                    );
                    return Some(Resolution::new(target_w, target_h));
                }
            }
            None => {
                trace!("AML pipeline: sysfs signal_info not available or zero resolution");
                stable_count = 0;
            }
        }

        std::thread::sleep(Duration::from_millis(STABILITY_CHECK_INTERVAL_MS));
    }
}

/// Detect appropriate color mode based on signal HDR status
///
/// Maps vfmcap HDR status to VfmcapColorMode:
/// - SDR / unknown: Passthrough (no conversion needed)
/// - HDR10: HDR10ToSdr (GPU tone mapping)
/// - HLG: HlgToSdr (GPU tone mapping)
fn detect_hdr_color_mode(info: &VfmcapSignalInfo) -> VfmcapColorMode {
    // hdr_status values from libvfmcap:
    // 0 = SDR, 1 = HDR10, 2 = HLG, 3 = HDR10+ (treat as HDR10)
    match info.hdr_status {
        1 | 3 => VfmcapColorMode::Hdr10ToSdr,
        2 => VfmcapColorMode::HlgToSdr,
        _ => VfmcapColorMode::Passthrough,
    }
}
