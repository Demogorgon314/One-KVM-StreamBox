use std::os::raw::{c_char, c_int, c_uint, c_void};

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VfmcapOutputFmt {
    Raw = 0,
    Nv12 = 1,
    Nv21 = 2,
    P010 = 3,
    Nv12Afbc = 4,
    A2b10g10r10Afbc = 5,
    Vyuy10bit = 6,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VfmcapColorMode {
    Passthrough = 0,
    Hdr10ToSdr = 1,
    HlgToSdr = 2,
}

#[repr(C)]
#[derive(Debug, Clone)]
pub struct VfmcapConfig {
    pub output_format: VfmcapOutputFmt,
    pub target_width: c_uint,
    pub target_height: c_uint,
    pub target_fps: f32,
    pub color_mode: VfmcapColorMode,
}

#[repr(C)]
#[derive(Debug, Clone)]
pub struct VfmcapFrame {
    pub dmabuf_fd: c_int,
    pub dmabuf_fd2: c_int,
    pub index: c_uint,
    pub width: c_uint,
    pub height: c_uint,
    pub bytesperline: c_uint,
    pub size: c_uint,
    pub pixelformat: c_uint,
    pub bitdepth: c_uint,
    pub sequence: c_uint,
    pub timestamp_us: u64,
    pub signal_type: c_uint,
    pub drm_modifier: u64,
    pub is_repeated: c_uint,
    pub priv_: *mut c_void,
}

#[repr(C)]
#[derive(Debug, Clone)]
pub struct VfmcapSignalInfo {
    pub width: c_uint,
    pub height: c_uint,
    pub fps: c_uint,
    pub pixelformat: c_uint,
    pub signal_type: c_uint,
    pub hdr_status: c_uint,
    pub is_interlaced: c_uint,
    pub status: c_uint,
    pub bitdepth: c_uint,
}

#[repr(C)]
pub struct VfmcapCtx {
    _opaque: [u8; 0],
}

pub const VFMCAP_OK: c_int = 0;
pub const VFMCAP_ERR_OPEN: c_int = -1;
pub const VFMCAP_ERR_IOCTL: c_int = -2;
pub const VFMCAP_ERR_TIMEOUT: c_int = -3;
pub const VFMCAP_ERR_NOSIG: c_int = -4;
pub const VFMCAP_ERR_VULKAN: c_int = -5;
pub const VFMCAP_ERR_NOMEM: c_int = -6;
pub const VFMCAP_ERR_INVAL: c_int = -7;
pub const VFMCAP_ERR_STATE: c_int = -8;
pub const VFMCAP_RECONFIGURED: c_int = 1;

pub const VFMCAP_EVENT_SOURCE_CHANGE: c_int = 1;
pub const VFMCAP_EVENT_NOSIG: c_int = 2;
pub const VFMCAP_EVENT_TIMEOUT: c_int = 0;
pub const VFMCAP_EVENT_ERROR: c_int = -1;

pub const VFMCAP_SIG_STABLE: c_uint = 0;
pub const VFMCAP_SIG_NOSIG: c_uint = 1;
pub const VFMCAP_SIG_NOTSUP: c_uint = 2;

#[link(name = "vfmcap")]
extern "C" {
    pub fn vfmcap_open(
        device: *const c_char,
        config: *const VfmcapConfig,
    ) -> *mut VfmcapCtx;

    pub fn vfmcap_start(ctx: *mut VfmcapCtx, num_buffers: c_uint) -> c_int;

    pub fn vfmcap_stop(ctx: *mut VfmcapCtx);

    pub fn vfmcap_close(ctx: *mut VfmcapCtx);

    pub fn vfmcap_acquire_frame(
        ctx: *mut VfmcapCtx,
        frame: *mut VfmcapFrame,
        timeout_ms: c_int,
    ) -> c_int;

    pub fn vfmcap_release_frame(ctx: *mut VfmcapCtx, frame: *mut VfmcapFrame);

    pub fn vfmcap_poll_event(ctx: *mut VfmcapCtx, timeout_ms: c_int) -> c_int;

    pub fn vfmcap_get_signal_info(
        ctx: *mut VfmcapCtx,
        info: *mut VfmcapSignalInfo,
    ) -> c_int;

    pub fn vfmcap_last_error(ctx: *mut VfmcapCtx) -> *const c_char;

    pub fn vfmcap_output_size(
        width: c_uint,
        height: c_uint,
        fmt: VfmcapOutputFmt,
    ) -> c_uint;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::{align_of, offset_of, size_of};

    #[test]
    fn vfmcap_config_layout() {
        assert_eq!(size_of::<VfmcapConfig>(), 20);
        assert_eq!(offset_of!(VfmcapConfig, output_format), 0);
        assert_eq!(offset_of!(VfmcapConfig, target_width), 4);
        assert_eq!(offset_of!(VfmcapConfig, target_height), 8);
        assert_eq!(offset_of!(VfmcapConfig, target_fps), 12);
        assert_eq!(offset_of!(VfmcapConfig, color_mode), 16);
    }

    #[test]
    fn vfmcap_frame_layout() {
        assert_eq!(size_of::<VfmcapFrame>(), 72);
        assert_eq!(offset_of!(VfmcapFrame, dmabuf_fd), 0);
        assert_eq!(offset_of!(VfmcapFrame, dmabuf_fd2), 4);
        assert_eq!(offset_of!(VfmcapFrame, index), 8);
        assert_eq!(offset_of!(VfmcapFrame, width), 12);
        assert_eq!(offset_of!(VfmcapFrame, height), 16);
        assert_eq!(offset_of!(VfmcapFrame, bytesperline), 20);
        assert_eq!(offset_of!(VfmcapFrame, size), 24);
        assert_eq!(offset_of!(VfmcapFrame, pixelformat), 28);
        assert_eq!(offset_of!(VfmcapFrame, bitdepth), 32);
        assert_eq!(offset_of!(VfmcapFrame, sequence), 36);
        assert_eq!(offset_of!(VfmcapFrame, timestamp_us), 40);
        assert_eq!(offset_of!(VfmcapFrame, signal_type), 48);
        assert_eq!(offset_of!(VfmcapFrame, drm_modifier), 56);
        assert_eq!(offset_of!(VfmcapFrame, is_repeated), 64);
        assert_eq!(offset_of!(VfmcapFrame, priv_), 72);
    }

    #[test]
    fn vfmcap_signal_info_layout() {
        assert_eq!(size_of::<VfmcapSignalInfo>(), 36);
        assert_eq!(offset_of!(VfmcapSignalInfo, width), 0);
        assert_eq!(offset_of!(VfmcapSignalInfo, height), 4);
        assert_eq!(offset_of!(VfmcapSignalInfo, fps), 8);
        assert_eq!(offset_of!(VfmcapSignalInfo, pixelformat), 12);
        assert_eq!(offset_of!(VfmcapSignalInfo, signal_type), 16);
        assert_eq!(offset_of!(VfmcapSignalInfo, hdr_status), 20);
        assert_eq!(offset_of!(VfmcapSignalInfo, is_interlaced), 24);
        assert_eq!(offset_of!(VfmcapSignalInfo, status), 28);
        assert_eq!(offset_of!(VfmcapSignalInfo, bitdepth), 32);
    }

    #[test]
    fn enum_sizes() {
        assert_eq!(size_of::<VfmcapOutputFmt>(), 4);
        assert_eq!(size_of::<VfmcapColorMode>(), 4);
    }
}
