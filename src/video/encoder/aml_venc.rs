use std::ffi::CStr;
use std::ptr;

use crate::error::{AppError, Result};
use crate::ffi::multienc::*;

const HEADER_BUF_SIZE: usize = 4096;
const BITSTREAM_BUF_SIZE: usize = 8 * 1024 * 1024;

pub struct AmlVencEncoder {
    handle: VlCodecHandle,
    width: i32,
    height: i32,
    codec: VlCodecId,
    out_buf: Vec<u8>,
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
        let encode_info = VlEncodeInfo {
            width,
            height,
            frame_rate: fps,
            bit_rate: bitrate,
            gop,
            prepend_spspps_to_idr_frames: true,
            img_format: VlImgFormat::Nv12,
            qp_mode: 0,
            force_pic_qp_enable: 0,
            force_pic_qp_i: 0,
            force_pic_qp_p: 0,
            force_pic_qp_b: 0,
            enc_feature_opts: 0,
            intra_refresh_mode: 0,
            intra_refresh_arg: 0,
            profile: 0,
            level: 0,
            frame_rotation: 0,
            frame_mirroring: 0,
            bitstream_buf_sz: 0,
            multi_slice_mode: 0,
            multi_slice_arg: 0,
            cust_gop_qp_delta: 0,
            strict_rc_window: 0,
            strict_rc_skip_thresh: 0,
            bitstream_buf_sz_kb: 0,
            vui_parameters_present_flag: 0,
            video_full_range_flag: 0,
            video_signal_type_present_flag: 0,
            colour_description_present_flag: 0,
            colour_primaries: 0,
            transfer_characteristics: 0,
            matrix_coefficients: 0,
            crop_enable: false,
            crop: CropInfoMulti {
                left: 0,
                top: 0,
                right: 0,
                bottom: 0,
            },
            internal_bit_depth: 0,
            gop_pattern,
            rc_mode,
            lossless_enable: 0,
        };

        let handle =
            unsafe { vl_multi_encoder_init(codec, encode_info, ptr::null_mut()) };
        if handle <= 0 {
            return Err(AppError::VideoError(format!(
                "vl_multi_encoder_init failed (handle={})",
                handle
            )));
        }

        Ok(Self {
            handle,
            width,
            height,
            codec,
            out_buf: vec![0u8; BITSTREAM_BUF_SIZE],
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

        let meta = unsafe {
            vl_multi_encoder_encode(
                self.handle,
                VlFrameType::Auto,
                self.out_buf.as_mut_ptr(),
                &buf_info as *const VlBufferInfo as *mut VlBufferInfo,
                &mut ret_buf,
            )
        };

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

        let meta = unsafe {
            vl_multi_encoder_encode(
                self.handle,
                VlFrameType::Auto,
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
        let mut ret_buf: VlBufferInfo = unsafe { std::mem::zeroed() };
        let buf_info = VlBufferInfo {
            buf_type: VlBufferType::Vmalloc,
            buf_info: VlBufInfoU { in_ptr: [0, 0, 0] },
            buf_stride: 0,
            buf_fmt: VlImgFormat::Nv12,
        };

        unsafe {
            vl_multi_encoder_encode(
                self.handle,
                VlFrameType::Idr,
                self.out_buf.as_mut_ptr(),
                &buf_info as *const VlBufferInfo as *mut VlBufferInfo,
                &mut ret_buf,
            )
        };
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
