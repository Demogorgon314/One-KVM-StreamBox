pub mod capture_trait;
#[cfg(feature = "hwencode")]
pub mod codec_constraints;
#[cfg(feature = "hwencode")]
pub mod convert;
#[cfg(feature = "hwencode")]
pub mod decoder;
#[cfg(all(feature = "hwencode", feature = "v4l2"))]
#[path = "device.rs"]
pub mod device;
#[cfg(all(feature = "hwencode", feature = "aml", not(feature = "v4l2")))]
#[path = "device_aml.rs"]
pub mod device;
pub mod encoder;
pub mod format;
#[cfg(feature = "hwencode")]
pub mod frame;
#[cfg(all(feature = "hwencode", feature = "v4l2"))]
#[path = "shared_video_pipeline.rs"]
pub mod shared_video_pipeline;
#[cfg(all(feature = "hwencode", feature = "aml", not(feature = "v4l2")))]
#[path = "shared_video_pipeline_aml.rs"]
pub mod shared_video_pipeline;
pub mod stream_manager;
#[cfg(all(feature = "hwencode", feature = "v4l2"))]
#[path = "streamer.rs"]
pub mod streamer;
#[cfg(all(feature = "hwencode", feature = "aml", not(feature = "v4l2")))]
#[path = "streamer_aml.rs"]
pub mod streamer;
#[cfg(all(feature = "hwencode", feature = "v4l2"))]
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
