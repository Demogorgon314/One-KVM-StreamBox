use parking_lot::RwLock as ParkingRwLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, watch, Mutex, RwLock};
use tracing::{info, warn};

use crate::error::{AppError, Result};
use crate::video::aml_pipeline::{AmlPipeline, AmlPipelineConfig};
use crate::video::encoder::registry::VideoEncoderType;
use crate::video::format::{PixelFormat, Resolution};

#[derive(Debug, Clone)]
pub struct EncodedVideoFrame {
    pub data: bytes::Bytes,
    pub pts_ms: i64,
    pub is_keyframe: bool,
    pub sequence: u64,
    pub duration: Duration,
    pub codec: VideoEncoderType,
}

#[derive(Debug, Clone)]
pub struct SharedVideoPipelineConfig {
    pub resolution: Resolution,
    pub input_format: PixelFormat,
    pub output_codec: VideoEncoderType,
    pub bitrate_preset: crate::video::encoder::BitratePreset,
    pub fps: u32,
    pub hdr_mode: crate::config::HdrMode,
    pub encoder_backend: Option<crate::video::encoder::registry::EncoderBackend>,
}

impl Default for SharedVideoPipelineConfig {
    fn default() -> Self {
        Self {
            resolution: Resolution::HD720,
            input_format: PixelFormat::Nv12,
            output_codec: VideoEncoderType::H264,
            bitrate_preset: crate::video::encoder::BitratePreset::Balanced,
            fps: 30,
            hdr_mode: crate::config::HdrMode::Auto,
            encoder_backend: None,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct SharedVideoPipelineStats {
    pub current_fps: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PipelineStateNotification {
    pub state: &'static str,
    pub reason: Option<&'static str>,
    pub next_retry_ms: Option<u64>,
}

pub struct SharedVideoPipeline {
    config: RwLock<SharedVideoPipelineConfig>,
    aml_pipeline: Mutex<Option<Arc<AmlPipeline>>>,
    subscribers: ParkingRwLock<Vec<tokio::sync::mpsc::UnboundedSender<Arc<EncodedVideoFrame>>>>,
    running: watch::Sender<bool>,
    running_rx: watch::Receiver<bool>,
    pending_reconnect: AtomicBool,
}

impl SharedVideoPipeline {
    pub fn new(config: SharedVideoPipelineConfig) -> Result<Arc<Self>> {
        let (running, running_rx) = watch::channel(false);
        Ok(Arc::new(Self {
            config: RwLock::new(config),
            aml_pipeline: Mutex::new(None),
            subscribers: ParkingRwLock::new(Vec::new()),
            running,
            running_rx,
            pending_reconnect: AtomicBool::new(false),
        }))
    }

    pub fn subscribe(self: &Arc<Self>) -> mpsc::Receiver<Arc<EncodedVideoFrame>> {
        let (tx, rx) = mpsc::channel(4);
        let (bridge_tx, mut bridge_rx) = tokio::sync::mpsc::unbounded_channel();
        self.subscribers.write().push(bridge_tx.clone());

        if let Ok(guard) = self.aml_pipeline.try_lock() {
            if let Some(pipeline) = guard.as_ref() {
                pipeline.add_subscriber(bridge_tx.clone());
            }
        }

        let weak = Arc::downgrade(self);
        tokio::spawn(async move {
            while let Some(frame) = bridge_rx.recv().await {
                if tx.send(frame).await.is_err() {
                    break;
                }
            }
            // Bridge task exited: remove our sender from the subscribers list
            if let Some(pipeline) = weak.upgrade() {
                let mut subs = pipeline.subscribers.write();
                subs.retain(|s| !s.same_channel(&bridge_tx));
            }
        });

        rx
    }

    pub fn subscriber_count(&self) -> usize {
        let mut subs = self.subscribers.write();
        subs.retain(|tx| !tx.is_closed());
        subs.len()
    }

    pub async fn request_keyframe(&self) {
        if let Some(pipeline) = self.aml_pipeline.lock().await.as_ref() {
            pipeline.request_keyframe();
        }
    }

    pub async fn stats(&self) -> SharedVideoPipelineStats {
        SharedVideoPipelineStats::default()
    }

    pub fn is_running(&self) -> bool {
        *self.running_rx.borrow()
    }

    pub fn running_watch(&self) -> watch::Receiver<bool> {
        self.running_rx.clone()
    }

    pub fn set_state_notifier(
        &self,
        _notifier: Option<Arc<dyn Fn(PipelineStateNotification) + Send + Sync>>,
    ) {
    }

    pub fn take_pending_sync_geometry(&self) -> Option<(Resolution, PixelFormat)> {
        if self.pending_reconnect.swap(false, Ordering::AcqRel) {
            self.config
                .try_read()
                .ok()
                .map(|config| (config.resolution, config.input_format))
        } else {
            None
        }
    }

    pub fn take_device_lost_reason(&self) -> Option<String> {
        None
    }

    pub async fn start_with_device(
        self: &Arc<Self>,
        device_path: std::path::PathBuf,
        _buffer_count: u32,
        _jpeg_quality: u8,
        _subdev_path: Option<std::path::PathBuf>,
        _bridge_kind: Option<String>,
        _v4l2_driver: Option<String>,
    ) -> Result<()> {
        if *self.running_rx.borrow() {
            return Ok(());
        }

        let config = self.config.read().await.clone();
        self.pending_reconnect.store(true, Ordering::Release);
        let output_codec = match config.output_codec {
            VideoEncoderType::H264 | VideoEncoderType::H265 => config.output_codec,
            other => {
                return Err(AppError::VideoError(format!(
                    "AML pipeline does not support codec {}",
                    other
                )))
            }
        };

        let aml = Arc::new(AmlPipeline::new(AmlPipelineConfig {
            max_width: config.resolution.width,
            max_height: config.resolution.height,
            max_fps: config.fps as f32,
            output_codec,
            bitrate_kbps: config.bitrate_preset.bitrate_kbps(),
            hdr_mode: config.hdr_mode,
            device: device_path.to_string_lossy().to_string(),
            ..AmlPipelineConfig::default()
        }));

        for tx in self.subscribers.read().iter() {
            aml.add_subscriber(tx.clone());
        }

        aml.start().await?;
        *self.aml_pipeline.lock().await = Some(aml);
        let _ = self.running.send(true);
        Ok(())
    }

    pub async fn config(&self) -> SharedVideoPipelineConfig {
        self.config.read().await.clone()
    }

    pub fn stop(&self) {
        if let Ok(mut guard) = self.aml_pipeline.try_lock() {
            if let Some(pipeline) = guard.take() {
                pipeline.stop();
            }
        }
        let _ = self.running.send(false);
    }

    pub async fn stop_and_wait(&self, timeout: Duration) {
        let pipeline = {
            let mut guard = self.aml_pipeline.lock().await;
            guard.take()
        };
        info!("stop_and_wait: pipeline is_some={}", pipeline.is_some());
        // Mark as not running immediately so monitor task can clean up
        // even if this future is cancelled (e.g. HTTP request aborted).
        let _ = self.running.send(false);
        if let Some(pipeline) = pipeline {
            pipeline.stop();
            // Block in spawn_blocking so we don't starve the async runtime
            info!("stop_and_wait: spawning blocking task");
            let handle = tokio::task::spawn_blocking(move || {
                info!("blocking task: calling wait_for_stop");
                let result = pipeline.wait_for_stop(timeout);
                info!("blocking task: wait_for_stop returned {}", result);
                result
            });
            info!("stop_and_wait: awaiting blocking task");
            match handle.await {
                Ok(result) => info!("stop_and_wait: blocking task completed with {}", result),
                Err(e) => warn!("stop_and_wait: blocking task panicked: {}", e),
            }
        }
        info!("stop_and_wait: done");
    }
}

impl Drop for SharedVideoPipeline {
    fn drop(&mut self) {
        let _ = self.running.send(false);
    }
}
