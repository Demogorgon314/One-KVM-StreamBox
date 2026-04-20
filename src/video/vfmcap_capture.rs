use std::ffi::CStr;
use std::io;
use std::ptr;

use crate::error::{AppError, Result};
use crate::ffi::vfmcap::*;
use crate::video::capture_trait::{CaptureResult, CaptureStream, DmaBufFrame, FrameData};
use crate::video::format::{PixelFormat, Resolution};

pub struct VfmcapStream {
    ctx: *mut VfmcapCtx,
}

unsafe impl Send for VfmcapStream {}

#[derive(Debug, Clone)]
pub struct VfmcapFrameData {
    pub dmabuf_fd: i32,
    pub dmabuf_fd2: i32,
    pub index: u32,
    pub width: u32,
    pub height: u32,
    pub bytesperline: u32,
    pub size: u32,
    pub pixelformat: u32,
    pub bitdepth: u32,
    pub sequence: u32,
    pub timestamp_us: u64,
    pub signal_type: u32,
    pub drm_modifier: u64,
    pub is_repeated: bool,
}

pub enum AcquireResult {
    Frame(VfmcapFrameData),
    Reconfigured(VfmcapFrameData),
    Timeout,
    NoSignal,
}

impl VfmcapStream {
    pub fn open(device: &str, config: &VfmcapConfig) -> Result<Self> {
        let c_device = if device.is_empty() {
            ptr::null()
        } else {
            std::ffi::CString::new(device)
                .map_err(|e| AppError::VideoError(format!("Invalid device path: {}", e)))?
        };
        let ctx = unsafe {
            vfmcap_open(
                if c_device.is_null() {
                    ptr::null()
                } else {
                    c_device.as_ptr()
                },
                config,
            )
        };
        if ctx.is_null() {
            Err(AppError::VideoError(format!(
                "vfmcap_open failed: {}",
                last_error_safe(ptr::null_mut())
            )))
        } else {
            Ok(Self { ctx })
        }
    }

    pub fn start(&mut self, num_buffers: u32) -> Result<()> {
        let rc = unsafe { vfmcap_start(self.ctx, num_buffers) };
        if rc == VFMCAP_OK {
            Ok(())
        } else {
            Err(AppError::VideoError(format!(
                "vfmcap_start failed (rc={}): {}",
                rc,
                self.last_error()
            )))
        }
    }

    pub fn stop(&mut self) {
        unsafe { vfmcap_stop(self.ctx) };
    }

    pub fn acquire_frame(&mut self, timeout_ms: i32) -> Result<AcquireResult> {
        let mut frame: VfmcapFrame = unsafe { std::mem::zeroed() };
        let rc = unsafe { vfmcap_acquire_frame(self.ctx, &mut frame, timeout_ms) };
        match rc {
            VFMCAP_OK => {
                let data = frame_to_data(&frame);
                Ok(AcquireResult::Frame(data))
            }
            VFMCAP_RECONFIGURED => {
                let data = frame_to_data(&frame);
                Ok(AcquireResult::Reconfigured(data))
            }
            VFMCAP_ERR_TIMEOUT => Ok(AcquireResult::Timeout),
            VFMCAP_ERR_NOSIG => Ok(AcquireResult::NoSignal),
            _ => Err(AppError::VideoError(format!(
                "vfmcap_acquire_frame failed (rc={}): {}",
                rc,
                self.last_error()
            ))),
        }
    }

    pub fn release_frame(&mut self, index: u32) {
        let mut frame = VfmcapFrame {
            dmabuf_fd: -1,
            dmabuf_fd2: -1,
            index,
            width: 0,
            height: 0,
            bytesperline: 0,
            size: 0,
            pixelformat: 0,
            bitdepth: 0,
            sequence: 0,
            timestamp_us: 0,
            signal_type: 0,
            drm_modifier: 0,
            is_repeated: 0,
            priv_: ptr::null_mut(),
        };
        unsafe { vfmcap_release_frame(self.ctx, &mut frame) };
    }

    pub fn poll_event(&mut self, timeout_ms: i32) -> i32 {
        unsafe { vfmcap_poll_event(self.ctx, timeout_ms) }
    }

    pub fn signal_info(&mut self) -> Result<VfmcapSignalInfo> {
        let mut info: VfmcapSignalInfo = unsafe { std::mem::zeroed() };
        let rc = unsafe { vfmcap_get_signal_info(self.ctx, &mut info) };
        if rc == VFMCAP_OK {
            Ok(info)
        } else {
            Err(AppError::VideoError(format!(
                "vfmcap_get_signal_info failed (rc={}): {}",
                rc,
                self.last_error()
            )))
        }
    }

    pub fn last_error(&self) -> String {
        last_error_safe(self.ctx)
    }
}

impl Drop for VfmcapStream {
    fn drop(&mut self) {
        if !self.ctx.is_null() {
            unsafe { vfmcap_close(self.ctx) };
        }
    }
}

pub struct AmlVfmcapCaptureStream {
    inner: VfmcapStream,
    pending_index: Option<u32>,
    resolution: Resolution,
    format: PixelFormat,
    num_buffers: u32,
}

impl AmlVfmcapCaptureStream {
    pub fn open(
        device: &str,
        output_format: VfmcapOutputFmt,
        target_width: u32,
        target_height: u32,
        target_fps: f32,
        color_mode: VfmcapColorMode,
        num_buffers: u32,
    ) -> Result<Self> {
        let config = VfmcapConfig {
            output_format,
            target_width,
            target_height,
            target_fps,
            color_mode,
        };

        let mut inner = VfmcapStream::open(device, &config)?;
        inner.start(num_buffers)?;

        let resolution = if target_width > 0 && target_height > 0 {
            Resolution::new(target_width, target_height)
        } else {
            match inner.signal_info() {
                Ok(info) => Resolution::new(info.width, info.height),
                Err(_) => Resolution::new(1920, 1080),
            }
        };

        Ok(Self {
            inner,
            pending_index: None,
            resolution,
            format: PixelFormat::Nv12,
            num_buffers,
        })
    }

    pub fn signal_info(&mut self) -> Result<VfmcapSignalInfo> {
        self.inner.signal_info()
    }

    pub fn poll_event(&mut self, timeout_ms: i32) -> i32 {
        self.inner.poll_event(timeout_ms)
    }
}

impl CaptureStream for AmlVfmcapCaptureStream {
    fn next_frame(&mut self) -> io::Result<CaptureResult> {
        if self.pending_index.is_some() {
            self.release_frame(&FrameData::DmaBuf(DmaBufFrame {
                dmabuf_fd: -1,
                dmabuf_fd2: -1,
                width: 0,
                height: 0,
                bytesperline: 0,
                size: 0,
                format: 0,
                sequence: 0,
                timestamp_us: 0,
            }));
        }

        let timeout_ms = 2000;
        match self.inner.acquire_frame(timeout_ms) {
            Ok(AcquireResult::Frame(data)) => {
                let res = Resolution::new(data.width, data.height);
                self.resolution = res;
                self.pending_index = Some(data.index);
                Ok(CaptureResult {
                    frame: FrameData::DmaBuf(DmaBufFrame {
                        dmabuf_fd: data.dmabuf_fd,
                        dmabuf_fd2: data.dmabuf_fd2,
                        width: data.width,
                        height: data.height,
                        bytesperline: data.bytesperline,
                        size: data.size,
                        format: data.pixelformat,
                        sequence: data.sequence,
                        timestamp_us: data.timestamp_us,
                    }),
                    resolution: res,
                    format: PixelFormat::Nv12,
                    reconfigured: false,
                })
            }
            Ok(AcquireResult::Reconfigured(data)) => {
                let res = Resolution::new(data.width, data.height);
                self.resolution = res;
                self.pending_index = Some(data.index);
                Ok(CaptureResult {
                    frame: FrameData::DmaBuf(DmaBufFrame {
                        dmabuf_fd: data.dmabuf_fd,
                        dmabuf_fd2: data.dmabuf_fd2,
                        width: data.width,
                        height: data.height,
                        bytesperline: data.bytesperline,
                        size: data.size,
                        format: data.pixelformat,
                        sequence: data.sequence,
                        timestamp_us: data.timestamp_us,
                    }),
                    resolution: res,
                    format: PixelFormat::Nv12,
                    reconfigured: true,
                })
            }
            Ok(AcquireResult::Timeout) => {
                Err(io::Error::new(io::ErrorKind::TimedOut, "vfmcap timeout"))
            }
            Ok(AcquireResult::NoSignal) => {
                Err(io::Error::new(io::ErrorKind::NotConnected, "no signal"))
            }
            Err(e) => Err(io::Error::new(io::ErrorKind::Other, e.to_string())),
        }
    }

    fn resolution(&self) -> Resolution {
        self.resolution
    }

    fn format(&self) -> PixelFormat {
        self.format
    }

    fn release_frame(&mut self, frame: &FrameData) {
        if let FrameData::DmaBuf(ref dma) = frame {
            if let Some(idx) = self.pending_index.take() {
                self.inner.release_frame(idx);
            }
            let _ = dma;
        }
    }
}

fn frame_to_data(f: &VfmcapFrame) -> VfmcapFrameData {
    VfmcapFrameData {
        dmabuf_fd: f.dmabuf_fd,
        dmabuf_fd2: f.dmabuf_fd2,
        index: f.index,
        width: f.width,
        height: f.height,
        bytesperline: f.bytesperline,
        size: f.size,
        pixelformat: f.pixelformat,
        bitdepth: f.bitdepth,
        sequence: f.sequence,
        timestamp_us: f.timestamp_us,
        signal_type: f.signal_type,
        drm_modifier: f.drm_modifier,
        is_repeated: f.is_repeated != 0,
    }
}

fn last_error_safe(ctx: *mut VfmcapCtx) -> String {
    unsafe {
        let ptr = vfmcap_last_error(ctx);
        if ptr.is_null() {
            "(unknown)".to_string()
        } else {
            CStr::from_ptr(ptr).to_string_lossy().into_owned()
        }
    }
}
