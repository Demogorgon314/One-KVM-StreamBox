//! Video Stream Manager
//!
//! Unified manager for video streaming that supports single-mode operation.
//! At any given time, only one streaming mode (MJPEG or WebRTC) is active.
//!
//! # Architecture
//!
//! ```text
//! VideoStreamManager (Public API - Single Entry Point)
//!     │
//!     ├── mode: StreamMode (current active mode)
//!     │
//!     ├── MJPEG Mode
//!     │       └── Streamer ──► MjpegStreamHandler
//!     │
//!     └── WebRTC Mode
//!             └── WebRtcStreamer ──► H264SessionManager
//!                 (Extensible: H264, VP8, VP9, H265)
//! ```
//!
//! # Design Goals
//!
//! 1. **Single Entry Point**: All video operations go through VideoStreamManager
//! 2. **Mode Isolation**: MJPEG and WebRTC modes are cleanly separated
//! 3. **Extensible Codecs**: WebRTC supports multiple video codecs (H264 now, others reserved)
//! 4. **Simplified API**: Complex configuration flows are encapsulated

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use crate::config::{ConfigStore, StreamMode};
use crate::error::Result;
use crate::events::{EventBus, SystemEvent, VideoDeviceInfo};
#[cfg(feature = "hwencode")]
use crate::hid::HidController;
#[cfg(feature = "hwencode")]
use crate::stream::MjpegStreamHandler;
#[cfg(feature = "hwencode")]
use crate::video::codec_constraints::StreamCodecConstraints;
use crate::video::format::{PixelFormat, Resolution};
#[cfg(feature = "hwencode")]
use crate::video::is_rk_hdmirx_device;
#[cfg(feature = "hwencode")]
use crate::video::streamer::{Streamer, StreamerState};
#[cfg(feature = "hwencode")]
use crate::webrtc::WebRtcStreamer;

#[cfg(feature = "hwencode")]
use crate::video::encoder::VideoCodecType;

/// Video stream manager configuration
#[derive(Debug, Clone)]
pub struct StreamManagerConfig {
    /// Initial streaming mode
    pub mode: StreamMode,
    /// Video device path
    pub device: Option<String>,
    /// Video format
    pub format: PixelFormat,
    /// Resolution
    pub resolution: Resolution,
    /// FPS
    pub fps: u32,
}

/// Result of a mode switch request.
#[derive(Debug, Clone)]
pub struct ModeSwitchTransaction {
    /// Whether this request started a new switch.
    pub accepted: bool,
    /// Whether a switch is currently in progress after handling this request.
    pub switching: bool,
    /// Transition ID if a switch is/was in progress.
    pub transition_id: Option<String>,
}

impl Default for StreamManagerConfig {
    fn default() -> Self {
        Self {
            mode: StreamMode::WebRTC,
            device: None,
            format: PixelFormat::Nv12,
            resolution: Resolution::HD1080,
            fps: 30,
        }
    }
}

/// Unified video stream manager
///
/// Manages both MJPEG and WebRTC streaming modes, ensuring only one is active
/// at any given time. This reduces resource usage and simplifies the architecture.
///
/// # Components
///
/// - **Streamer**: Handles video capture and MJPEG distribution (current implementation)
/// - **WebRtcStreamer**: High-level WebRTC manager with multi-codec support (new)
/// - **H264SessionManager**: Legacy WebRTC manager (for backward compatibility)
pub struct VideoStreamManager {
    mode: RwLock<StreamMode>,
    #[cfg(feature = "hwencode")]
    streamer: Arc<Streamer>,
    #[cfg(feature = "hwencode")]
    webrtc_streamer: Arc<WebRtcStreamer>,
    events: RwLock<Option<Arc<EventBus>>>,
    config_store: RwLock<Option<ConfigStore>>,
    switching: AtomicBool,
    transition_id: RwLock<Option<String>>,
}

impl VideoStreamManager {
    #[cfg(feature = "hwencode")]
    pub fn with_webrtc_streamer(
        streamer: Arc<Streamer>,
        webrtc_streamer: Arc<WebRtcStreamer>,
    ) -> Arc<Self> {
        Arc::new(Self {
            mode: RwLock::new(StreamMode::Mjpeg),
            streamer,
            webrtc_streamer,
            events: RwLock::new(None),
            config_store: RwLock::new(None),
            switching: AtomicBool::new(false),
            transition_id: RwLock::new(None),
        })
    }

    #[cfg(not(feature = "hwencode"))]
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            mode: RwLock::new(StreamMode::Mjpeg),
            events: RwLock::new(None),
            config_store: RwLock::new(None),
            switching: AtomicBool::new(false),
            transition_id: RwLock::new(None),
        })
    }

    pub fn is_switching(&self) -> bool {
        self.switching.load(Ordering::SeqCst)
    }

    pub async fn current_transition_id(&self) -> Option<String> {
        self.transition_id.read().await.clone()
    }

    pub async fn set_event_bus(&self, events: Arc<EventBus>) {
        *self.events.write().await = Some(events.clone());
        #[cfg(feature = "hwencode")]
        self.webrtc_streamer.set_event_bus(events).await;
    }

    pub async fn set_config_store(&self, config: ConfigStore) {
        *self.config_store.write().await = Some(config);
    }

    #[cfg(feature = "hwencode")]
    pub async fn codec_constraints(&self) -> StreamCodecConstraints {
        if let Some(ref config_store) = *self.config_store.read().await {
            let config = config_store.get();
            StreamCodecConstraints::from_config(&config)
        } else {
            StreamCodecConstraints::unrestricted()
        }
    }

    pub async fn current_mode(&self) -> StreamMode {
        self.mode.read().await.clone()
    }

    pub async fn is_mjpeg_enabled(&self) -> bool {
        *self.mode.read().await == StreamMode::Mjpeg
    }

    pub async fn is_webrtc_enabled(&self) -> bool {
        *self.mode.read().await == StreamMode::WebRTC
    }

    #[cfg(feature = "hwencode")]
    pub fn streamer(&self) -> Arc<Streamer> {
        self.streamer.clone()
    }

    #[cfg(feature = "hwencode")]
    pub fn webrtc_streamer(&self) -> Arc<WebRtcStreamer> {
        self.webrtc_streamer.clone()
    }

    #[cfg(feature = "hwencode")]
    pub fn mjpeg_handler(&self) -> Arc<MjpegStreamHandler> {
        self.streamer.mjpeg_handler()
    }

    #[cfg(feature = "hwencode")]
    pub async fn init_with_mode(self: &Arc<Self>, mode: StreamMode) -> Result<()> {
        info!("Initializing video stream manager with mode: {:?}", mode);
        *self.mode.write().await = mode.clone();

        let needs_init = self.streamer.state().await == StreamerState::Uninitialized;

        if needs_init {
            match mode {
                StreamMode::Mjpeg => {
                    if let Err(e) = self.streamer.init_auto().await {
                        warn!("Failed to auto-initialize MJPEG streamer: {}", e);
                    }
                }
                StreamMode::WebRTC => {
                    if let Err(e) = self.streamer.init_auto().await {
                        warn!("Failed to auto-initialize video capture for WebRTC: {}", e);
                    }
                }
            }
        }

        self.sync_webrtc_capture_source("after init").await;

        Ok(())
    }

    #[cfg(not(feature = "hwencode"))]
    pub async fn init_with_mode(self: &Arc<Self>, mode: StreamMode) -> Result<()> {
        *self.mode.write().await = mode;
        Ok(())
    }

    #[cfg(feature = "hwencode")]
    pub async fn switch_mode(self: &Arc<Self>, new_mode: StreamMode) -> Result<()> {
        let _ = self.switch_mode_transaction(new_mode).await?;
        Ok(())
    }

    #[cfg(feature = "hwencode")]
    pub async fn switch_mode_transaction(
        self: &Arc<Self>,
        new_mode: StreamMode,
    ) -> Result<ModeSwitchTransaction> {
        let current_mode = self.mode.read().await.clone();

        if current_mode == new_mode {
            debug!("Already in {:?} mode, no switch needed", new_mode);
            if new_mode == StreamMode::WebRTC {
                self.ensure_video_capture_running().await?;
            }
            return Ok(ModeSwitchTransaction {
                accepted: false,
                switching: false,
                transition_id: None,
            });
        }

        if self
            .switching
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            debug!("Mode switch already in progress, ignoring duplicate request");
            return Ok(ModeSwitchTransaction {
                accepted: false,
                switching: true,
                transition_id: self.transition_id.read().await.clone(),
            });
        }

        let transition_id = Uuid::new_v4().to_string();
        *self.transition_id.write().await = Some(transition_id.clone());

        let from_mode_str = self.mode_to_string(&current_mode).await;
        let to_mode_str = self.mode_to_string(&new_mode).await;
        self.publish_event(SystemEvent::StreamModeSwitching {
            transition_id: transition_id.clone(),
            to_mode: to_mode_str,
            from_mode: from_mode_str,
        })
        .await;

        let manager = Arc::clone(self);
        let transition_id_for_task = transition_id.clone();
        tokio::spawn(async move {
            let result = manager
                .do_switch_mode(current_mode, new_mode, transition_id_for_task.clone())
                .await;

            if let Err(e) = result {
                error!(
                    "Mode switch transaction {} failed: {}",
                    transition_id_for_task, e
                );
            }

            let actual_mode = manager.mode.read().await.clone();
            let actual_mode_str = manager.mode_to_string(&actual_mode).await;
            manager
                .publish_event(SystemEvent::StreamModeReady {
                    transition_id: transition_id_for_task.clone(),
                    mode: actual_mode_str,
                })
                .await;

            *manager.transition_id.write().await = None;
            manager.switching.store(false, Ordering::SeqCst);
        });

        Ok(ModeSwitchTransaction {
            accepted: true,
            switching: true,
            transition_id: Some(transition_id),
        })
    }

    #[cfg(not(feature = "hwencode"))]
    pub async fn switch_mode_transaction(
        self: &Arc<Self>,
        new_mode: StreamMode,
    ) -> Result<ModeSwitchTransaction> {
        *self.mode.write().await = new_mode;
        Ok(ModeSwitchTransaction {
            accepted: true,
            switching: false,
            transition_id: None,
        })
    }

    #[cfg(feature = "hwencode")]
    async fn mode_to_string(&self, mode: &StreamMode) -> String {
        match mode {
            StreamMode::Mjpeg => "mjpeg".to_string(),
            StreamMode::WebRTC => {
                let codec = self.webrtc_streamer.current_video_codec().await;
                codec_to_string(codec)
            }
        }
    }

    #[cfg(feature = "hwencode")]
    async fn ensure_video_capture_running(self: &Arc<Self>) -> Result<()> {
        if self.streamer.state().await == StreamerState::Uninitialized {
            info!("Initializing video capture for WebRTC (ensure)");
            if let Err(e) = self.streamer.init_auto().await {
                error!("Failed to initialize video capture: {}", e);
                return Err(e);
            }
        }

        self.sync_webrtc_capture_source("for WebRTC ensure").await;

        Ok(())
    }

    #[cfg(feature = "hwencode")]
    async fn sync_webrtc_capture_source(&self, reason: &str) {
        let (device_path, resolution, format, fps, jpeg_quality) =
            self.streamer.current_capture_config().await;
        info!(
            "Syncing WebRTC capture source {}: {}x{} {:?} @ {}fps",
            reason, resolution.width, resolution.height, format, fps
        );
        self.webrtc_streamer
            .update_video_config(resolution, format, fps)
            .await;
        if let Some(device_path) = device_path {
            self.webrtc_streamer
                .set_capture_device(device_path, jpeg_quality)
                .await;
        } else {
            warn!("No capture device configured while syncing WebRTC capture source");
        }
    }

    #[cfg(feature = "hwencode")]
    async fn do_switch_mode(
        self: &Arc<Self>,
        current_mode: StreamMode,
        new_mode: StreamMode,
        transition_id: String,
    ) -> Result<()> {
        info!("Switching video mode: {:?} -> {:?}", current_mode, new_mode);

        let new_mode_str = match &new_mode {
            StreamMode::Mjpeg => "mjpeg".to_string(),
            StreamMode::WebRTC => {
                let codec = self.webrtc_streamer.current_video_codec().await;
                codec_to_string(codec)
            }
        };
        let previous_mode_str = match &current_mode {
            StreamMode::Mjpeg => "mjpeg".to_string(),
            StreamMode::WebRTC => {
                let codec = self.webrtc_streamer.current_video_codec().await;
                codec_to_string(codec)
            }
        };

        self.publish_event(SystemEvent::StreamModeChanged {
            transition_id: Some(transition_id.clone()),
            mode: new_mode_str,
            previous_mode: previous_mode_str,
        })
        .await;

        match current_mode {
            StreamMode::Mjpeg => {
                info!("Stopping MJPEG streaming");
                self.streamer.mjpeg_handler().set_offline();
                if let Err(e) = self.streamer.stop().await {
                    warn!("Error stopping MJPEG streamer: {}", e);
                }
            }
            StreamMode::WebRTC => {
                info!("Closing all WebRTC sessions and releasing capture device");
                let closed = self
                    .webrtc_streamer
                    .close_all_sessions_and_release_device()
                    .await;
                if closed > 0 {
                    info!("Closed {} WebRTC sessions", closed);
                }
            }
        }

        *self.mode.write().await = new_mode.clone();

        match new_mode {
            StreamMode::Mjpeg => {
                info!("Starting MJPEG streaming");

                if let Some(device) = self.streamer.current_device().await {
                    let (current_format, resolution, fps) =
                        self.streamer.current_video_config().await;
                    let available_formats: Vec<PixelFormat> =
                        device.formats.iter().map(|f| f.format).collect();

                    if !is_rk_hdmirx_device(&device)
                        && current_format != PixelFormat::Mjpeg
                        && available_formats.contains(&PixelFormat::Mjpeg)
                    {
                        info!("Auto-switching to MJPEG format for MJPEG mode");
                        let device_path = device.path.to_string_lossy().to_string();
                        if let Err(e) = self
                            .streamer
                            .apply_video_config(&device_path, PixelFormat::Mjpeg, resolution, fps)
                            .await
                        {
                            warn!(
                                "Failed to auto-switch to MJPEG format: {}, keeping current format",
                                e
                            );
                        }
                    }
                }

                if let Err(e) = self.streamer.start().await {
                    error!("Failed to start MJPEG streamer: {}", e);
                    return Err(e);
                }
            }
            StreamMode::WebRTC => {
                info!("Activating WebRTC mode");

                if self.streamer.state().await == StreamerState::Uninitialized {
                    info!("Initializing video capture for WebRTC");
                    if let Err(e) = self.streamer.init_auto().await {
                        error!("Failed to initialize video capture for WebRTC: {}", e);
                        return Err(e);
                    }
                }

                self.sync_webrtc_capture_source("for WebRTC mode").await;

                let codec = self.webrtc_streamer.current_video_codec().await;
                let is_hardware = self.webrtc_streamer.is_hardware_encoding().await;
                self.publish_event(SystemEvent::WebRTCReady {
                    transition_id: Some(transition_id.clone()),
                    codec: codec_to_string(codec),
                    hardware: is_hardware,
                })
                .await;

                info!("WebRTC mode activated (sessions created on-demand)");
            }
        }

        if let Some(ref config_store) = *self.config_store.read().await {
            let mut config = (*config_store.get()).clone();
            config.stream.mode = new_mode.clone();
            if let Err(e) = config_store.set(config).await {
                warn!("Failed to persist stream mode to config: {}", e);
            }
        }

        info!("Video mode switched to {:?}", new_mode);
        Ok(())
    }

    #[cfg(feature = "hwencode")]
    pub async fn apply_video_config(
        self: &Arc<Self>,
        device_path: &str,
        format: PixelFormat,
        resolution: Resolution,
        fps: u32,
    ) -> Result<()> {
        let mode = self.mode.read().await.clone();

        info!(
            "Applying video config: {} {:?} {}x{} @ {} fps (mode: {:?})",
            device_path, format, resolution.width, resolution.height, fps, mode
        );

        if mode == StreamMode::WebRTC {
            self.webrtc_streamer
                .update_video_config(resolution, format, fps)
                .await;
            info!("WebRTC streamer config updated (pipeline stopped, sessions closed)");
        }

        self.streamer
            .apply_video_config(device_path, format, resolution, fps)
            .await?;

        if mode != StreamMode::WebRTC {
            if let Err(e) = self.start().await {
                error!("Failed to start streamer after config change: {}", e);
            } else {
                info!("Streamer started after config change");
            }
        }

        if mode == StreamMode::WebRTC {
            let (device_path, actual_resolution, actual_format, actual_fps, jpeg_quality) =
                self.streamer.current_capture_config().await;
            if actual_format != format || actual_resolution != resolution || actual_fps != fps {
                info!(
                    "Actual capture config differs from requested, updating WebRTC: {}x{} {:?} @ {}fps",
                    actual_resolution.width, actual_resolution.height, actual_format, actual_fps
                );
                self.webrtc_streamer
                    .update_video_config(actual_resolution, actual_format, actual_fps)
                    .await;
            }
            if let Some(device_path) = device_path {
                info!("Configuring direct capture for WebRTC after config change");
                self.webrtc_streamer
                    .set_capture_device(device_path, jpeg_quality)
                    .await;
            } else {
                warn!("No capture device configured for WebRTC after config change");
            }

            let codec = self.webrtc_streamer.current_video_codec().await;
            let is_hardware = self.webrtc_streamer.is_hardware_encoding().await;
            self.publish_event(SystemEvent::WebRTCReady {
                transition_id: None,
                codec: codec_to_string(codec),
                hardware: is_hardware,
            })
            .await;
        }

        Ok(())
    }

    #[cfg(feature = "hwencode")]
    pub async fn start(self: &Arc<Self>) -> Result<()> {
        let mode = self.mode.read().await.clone();

        match mode {
            StreamMode::Mjpeg => {
                self.streamer.start().await?;
            }
            StreamMode::WebRTC => {
                if self.streamer.state().await == StreamerState::Uninitialized {
                    self.streamer.init_auto().await?;
                }

                self.sync_webrtc_capture_source("before start").await;
            }
        }

        Ok(())
    }

    #[cfg(not(feature = "hwencode"))]
    pub async fn start(self: &Arc<Self>) -> Result<()> {
        Ok(())
    }

    #[cfg(feature = "hwencode")]
    pub async fn stop(&self) -> Result<()> {
        let mode = self.mode.read().await.clone();

        match mode {
            StreamMode::Mjpeg => {
                self.streamer.stop().await?;
            }
            StreamMode::WebRTC => {
                self.webrtc_streamer.close_all_sessions().await;
                self.streamer.stop().await?;
            }
        }

        Ok(())
    }

    #[cfg(not(feature = "hwencode"))]
    pub async fn stop(&self) -> Result<()> {
        Ok(())
    }

    #[cfg(feature = "hwencode")]
    pub async fn get_video_info(&self) -> VideoDeviceInfo {
        let stats = self.streamer.stats().await;
        let state = self.streamer.state().await;
        let device = self.streamer.current_device().await;
        let mode = self.mode.read().await.clone();

        let stream_mode = match &mode {
            StreamMode::Mjpeg => "mjpeg".to_string(),
            StreamMode::WebRTC => {
                let codec = self.webrtc_streamer.current_video_codec().await;
                codec_to_string(codec)
            }
        };

        VideoDeviceInfo {
            available: state != StreamerState::Uninitialized,
            device: device.map(|d| d.path.display().to_string()),
            format: stats.format,
            resolution: stats.resolution,
            fps: stats.target_fps,
            online: state == StreamerState::Streaming,
            stream_mode,
            config_changing: self.streamer.is_config_changing(),
            error: if state == StreamerState::Error {
                Some("Video stream error".to_string())
            } else if state == StreamerState::NoSignal {
                Some("No video signal".to_string())
            } else {
                None
            },
        }
    }

    #[cfg(not(feature = "hwencode"))]
    pub async fn get_video_info(&self) -> VideoDeviceInfo {
        let mode = self.mode.read().await.clone();
        let stream_mode = match &mode {
            StreamMode::Mjpeg => "mjpeg".to_string(),
            StreamMode::WebRTC => "webrtc".to_string(),
        };
        VideoDeviceInfo {
            available: false,
            device: None,
            format: String::new(),
            resolution: String::new(),
            fps: 0,
            online: false,
            stream_mode,
            config_changing: false,
            error: None,
        }
    }

    #[cfg(feature = "hwencode")]
    pub fn mjpeg_client_count(&self) -> u64 {
        self.streamer.mjpeg_handler().client_count()
    }

    #[cfg(feature = "hwencode")]
    pub async fn webrtc_session_count(&self) -> usize {
        self.webrtc_streamer.session_count().await
    }

    #[cfg(feature = "hwencode")]
    pub async fn set_hid_controller(&self, hid: Arc<HidController>) {
        self.webrtc_streamer.set_hid_controller(hid).await;
    }

    #[cfg(feature = "hwencode")]
    pub async fn set_webrtc_audio_enabled(&self, enabled: bool) -> Result<()> {
        self.webrtc_streamer.set_audio_enabled(enabled).await
    }

    #[cfg(feature = "hwencode")]
    pub async fn is_webrtc_audio_enabled(&self) -> bool {
        self.webrtc_streamer.is_audio_enabled().await
    }

    #[cfg(feature = "hwencode")]
    pub async fn reconnect_webrtc_audio_sources(&self) {
        self.webrtc_streamer.reconnect_audio_sources().await;
    }

    #[cfg(feature = "hwencode")]
    pub async fn list_devices(
        &self,
    ) -> crate::error::Result<Vec<crate::video::device::VideoDeviceInfo>> {
        self.streamer.list_devices().await
    }

    #[cfg(feature = "hwencode")]
    pub async fn stats(&self) -> crate::video::streamer::StreamerStats {
        self.streamer.stats().await
    }

    #[cfg(feature = "hwencode")]
    pub fn is_config_changing(&self) -> bool {
        self.streamer.is_config_changing()
    }

    #[cfg(feature = "hwencode")]
    pub async fn is_streaming(&self) -> bool {
        self.streamer.is_streaming().await
    }

    #[cfg(feature = "hwencode")]
    pub async fn subscribe_encoded_frames(
        &self,
    ) -> Option<
        tokio::sync::mpsc::Receiver<
            std::sync::Arc<crate::video::shared_video_pipeline::EncodedVideoFrame>,
        >,
    > {
        if self.streamer.state().await == StreamerState::Uninitialized {
            tracing::info!("Initializing video capture for encoded frame subscription");
            if let Err(e) = self.streamer.init_auto().await {
                tracing::error!(
                    "Failed to initialize video capture for encoded frames: {}",
                    e
                );
                return None;
            }
        }

        let (device_path, _, _, _, _) = self.streamer.current_capture_config().await;
        self.sync_webrtc_capture_source("for encoded frame subscription")
            .await;
        if device_path.is_none() {
            return None;
        }

        match self
            .webrtc_streamer
            .ensure_video_pipeline_for_external()
            .await
        {
            Ok(pipeline) => Some(pipeline.subscribe()),
            Err(e) => {
                tracing::error!("Failed to start shared video pipeline: {}", e);
                None
            }
        }
    }

    #[cfg(feature = "hwencode")]
    pub async fn get_encoding_config(
        &self,
    ) -> Option<crate::video::shared_video_pipeline::SharedVideoPipelineConfig> {
        self.webrtc_streamer.get_pipeline_config().await
    }

    #[cfg(feature = "hwencode")]
    pub async fn set_video_codec(
        &self,
        codec: crate::video::encoder::VideoCodecType,
    ) -> crate::error::Result<()> {
        self.webrtc_streamer.set_video_codec(codec).await
    }

    #[cfg(feature = "hwencode")]
    pub async fn set_bitrate_preset(
        &self,
        preset: crate::video::encoder::BitratePreset,
    ) -> crate::error::Result<()> {
        self.webrtc_streamer.set_bitrate_preset(preset).await
    }

    #[cfg(feature = "hwencode")]
    pub async fn request_keyframe(&self) -> crate::error::Result<()> {
        self.webrtc_streamer.request_keyframe().await
    }

    #[cfg(feature = "hwencode")]
    pub async fn notify_codec_switch(
        self: &Arc<Self>,
        transition_id: &str,
        new_codec_str: &str,
        previous_codec_str: &str,
    ) {
        let manager = Arc::clone(self);
        let transition_id = transition_id.to_string();
        let new_codec = new_codec_str.to_string();
        let prev_codec = previous_codec_str.to_string();

        tokio::spawn(async move {
            tokio::task::yield_now().await;

            manager
                .publish_event(SystemEvent::StreamModeChanged {
                    transition_id: Some(transition_id.clone()),
                    mode: new_codec.clone(),
                    previous_mode: prev_codec.clone(),
                })
                .await;

            let is_hardware = manager.webrtc_streamer.is_hardware_encoding().await;
            manager
                .publish_event(SystemEvent::WebRTCReady {
                    transition_id: Some(transition_id.clone()),
                    codec: new_codec.clone(),
                    hardware: is_hardware,
                })
                .await;

            manager
                .publish_event(SystemEvent::StreamModeReady {
                    transition_id: transition_id.clone(),
                    mode: new_codec.clone(),
                })
                .await;

            info!(
                "Codec switch notified: {} -> {} (transition: {})",
                prev_codec, new_codec, transition_id
            );
        });
    }

    async fn publish_event(&self, event: SystemEvent) {
        if let Some(ref events) = *self.events.read().await {
            events.publish(event);
        }
    }
}

#[cfg(feature = "hwencode")]
fn codec_to_string(codec: crate::video::encoder::VideoCodecType) -> String {
    match codec {
        crate::video::encoder::VideoCodecType::H264 => "h264".to_string(),
        crate::video::encoder::VideoCodecType::H265 => "h265".to_string(),
        crate::video::encoder::VideoCodecType::VP8 => "vp8".to_string(),
        crate::video::encoder::VideoCodecType::VP9 => "vp9".to_string(),
    }
}

#[cfg(all(test, feature = "hwencode"))]
mod tests {
    use super::*;
    use crate::video::encoder::VideoCodecType;

    #[test]
    fn test_codec_to_string() {
        assert_eq!(codec_to_string(VideoCodecType::H264), "h264");
        assert_eq!(codec_to_string(VideoCodecType::H265), "h265");
        assert_eq!(codec_to_string(VideoCodecType::VP8), "vp8");
        assert_eq!(codec_to_string(VideoCodecType::VP9), "vp9");
    }
}
