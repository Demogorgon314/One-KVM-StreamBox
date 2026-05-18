use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;

use super::device::{enumerate_devices, find_best_device, VideoDeviceInfo};
use super::format::{PixelFormat, Resolution};
use crate::error::{AppError, Result};
use crate::events::{EventBus, SystemEvent};
use crate::stream::MjpegStreamHandler;

#[derive(Debug, Clone)]
pub struct StreamerConfig {
    pub device_path: Option<PathBuf>,
    pub resolution: Resolution,
    pub format: PixelFormat,
    pub fps: u32,
    pub jpeg_quality: u8,
}

impl Default for StreamerConfig {
    fn default() -> Self {
        Self {
            device_path: None,
            resolution: Resolution::HD1080,
            format: PixelFormat::Nv12,
            fps: 30,
            jpeg_quality: 80,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamerState {
    Uninitialized,
    Ready,
    Streaming,
    NoSignal,
    Error,
    DeviceLost,
    Recovering,
}

pub struct Streamer {
    config: RwLock<StreamerConfig>,
    mjpeg_handler: Arc<MjpegStreamHandler>,
    current_device: RwLock<Option<VideoDeviceInfo>>,
    state: RwLock<StreamerState>,
    events: RwLock<Option<Arc<EventBus>>>,
    config_changing: std::sync::atomic::AtomicBool,
}

impl Streamer {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            config: RwLock::new(StreamerConfig::default()),
            mjpeg_handler: Arc::new(MjpegStreamHandler::new()),
            current_device: RwLock::new(None),
            state: RwLock::new(StreamerState::Uninitialized),
            events: RwLock::new(None),
            config_changing: std::sync::atomic::AtomicBool::new(false),
        })
    }

    pub fn with_config(config: StreamerConfig) -> Arc<Self> {
        Arc::new(Self {
            config: RwLock::new(config),
            mjpeg_handler: Arc::new(MjpegStreamHandler::new()),
            current_device: RwLock::new(None),
            state: RwLock::new(StreamerState::Uninitialized),
            events: RwLock::new(None),
            config_changing: std::sync::atomic::AtomicBool::new(false),
        })
    }

    pub async fn set_event_bus(&self, events: Arc<EventBus>) {
        *self.events.write().await = Some(events);
    }

    pub async fn state(&self) -> StreamerState {
        *self.state.read().await
    }

    pub fn is_config_changing(&self) -> bool {
        self.config_changing.load(std::sync::atomic::Ordering::SeqCst)
    }

    pub async fn is_streaming(&self) -> bool {
        self.state().await == StreamerState::Streaming
    }

    pub fn mjpeg_handler(&self) -> Arc<MjpegStreamHandler> {
        self.mjpeg_handler.clone()
    }

    pub async fn current_device(&self) -> Option<VideoDeviceInfo> {
        self.current_device.read().await.clone()
    }

    pub async fn current_video_config(&self) -> (PixelFormat, Resolution, u32) {
        let config = self.config.read().await;
        (config.format, config.resolution, config.fps)
    }

    pub async fn current_capture_config(&self) -> (Option<PathBuf>, Resolution, PixelFormat, u32, u8) {
        let config = self.config.read().await;
        (
            config.device_path.clone(),
            config.resolution,
            config.format,
            config.fps,
            config.jpeg_quality,
        )
    }

    pub async fn list_devices(&self) -> Result<Vec<VideoDeviceInfo>> {
        enumerate_devices()
    }

    pub async fn apply_video_config(
        self: &Arc<Self>,
        device_path: &str,
        format: PixelFormat,
        resolution: Resolution,
        fps: u32,
    ) -> Result<()> {
        self.config_changing.store(true, std::sync::atomic::Ordering::SeqCst);

        let device = enumerate_devices()?
            .into_iter()
            .find(|d| d.path.to_string_lossy() == device_path)
            .ok_or_else(|| AppError::VideoError("Video device not found".to_string()))?;

        {
            let mut cfg = self.config.write().await;
            cfg.device_path = Some(device.path.clone());
            cfg.format = format;
            cfg.resolution = resolution;
            cfg.fps = fps;
        }
        *self.current_device.write().await = Some(device.clone());
        *self.state.write().await = StreamerState::Ready;
        self.publish_event(SystemEvent::StreamConfigApplied {
            transition_id: None,
            device: device_path.to_string(),
            resolution: (resolution.width, resolution.height),
            format: format.to_string(),
            fps,
        })
        .await;

        self.config_changing.store(false, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }

    pub async fn init_auto(self: &Arc<Self>) -> Result<()> {
        let device = find_best_device()?;
        {
            let mut cfg = self.config.write().await;
            cfg.device_path = Some(device.path.clone());
            cfg.format = PixelFormat::Nv12;
            cfg.resolution = Resolution::HD1080;
        }
        *self.current_device.write().await = Some(device);
        *self.state.write().await = StreamerState::Ready;
        self.publish_state().await;
        Ok(())
    }

    pub async fn start(self: &Arc<Self>) -> Result<()> {
        if self.state().await == StreamerState::Uninitialized {
            self.init_auto().await?;
        }
        self.mjpeg_handler.set_online();
        *self.state.write().await = StreamerState::Streaming;
        self.publish_state().await;
        Ok(())
    }

    pub async fn stop(&self) -> Result<()> {
        self.mjpeg_handler.set_offline();
        *self.state.write().await = StreamerState::Ready;
        self.publish_state().await;
        Ok(())
    }

    pub async fn stats(&self) -> StreamerStats {
        let config = self.config.read().await;
        // Query actual HDMI signal resolution from vfmcap device.
        // The configured resolution may be a max/downscaled limit,
        // but the UI should show the actual input signal size.
        let actual_resolution = config
            .device_path
            .as_ref()
            .and_then(|path| query_vfmcap_signal_resolution(path))
            .unwrap_or(config.resolution);
        tracing::debug!(
            "Streamer stats: configured={}x{}, actual={}x{}",
            config.resolution.width,
            config.resolution.height,
            actual_resolution.width,
            actual_resolution.height
        );
        StreamerStats {
            state: self.state().await,
            device: self.current_device().await.map(|d| d.name),
            format: Some(config.format.to_string()),
            resolution: Some((actual_resolution.width, actual_resolution.height)),
            clients: self.mjpeg_handler.client_count(),
            target_fps: config.fps,
            fps: 0.0,
        }
    }

    async fn publish_state(&self) {
        let state = self.state().await;
        let state = match state {
            StreamerState::Uninitialized => "uninitialized",
            StreamerState::Ready => "ready",
            StreamerState::Streaming => "streaming",
            StreamerState::NoSignal => "no_signal",
            StreamerState::Error => "error",
            StreamerState::DeviceLost => "device_lost",
            StreamerState::Recovering => "recovering",
        };
        self.publish_event(SystemEvent::StreamStateChanged {
            state: state.to_string(),
            device: self.current_device().await.map(|d| d.path.display().to_string()),
            reason: None,
            next_retry_ms: None,
        })
        .await;
    }

    async fn publish_event(&self, event: SystemEvent) {
        if let Some(events) = self.events.read().await.as_ref() {
            events.publish(event);
        }
    }
}

impl Default for Streamer {
    fn default() -> Self {
        Self {
            config: RwLock::new(StreamerConfig::default()),
            mjpeg_handler: Arc::new(MjpegStreamHandler::new()),
            current_device: RwLock::new(None),
            state: RwLock::new(StreamerState::Uninitialized),
            events: RwLock::new(None),
            config_changing: std::sync::atomic::AtomicBool::new(false),
        }
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct StreamerStats {
    pub state: StreamerState,
    pub device: Option<String>,
    pub format: Option<String>,
    pub resolution: Option<(u32, u32)>,
    pub clients: u64,
    pub target_fps: u32,
    pub fps: f32,
}

impl serde::Serialize for StreamerState {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let s = match self {
            StreamerState::Uninitialized => "uninitialized",
            StreamerState::Ready => "ready",
            StreamerState::Streaming => "streaming",
            StreamerState::NoSignal => "no_signal",
            StreamerState::Error => "error",
            StreamerState::DeviceLost => "device_lost",
            StreamerState::Recovering => "recovering",
        };
        serializer.serialize_str(s)
    }
}

/// Query the actual HDMI signal resolution from a vfmcap device.
/// This opens the device briefly to get signal info without starting capture.
fn query_vfmcap_signal_resolution(device_path: &std::path::Path) -> Option<Resolution> {
    use crate::ffi::vfmcap::{
        vfmcap_close, vfmcap_get_signal_info, vfmcap_open, VfmcapColorMode, VfmcapConfig,
        VfmcapOutputFmt, VfmcapSignalInfo,
    };
    use std::ffi::CString;
    use std::os::raw::c_char;

    let device_str = CString::new(device_path.to_string_lossy().as_bytes()).ok()?;
    let config = VfmcapConfig {
        output_format: VfmcapOutputFmt::Nv12,
        target_width: 0,
        target_height: 0,
        target_fps: 0.0,
        color_mode: VfmcapColorMode::Passthrough,
    };

    let ctx = unsafe { vfmcap_open(device_str.as_ptr() as *const c_char, &config) };
    if ctx.is_null() {
        return None;
    }

    let mut info = VfmcapSignalInfo {
        width: 0,
        height: 0,
        fps: 0,
        pixelformat: 0,
        signal_type: 0,
        hdr_status: 0,
        is_interlaced: 0,
        status: 0,
        bitdepth: 0,
    };

    let result = unsafe { vfmcap_get_signal_info(ctx, &mut info) };
    unsafe { vfmcap_close(ctx) };

    if result == 0 && info.width > 0 && info.height > 0 {
        Some(Resolution::new(info.width, info.height))
    } else {
        None
    }
}
