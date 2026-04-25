use std::ffi::CStr;
use std::mem::size_of;

use crate::error::{AppError, Result};
use crate::ffi::multienc::*;
use tracing::warn;

const HEADER_BUF_SIZE: usize = 4096;
const BITSTREAM_BUF_SIZE: usize = 8 * 1024 * 1024;

const DMA_BUF_BASE: u64 = b'b' as u64;
const IOC_NRBITS: u64 = 8;
const IOC_TYPEBITS: u64 = 8;
const IOC_SIZEBITS: u64 = 14;
const IOC_NRSHIFT: u64 = 0;
const IOC_TYPESHIFT: u64 = IOC_NRSHIFT + IOC_NRBITS;
const IOC_SIZESHIFT: u64 = IOC_TYPESHIFT + IOC_TYPEBITS;
const IOC_DIRSHIFT: u64 = IOC_SIZESHIFT + IOC_SIZEBITS;
const IOC_WRITE: u64 = 1;
const DMA_BUF_IOCTL_SYNC: libc::c_ulong = ((IOC_WRITE << IOC_DIRSHIFT)
    | (DMA_BUF_BASE << IOC_TYPESHIFT)
    | (size_of::<DmaBufSync>() as u64) << IOC_SIZESHIFT) as libc::c_ulong;
const DMA_BUF_SYNC_READ: u64 = 1 << 0;
const DMA_BUF_SYNC_WRITE: u64 = 2 << 0;
const DMA_BUF_SYNC_END: u64 = 1 << 2;

#[repr(C)]
struct DmaBufSync {
    flags: u64,
}

fn dma_buf_sync(fd: i32, flags: u64, label: &str) {
    if fd < 0 {
        return;
    }

    let mut sync = DmaBufSync { flags };
    let rc = unsafe { libc::ioctl(fd, DMA_BUF_IOCTL_SYNC, &mut sync) };
    if rc != 0 {
        warn!("DMA_BUF_IOCTL_SYNC {} failed on fd {}: rc={}", label, fd, rc);
    }
}

fn dma_buf_sync_write_end(fd: i32) {
    dma_buf_sync(fd, DMA_BUF_SYNC_END | DMA_BUF_SYNC_WRITE, "WRITE_END");
}

fn dma_buf_sync_read_start(fd: i32) {
    dma_buf_sync(fd, DMA_BUF_SYNC_READ, "READ_START");
}

fn dma_buf_sync_read_end(fd: i32) {
    dma_buf_sync(fd, DMA_BUF_SYNC_END | DMA_BUF_SYNC_READ, "READ_END");
}

fn choose_bitstream_buf_sz_kb(width: i32, height: i32) -> i32 {
    let pixels = i64::from(width.max(1)) * i64::from(height.max(1));
    if pixels >= i64::from(3840 * 2160) {
        4096
    } else if pixels >= i64::from(1920 * 1080) {
        2048
    } else {
        1024
    }
}

pub struct AmlVencEncoder {
    handle: VlCodecHandle,
    width: i32,
    height: i32,
    codec: VlCodecId,
    out_buf: Vec<u8>,
    pending_frame_type: VlFrameType,
}

unsafe impl Send for AmlVencEncoder {}

#[derive(Debug)]
pub struct AmlEncodedFrame {
    pub data: Vec<u8>,
    pub is_keyframe: bool,
    pub is_delay: bool,
    pub pts_us: i32,
    pub frame_type: i32,
    pub input_frame_num: i32,
}

impl AmlVencEncoder {
    pub fn new_h265(
        width: i32,
        height: i32,
        fps: i32,
        bitrate: i32,
        gop: i32,
        gop_pattern: i32,
        rc_mode: i32,
    ) -> Result<Self> {
        Self::new(
            VlCodecId::H265,
            width,
            height,
            fps,
            bitrate,
            gop,
            gop_pattern,
            rc_mode,
        )
    }

    pub fn new_h264(
        width: i32,
        height: i32,
        fps: i32,
        bitrate: i32,
        gop: i32,
        gop_pattern: i32,
        rc_mode: i32,
    ) -> Result<Self> {
        Self::new(
            VlCodecId::H264,
            width,
            height,
            fps,
            bitrate,
            gop,
            gop_pattern,
            rc_mode,
        )
    }

    fn new(
        codec: VlCodecId,
        width: i32,
        height: i32,
        fps: i32,
        bitrate: i32,
        gop: i32,
        gop_pattern: i32,
        rc_mode: i32,
    ) -> Result<Self> {
        let mut encode_info: VlEncodeInfo = unsafe { std::mem::zeroed() };
        encode_info.width = width;
        encode_info.height = height;
        encode_info.frame_rate = fps.max(1);
        encode_info.bit_rate = bitrate.saturating_mul(1000);
        encode_info.gop = gop;
        encode_info.prepend_spspps_to_idr_frames = true;
        encode_info.img_format = VlImgFormat::Nv12;
        encode_info.enc_feature_opts = ENABLE_ROI_FEATURE;
        encode_info.internal_bit_depth = 8;
        encode_info.gop_pattern = gop_pattern;
        encode_info.rc_mode = rc_mode;
        encode_info.bitstream_buf_sz_kb = choose_bitstream_buf_sz_kb(width, height);

        let mut qp: QpParam = unsafe { std::mem::zeroed() };
        qp.qp_min = 0;
        qp.qp_max = 51;
        qp.qp_i_base = 30;
        qp.qp_i_min = 0;
        qp.qp_i_max = 51;
        qp.qp_p_base = 30;
        qp.qp_p_min = 0;
        qp.qp_p_max = 51;
        qp.qp_b_base = 30;
        qp.qp_b_min = 0;
        qp.qp_b_max = 51;

        let handle = unsafe { vl_multi_encoder_init(codec, encode_info, &mut qp) };
        if handle <= 0 {
            return Err(AppError::VideoError(format!(
                "vl_multi_encoder_init failed (handle={}, codec={:?}, {}x{}, fps={}, bitrate_kbps={}, gop={}, gop_pattern={}, rc_mode={})",
                handle, codec, width, height, fps, bitrate, gop, gop_pattern, rc_mode
            )));
        }

        Ok(Self {
            handle,
            width,
            height,
            codec,
            out_buf: vec![0u8; BITSTREAM_BUF_SIZE],
            pending_frame_type: VlFrameType::Auto,
        })
    }

    pub fn generate_header(&mut self) -> Result<Vec<u8>> {
        let mut header_buf = vec![0u8; HEADER_BUF_SIZE];
        let mut header_len: std::os::raw::c_uint = 0;

        let meta = unsafe {
            vl_multi_encoder_generate_header(
                self.handle,
                header_buf.as_mut_ptr(),
                &mut header_len,
            )
        };

        if !meta.is_valid {
            return Err(AppError::VideoError(format!(
                "vl_multi_encoder_generate_header failed: err={}",
                meta.err_cod
            )));
        }

        header_buf.truncate(header_len as usize);
        Ok(header_buf)
    }

    pub fn encode_dma(
        &mut self,
        dmabuf_fd: i32,
        dmabuf_fd2: i32,
        num_planes: u32,
        stride: i32,
    ) -> Result<AmlEncodedFrame> {
        let buf_info = VlBufferInfo {
            buf_type: VlBufferType::Dma,
            buf_info: VlBufInfoU {
                dma_info: VlDmaInfo {
                    shared_fd: [dmabuf_fd, dmabuf_fd2, -1],
                    num_planes,
                },
            },
            buf_stride: stride,
            buf_fmt: VlImgFormat::Nv12,
        };

        let mut ret_buf: VlBufferInfo = unsafe { std::mem::zeroed() };
        let frame_type = std::mem::replace(&mut self.pending_frame_type, VlFrameType::Auto);

        dma_buf_sync_write_end(dmabuf_fd);
        dma_buf_sync_read_start(dmabuf_fd);
        if dmabuf_fd2 >= 0 {
            dma_buf_sync_write_end(dmabuf_fd2);
            dma_buf_sync_read_start(dmabuf_fd2);
        }

        let meta = unsafe {
            vl_multi_encoder_encode(
                self.handle,
                frame_type,
                self.out_buf.as_mut_ptr(),
                &buf_info as *const VlBufferInfo as *mut VlBufferInfo,
                &mut ret_buf,
            )
        };

        dma_buf_sync_read_end(dmabuf_fd);
        if dmabuf_fd2 >= 0 {
            dma_buf_sync_read_end(dmabuf_fd2);
        }

        if !meta.is_valid {
            return Err(AppError::VideoError(format!(
                "encode_dma failed: err={}",
                meta.err_cod
            )));
        }

        let is_delay = meta.encoded_data_length_in_bytes == 0;

        Ok(AmlEncodedFrame {
            data: if is_delay {
                Vec::new()
            } else {
                self.out_buf[..meta.encoded_data_length_in_bytes as usize].to_vec()
            },
            is_keyframe: meta.is_key_frame,
            is_delay,
            pts_us: meta.timestamp_us,
            frame_type: meta.extra.frame_type,
            input_frame_num: meta.input_frame_num,
        })
    }

    pub fn encode_raw(&mut self, data: &[u8], stride: i32) -> Result<AmlEncodedFrame> {
        let buf_info = VlBufferInfo {
            buf_type: VlBufferType::Vmalloc,
            buf_info: VlBufInfoU {
                in_ptr: [
                    data.as_ptr() as std::os::raw::c_ulong,
                    0,
                    0,
                ],
            },
            buf_stride: stride,
            buf_fmt: VlImgFormat::Nv12,
        };

        let mut ret_buf: VlBufferInfo = unsafe { std::mem::zeroed() };
        let frame_type = std::mem::replace(&mut self.pending_frame_type, VlFrameType::Auto);

        let meta = unsafe {
            vl_multi_encoder_encode(
                self.handle,
                frame_type,
                self.out_buf.as_mut_ptr(),
                &buf_info as *const VlBufferInfo as *mut VlBufferInfo,
                &mut ret_buf,
            )
        };

        if !meta.is_valid {
            return Err(AppError::VideoError(format!(
                "encode_raw failed: err={}",
                meta.err_cod
            )));
        }

        let is_delay = meta.encoded_data_length_in_bytes == 0;

        Ok(AmlEncodedFrame {
            data: if is_delay {
                Vec::new()
            } else {
                self.out_buf[..meta.encoded_data_length_in_bytes as usize].to_vec()
            },
            is_keyframe: meta.is_key_frame,
            is_delay,
            pts_us: meta.timestamp_us,
            frame_type: meta.extra.frame_type,
            input_frame_num: meta.input_frame_num,
        })
    }

    pub fn set_bitrate(&mut self, bitrate_kbps: i32) -> Result<()> {
        let rc = unsafe { vl_video_encoder_change_bitrate(self.handle, bitrate_kbps) };
        if rc < 0 {
            Err(AppError::VideoError(format!(
                "change_bitrate failed (rc={})",
                rc
            )))
        } else {
            Ok(())
        }
    }

    pub fn set_gop(&mut self, intra_qp: i32, gop_period: i32) -> Result<()> {
        let rc = unsafe { vl_video_encoder_change_gop(self.handle, intra_qp, gop_period) };
        if rc < 0 {
            Err(AppError::VideoError(format!(
                "change_gop failed (rc={})",
                rc
            )))
        } else {
            Ok(())
        }
    }

    pub fn request_keyframe(&mut self) {
        self.pending_frame_type = VlFrameType::Idr;
    }

    pub fn width(&self) -> i32 {
        self.width
    }

    pub fn height(&self) -> i32 {
        self.height
    }

    pub fn codec(&self) -> VlCodecId {
        self.codec
    }

    pub fn version() -> String {
        unsafe {
            let ptr = vl_get_version();
            if ptr.is_null() {
                "(unknown)".to_string()
            } else {
                CStr::from_ptr(ptr).to_string_lossy().into_owned()
            }
        }
    }
}

impl Drop for AmlVencEncoder {
    fn drop(&mut self) {
        if self.handle > 0 {
            unsafe {
                vl_multi_encoder_destroy(self.handle);
            }
        }
    }
}
