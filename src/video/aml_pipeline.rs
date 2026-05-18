use bytes::Bytes;
use parking_lot::RwLock as ParkingRwLock;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::watch;
use tracing::{debug, error, info, trace, warn};

use crate::error::{AppError, Result};
use crate::ffi::vfmcap::{VfmcapColorMode, VfmcapOutputFmt, VfmcapSignalInfo};
use crate::utils::LogThrottler;
use crate::video::capture_trait::{CaptureStream, DmaBufFrame, FrameData};
use crate::video::encoder::aml_venc::AmlVencEncoder;
use crate::video::encoder::registry::VideoEncoderType;
use crate::video::vfmcap_capture::AmlVfmcapCaptureStream;

use super::EncodedVideoFrame;

const AUTO_STOP_GRACE_PERIOD_SECS: u64 = 3;
const CAPTURE_TIMEOUT_STOP_THRESHOLD: u32 = 60;
const ENCODE_ERROR_THROTTLE_SECS: u64 = 5;
const VFMCAP_BUFFER_COUNT: u32 = 12;

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

fn create_encoder(
    config: &AmlPipelineConfig,
    width: i32,
    height: i32,
    fps: i32,
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
        ),
        VideoEncoderType::H264 => AmlVencEncoder::new_h264(
            width,
            height,
            fps,
            config.bitrate_kbps as i32,
            config.gop,
            config.gop_pattern,
            config.rc_mode,
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
    pub color_mode: VfmcapColorMode,
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
            color_mode: VfmcapColorMode::Passthrough,
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

    pub fn add_subscriber(
        &self,
        tx: tokio::sync::mpsc::UnboundedSender<Arc<EncodedVideoFrame>>,
    ) {
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
            let stride = encoder.width();

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

        // 1. Open capture at native resolution (0 = native, no upscale/downscale)
        let mut capture = match AmlVfmcapCaptureStream::open(
            &config.device,
            VfmcapOutputFmt::Nv12,
            0, // native resolution
            0,
            0.0, // native fps
            config.color_mode,
            VFMCAP_BUFFER_COUNT,
        ) {
            Ok(c) => c,
            Err(e) => {
                error!("Failed to open vfmcap: {}", e);
                self.running_flag.store(false, Ordering::Release);
                let _ = self.running.send(false);
                return;
            }
        };

        let native_resolution = capture.resolution();
        info!(
            "AML pipeline: capture opened native {}x{}",
            native_resolution.width, native_resolution.height
        );

        // 2. If native resolution exceeds limit, re-open with limit to downscale
        let actual_resolution = if config.max_width > 0
            && config.max_height > 0
            && (native_resolution.width > config.max_width
                || native_resolution.height > config.max_height)
        {
            info!(
                "AML pipeline: native {}x{} exceeds limit {}x{}, downscaling",
                native_resolution.width, native_resolution.height,
                config.max_width, config.max_height
            );
            capture = match AmlVfmcapCaptureStream::open(
                &config.device,
                VfmcapOutputFmt::Nv12,
                config.max_width,
                config.max_height,
                config.max_fps,
                config.color_mode,
                VFMCAP_BUFFER_COUNT,
            ) {
                Ok(c) => c,
                Err(e) => {
                    error!("Failed to re-open vfmcap for downscale: {}", e);
                    self.running_flag.store(false, Ordering::Release);
                    let _ = self.running.send(false);
                    return;
                }
            };
            let res = capture.resolution();
            info!(
                "AML pipeline: capture re-opened at {}x{} (downscaled)",
                res.width, res.height
            );
            res
        } else {
            native_resolution
        };

        // Determine actual fps from signal info or config limit
        let actual_fps = match capture.signal_info() {
            Ok(info) if info.fps > 0 => {
                let fps = if config.max_fps > 0.0 {
                    info.fps.min(config.max_fps as u32)
                } else {
                    info.fps
                };
                info!("AML pipeline: signal fps={}, using fps={}", info.fps, fps);
                fps
            }
            _ => {
                let fps = if config.max_fps > 0.0 {
                    config.max_fps as u32
                } else {
                    30
                };
                info!("AML pipeline: no signal fps, using fps={}", fps);
                fps
            }
        };

        // 3. Open encoder with actual capture resolution (not the limit)
        let mut encoder = match create_encoder(config, actual_resolution.width as i32, actual_resolution.height as i32, actual_fps as i32) {
            Ok(e) => e,
            Err(e) => {
                error!("Failed to create AML encoder: {}", e);
                self.running_flag.store(false, Ordering::Release);
                let _ = self.running.send(false);
                return;
            }
        };

        info!(
            "AML pipeline: encoder created {}x{} @ {}fps codec={:?}",
            actual_resolution.width, actual_resolution.height, actual_fps, config.output_codec
        );
        let encoder_width = encoder.width();
        let encoder_height = encoder.height();

        // 4. Generate and broadcast VPS+SPS+PPS header
        match encoder.generate_header() {
            Ok(header) => {
                info!(
                    "AML pipeline: generated header ({} bytes)",
                    header.len()
                );
                let header_frame = Arc::new(EncodedVideoFrame {
                    data: Bytes::from(header),
                    pts_ms: 0,
                    is_keyframe: true,
                    sequence: 0,
                    duration: Duration::from_millis(0),
                    codec: config.output_codec,
                });
                self.broadcast_encoded(header_frame);
            }
            Err(e) => {
                warn!("Failed to generate encoder header: {}", e);
            }
        }

        // 5. Main capture+encode loop
        let mut consecutive_timeouts: u32 = 0;
        let mut frame_count: u64 = 0;
        let mut fps_frame_count: u64 = 0;
        let mut last_fps_time = Instant::now();

        // --- Task 7.3: HDR color mode detection ---
        // Read signal info and auto-configure color mode based on HDR status
        match capture.signal_info() {
            Ok(info) => {
                let detected_color_mode = detect_hdr_color_mode(&info);
                if detected_color_mode != config.color_mode {
                    info!(
                        "AML pipeline: auto-detected color mode {:?} from HDR status {}",
                        detected_color_mode, info.hdr_status
                    );
                    // Note: color_mode change requires pipeline restart
                }
            }
            Err(e) => {
                debug!("AML pipeline: could not read signal info for HDR detection: {}", e);
            }
        }

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
            if !has_subscribers {
                if last_subscriber_log.elapsed() >= Duration::from_secs(5) {
                    info!("AML pipeline: no subscribers, capturing but not encoding");
                    last_subscriber_log = Instant::now();
                }
            }

            // --- Signal change event polling ---
            // Check for source change events between frames.
            // Actual encoder recreation happens when next_frame returns reconfigured=true.
            let event = capture.poll_event(0);
            if event == crate::ffi::vfmcap::VFMCAP_EVENT_SOURCE_CHANGE {
                info!("AML pipeline: source change event detected, waiting for reconfigured frame...");
            } else if event == crate::ffi::vfmcap::VFMCAP_EVENT_NOSIG {
                trace!("AML pipeline: no signal");
            }

            let capture_result = match capture.next_frame() {
                Ok(r) => {
                    consecutive_timeouts = 0;
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
                        trace!("AML: no signal");
                        std::thread::sleep(Duration::from_millis(100));
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

            // Handle reconfiguration
            if capture_result.reconfigured {
                let new_res = capture_result.resolution;
                info!(
                    "AML pipeline: source reconfigured to {}x{}, stopping for rebuild",
                    new_res.width, new_res.height
                );
                capture.release_frame(&capture_result.frame);
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
                // No subscribers: release frame without encoding to keep vdin alive.
                // Throttle to ~30fps to avoid draining the backlog too quickly,
                // which can cause vfmcap/vdin to stop producing frames.
                std::thread::sleep(Duration::from_millis(33));
                capture.release_frame(&capture_result.frame);
            }

            // FPS tracking
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

        self.running_flag.store(false, Ordering::Release);
        let _ = self.running.send(false);
        info!("AML pipeline stopped (encoded {} frames)", frame_count);
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
