use std::os::raw::{c_char, c_int, c_long, c_uchar, c_uint, c_void};

pub type VlCodecHandle = c_long;

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VlCodecId {
    None = 0,
    Vp8 = 1,
    H261 = 2,
    H263 = 3,
    H264 = 4,
    H265 = 5,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VlImgFormat {
    None = 0,
    Nv12 = 1,
    Nv21 = 2,
    Yuv420p = 3,
    Yv12 = 4,
    Rgb888 = 5,
    Rgba8888 = 6,
    P010 = 7,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VlFrameType {
    None = 0,
    Auto = 1,
    Idr = 2,
    I = 3,
    P = 4,
    B = 5,
    DroppableP = 6,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VlBufferType {
    Vmalloc = 0,
    Canvas = 1,
    Physical = 2,
    Dma = 3,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NrModeType {
    Disable = 0,
    Spatial = 1,
    Temporal = 2,
    Both = 3,
}

#[repr(C)]
#[derive(Debug, Clone)]
pub struct EncFrameExtraInfo {
    pub frame_type: c_int,
    pub average_qp_value: c_int,
    pub intra_blocks: c_int,
    pub merged_blocks: c_int,
    pub skipped_blocks: c_int,
}

#[repr(C)]
#[derive(Debug, Clone)]
pub struct EncodingMetadata {
    pub encoded_data_length_in_bytes: c_int,
    pub is_key_frame: bool,
    pub timestamp_us: c_int,
    pub is_valid: bool,
    pub extra: EncFrameExtraInfo,
    pub err_cod: c_int,
    pub input_frame_num: c_int,
}

unsafe impl Send for EncodingMetadata {}

#[repr(C)]
#[derive(Debug, Clone)]
pub struct CropInfoMulti {
    pub left: c_int,
    pub top: c_int,
    pub right: c_int,
    pub bottom: c_int,
}

pub const ENABLE_ROI_FEATURE: c_int = 0x1;
pub const ENABLE_PARA_UPDATE: c_int = 0x2;
pub const ENABLE_LONG_TERM_REF: c_int = 0x80;

#[repr(C)]
#[derive(Debug, Clone)]
pub struct VlEncodeInfo {
    pub width: c_int,
    pub height: c_int,
    pub frame_rate: c_int,
    pub bit_rate: c_int,
    pub gop: c_int,
    pub prepend_spspps_to_idr_frames: bool,
    pub img_format: VlImgFormat,
    pub qp_mode: c_int,
    pub force_pic_qp_enable: c_int,
    pub force_pic_qp_i: c_int,
    pub force_pic_qp_p: c_int,
    pub force_pic_qp_b: c_int,
    pub enc_feature_opts: c_int,
    pub intra_refresh_mode: c_int,
    pub intra_refresh_arg: c_int,
    pub profile: c_int,
    pub level: c_int,
    pub frame_rotation: c_uint,
    pub frame_mirroring: c_uint,
    pub bitstream_buf_sz: c_int,
    pub multi_slice_mode: c_int,
    pub multi_slice_arg: c_int,
    pub cust_gop_qp_delta: c_int,
    pub strict_rc_window: c_int,
    pub strict_rc_skip_thresh: c_int,
    pub bitstream_buf_sz_kb: c_int,
    pub vui_parameters_present_flag: c_uchar,
    pub video_full_range_flag: c_uchar,
    pub video_signal_type_present_flag: c_uchar,
    pub colour_description_present_flag: c_uchar,
    pub colour_primaries: c_uchar,
    pub transfer_characteristics: c_uchar,
    pub matrix_coefficients: c_uchar,
    pub crop_enable: bool,
    pub crop: CropInfoMulti,
    pub internal_bit_depth: c_int,
    pub gop_pattern: c_int,
    pub rc_mode: c_int,
    pub lossless_enable: c_int,
}

#[repr(C)]
#[derive(Debug, Clone)]
pub struct VlDmaInfo {
    pub shared_fd: [c_int; 3],
    pub num_planes: c_uint,
}

#[repr(C)]
pub union VlBufInfoU {
    pub dma_info: VlDmaInfo,
    pub in_ptr: [std::os::raw::c_ulong; 3],
    pub canvas: c_uint,
}

#[repr(C)]
#[derive(Debug, Clone)]
pub struct VlBufferInfo {
    pub buf_type: VlBufferType,
    pub buf_info: VlBufInfoU,
    pub buf_stride: c_int,
    pub buf_fmt: VlImgFormat,
}

unsafe impl Send for VlBufferInfo {}

#[repr(C)]
#[derive(Debug, Clone)]
pub struct QpParam {
    pub qp_min: c_int,
    pub qp_max: c_int,
    pub qp_i_base: c_int,
    pub qp_p_base: c_int,
    pub qp_b_base: c_int,
    pub qp_i_min: c_int,
    pub qp_i_max: c_int,
    pub qp_p_min: c_int,
    pub qp_p_max: c_int,
    pub qp_b_min: c_int,
    pub qp_b_max: c_int,
}

#[repr(C)]
#[derive(Debug, Clone)]
pub struct VlParamRuntime {
    pub idr: *mut c_int,
    pub bitrate: c_int,
    pub frame_rate: c_int,
    pub enable_vfr: bool,
    pub min_frame_rate: c_int,
    pub nr_mode: NrModeType,
}

#[link(name = "vpcodec")]
extern "C" {
    pub fn vl_get_version() -> *const c_char;

    pub fn vl_multi_encoder_init(
        codec_id: VlCodecId,
        encode_info: VlEncodeInfo,
        qp_tbl: *mut QpParam,
    ) -> VlCodecHandle;

    pub fn vl_multi_encoder_destroy(handle: VlCodecHandle) -> c_int;

    pub fn vl_multi_encoder_generate_header(
        codec_handle: VlCodecHandle,
        p_header: *mut c_uchar,
        p_length: *mut c_uint,
    ) -> EncodingMetadata;

    pub fn vl_multi_encoder_encode(
        handle: VlCodecHandle,
        frame_type: VlFrameType,
        out: *mut c_uchar,
        in_buffer_info: *mut VlBufferInfo,
        ret_buffer_info: *mut VlBufferInfo,
    ) -> EncodingMetadata;

    pub fn vl_video_encoder_getavgqp(handle: VlCodecHandle, avg_qp: *mut c_int) -> c_int;

    pub fn vl_video_encoder_update_qp_hint(
        handle: VlCodecHandle,
        pq_hint_table: *mut c_uchar,
        size: c_int,
    ) -> c_int;

    pub fn vl_video_encoder_change_bitrate(handle: VlCodecHandle, bit_rate: c_int) -> c_int;

    pub fn vl_video_encoder_change_qp(
        handle: VlCodecHandle,
        min_qp_i: c_int,
        max_qp_i: c_int,
        max_delta_qp: c_int,
        min_qp_p: c_int,
        max_qp_p: c_int,
        min_qp_b: c_int,
        max_qp_b: c_int,
    ) -> c_int;

    pub fn vl_video_encoder_change_gop(
        handle: VlCodecHandle,
        intra_qp: c_int,
        gop_period: c_int,
    ) -> c_int;

    pub fn vl_video_encoder_change_multi_slice(
        handle: VlCodecHandle,
        multi_slice_mode: c_int,
        multi_slice_para: c_int,
    ) -> c_int;

    pub fn vl_video_encoder_longterm_ref(
        handle: VlCodecHandle,
        longterm_ref_flags: c_int,
    ) -> c_int;

    pub fn vl_video_encoder_skip_frame(handle: VlCodecHandle) -> c_int;

    pub fn vl_video_encoder_change_strict_rc(
        handle: VlCodecHandle,
        bitrate_window: c_int,
        skip_threshold: c_int,
    ) -> c_int;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::{offset_of, size_of};

    #[test]
    fn encoding_metadata_layout() {
        let expected_size = 44;
        assert_eq!(
            size_of::<EncodingMetadata>(),
            expected_size,
            "EncodingMetadata size mismatch — bool padding may be wrong"
        );
        assert_eq!(offset_of!(EncodingMetadata, encoded_data_length_in_bytes), 0);
        assert_eq!(offset_of!(EncodingMetadata, is_key_frame), 4);
        assert_eq!(offset_of!(EncodingMetadata, timestamp_us), 8);
        assert_eq!(offset_of!(EncodingMetadata, is_valid), 12);
        assert_eq!(offset_of!(EncodingMetadata, extra), 16);
        assert_eq!(offset_of!(EncodingMetadata, err_cod), 36);
        assert_eq!(offset_of!(EncodingMetadata, input_frame_num), 40);
    }

    #[test]
    fn enc_frame_extra_info_layout() {
        assert_eq!(size_of::<EncFrameExtraInfo>(), 20);
        assert_eq!(offset_of!(EncFrameExtraInfo, frame_type), 0);
        assert_eq!(offset_of!(EncFrameExtraInfo, average_qp_value), 4);
        assert_eq!(offset_of!(EncFrameExtraInfo, intra_blocks), 8);
        assert_eq!(offset_of!(EncFrameExtraInfo, merged_blocks), 12);
        assert_eq!(offset_of!(EncFrameExtraInfo, skipped_blocks), 16);
    }

    #[test]
    fn vl_encode_info_layout() {
        let s = size_of::<VlEncodeInfo>();
        assert!(
            (140..=200).contains(&s),
            "VlEncodeInfo size {} outside expected range — check bool padding",
            s
        );
        assert_eq!(offset_of!(VlEncodeInfo, width), 0);
        assert_eq!(offset_of!(VlEncodeInfo, height), 4);
        assert_eq!(offset_of!(VlEncodeInfo, frame_rate), 8);
        assert_eq!(offset_of!(VlEncodeInfo, bit_rate), 12);
        assert_eq!(offset_of!(VlEncodeInfo, gop), 16);
        assert_eq!(offset_of!(VlEncodeInfo, prepend_spspps_to_idr_frames), 20);
    }

    #[test]
    fn vl_buffer_info_layout() {
        assert_eq!(size_of::<VlBufferType>(), 4);
    }

    #[test]
    fn vl_dma_info_layout() {
        assert_eq!(size_of::<VlDmaInfo>(), 16);
        assert_eq!(offset_of!(VlDmaInfo, shared_fd), 0);
        assert_eq!(offset_of!(VlDmaInfo, num_planes), 12);
    }

    #[test]
    fn vl_buf_info_u_layout() {
        assert!(
            size_of::<VlBufInfoU>() >= size_of::<VlDmaInfo>(),
            "VlBufInfoU must be at least as large as VlDmaInfo"
        );
    }

    #[test]
    fn crop_info_layout() {
        assert_eq!(size_of::<CropInfoMulti>(), 16);
    }

    #[test]
    fn qp_param_layout() {
        assert_eq!(size_of::<QpParam>(), 44);
    }

    #[test]
    fn vl_param_runtime_layout() {
        let s = size_of::<VlParamRuntime>();
        assert!(
            (24..=32).contains(&s),
            "VlParamRuntime size {} outside expected range — check bool padding",
            s
        );
    }
}
