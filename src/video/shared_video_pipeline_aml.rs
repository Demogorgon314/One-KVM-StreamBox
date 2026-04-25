use bytes::Bytes;
use parking_lot::RwLock as ParkingRwLock;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, watch, Mutex, RwLock};

use crate::error::{AppError, Result};
use crate::video::aml_pipeline::{AmlPipeline, AmlPipelineConfig};
use crate::video::encoder::registry::VideoEncoderType;
use crate::video::format::{PixelFormat, Resolution};

#[derive(Debug, Clone)]
pub struct EncodedVideoFrame {
    pub data: Bytes,
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
            encoder_backend: None,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct SharedVideoPipelineStats {
    pub current_fps: f32,
}

pub struct SharedVideoPipeline {
    config: RwLock<SharedVideoPipelineConfig>,
    aml_pipeline: Mutex<Option<Arc<AmlPipeline>>>,
    subscribers: ParkingRwLock<Vec<tokio::sync::mpsc::UnboundedSender<Arc<EncodedVideoFrame>>>>,
    running: watch::Sender<bool>,
    running_rx: watch::Receiver<bool>,
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
        }))
    }

    pub fn subscribe(&self) -> mpsc::Receiver<Arc<EncodedVideoFrame>> {
        let (tx, rx) = mpsc::channel(4);
        let (bridge_tx, mut bridge_rx) = tokio::sync::mpsc::unbounded_channel();
        self.subscribers.write().push(bridge_tx.clone());

        if let Ok(guard) = self.aml_pipeline.try_lock() {
            if let Some(pipeline) = guard.as_ref() {
                pipeline.add_subscriber(bridge_tx.clone());
            }
        }

        tokio::spawn(async move {
            while let Some(frame) = bridge_rx.recv().await {
                if tx.send(frame).await.is_err() {
                    break;
                }
            }
        });

        rx
    }

    pub fn subscriber_count(&self) -> usize {
        self.subscribers.read().len()
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

    pub async fn start_with_device(
        self: &Arc<Self>,
        device_path: std::path::PathBuf,
        _buffer_count: u32,
        _jpeg_quality: u8,
    ) -> Result<()> {
        if *self.running_rx.borrow() {
            return Ok(());
        }

        let config = self.config.read().await.clone();
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

    pub async fn stop_and_wait(&self, _timeout: Duration) {
        self.stop();
    }
}

impl Drop for SharedVideoPipeline {
    fn drop(&mut self) {
        let _ = self.running.send(false);
    }
}
