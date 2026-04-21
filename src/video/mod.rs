pub mod capture_trait;
#[cfg(feature = "hwencode")]
pub mod codec_constraints;
#[cfg(feature = "hwencode")]
pub mod convert;
#[cfg(feature = "hwencode")]
pub mod decoder;
#[cfg(feature = "hwencode")]
pub mod device;
pub mod encoder;
pub mod format;
#[cfg(feature = "hwencode")]
pub mod frame;
#[cfg(feature = "hwencode")]
pub mod shared_video_pipeline;
pub mod stream_manager;
#[cfg(feature = "hwencode")]
pub mod streamer;
#[cfg(feature = "hwencode")]
pub mod v4l2r_capture;
#[cfg(feature = "aml")]
pub mod vfmcap_capture;
#[cfg(feature = "aml")]
pub mod aml_pipeline;

#[cfg(feature = "hwencode")]
pub use convert::{PixelConverter, Yuv420pBuffer};
#[cfg(feature = "hwencode")]
pub use device::{VideoDevice, VideoDeviceInfo};
#[cfg(feature = "hwencode")]
pub use frame::VideoFrame;
pub use format::PixelFormat;
#[cfg(feature = "hwencode")]
pub use shared_video_pipeline::{
    EncodedVideoFrame, SharedVideoPipeline, SharedVideoPipelineConfig, SharedVideoPipelineStats,
};
pub use stream_manager::VideoStreamManager;
#[cfg(feature = "hwencode")]
pub use streamer::{Streamer, StreamerState};

#[cfg(feature = "hwencode")]
pub(crate) fn is_rk_hdmirx_driver(driver: &str, card: &str) -> bool {
    driver.eq_ignore_ascii_case("rk_hdmirx") || card.eq_ignore_ascii_case("rk_hdmirx")
}

#[cfg(feature = "hwencode")]
pub(crate) fn is_rk_hdmirx_device(device: &device::VideoDeviceInfo) -> bool {
    is_rk_hdmirx_driver(&device.driver, &device.card)
}

pub(crate) fn is_aml_vfmcap_device() -> bool {
    std::path::Path::new("/dev/video_cap").exists()
}
