use std::io;

use crate::video::format::{PixelFormat, Resolution};

/// Metadata for a captured frame.
#[derive(Debug, Clone, Copy)]
pub struct CaptureMeta {
    pub bytes_used: usize,
    pub sequence: u64,
}

#[derive(Debug)]
pub struct DmaBufFrame {
    pub dmabuf_fd: i32,
    pub dmabuf_fd2: i32,
    pub width: u32,
    pub height: u32,
    pub bytesperline: u32,
    pub size: u32,
    pub format: u32,
    pub sequence: u32,
    pub timestamp_us: u64,
}

#[derive(Debug)]
pub enum FrameData {
    Mapped {
        data: Vec<u8>,
        meta: CaptureMeta,
    },
    DmaBuf(DmaBufFrame),
}

#[derive(Debug)]
pub struct CaptureResult {
    pub frame: FrameData,
    pub resolution: Resolution,
    pub format: PixelFormat,
    pub reconfigured: bool,
}

pub trait CaptureStream: Send {
    fn next_frame(&mut self) -> io::Result<CaptureResult>;
    fn resolution(&self) -> Resolution;
    fn format(&self) -> PixelFormat;
    fn release_frame(&mut self, frame: &FrameData);
}
