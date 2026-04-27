#[cfg(feature = "hwencode")]
use hwcodec::common::DataFormat;
#[cfg(feature = "hwencode")]
use hwcodec::ffmpeg_ram::CodecInfo;

#[cfg(feature = "hwencode")]
pub mod codec;
#[cfg(feature = "hwencode")]
pub mod h264;
#[cfg(feature = "hwencode")]
pub mod h265;
#[cfg(feature = "hwencode")]
pub mod jpeg;
#[cfg(feature = "hwencode")]
pub mod registry;
#[cfg(feature = "hwencode")]
pub mod self_check;
pub mod traits;
#[cfg(feature = "hwencode")]
pub mod vp8;
#[cfg(feature = "hwencode")]
pub mod vp9;
#[cfg(feature = "aml")]
pub mod aml_venc;

pub use traits::{
    BitratePreset, EncodedFormat, EncodedFrame, Encoder, EncoderConfig, EncoderFactory, GopPreset,
};

#[cfg(feature = "hwencode")]
pub use codec::{CodecFrame, VideoCodec, VideoCodecConfig, VideoCodecFactory, VideoCodecType};

#[cfg(feature = "hwencode")]
pub use registry::{AvailableEncoder, EncoderBackend, EncoderRegistry, VideoEncoderType};
#[cfg(feature = "hwencode")]
pub use self_check::{
    build_hardware_self_check_runtime_error, run_hardware_self_check, VideoEncoderSelfCheckCell,
    VideoEncoderSelfCheckCodec, VideoEncoderSelfCheckResponse, VideoEncoderSelfCheckRow,
};

#[cfg(feature = "hwencode")]
pub use h264::{H264Config, H264Encoder, H264EncoderType, H264InputFormat};

#[cfg(feature = "hwencode")]
pub use h265::{H265Config, H265Encoder, H265EncoderType, H265InputFormat};

#[cfg(feature = "hwencode")]
pub use vp8::{VP8Config, VP8Encoder, VP8EncoderType, VP8InputFormat};

#[cfg(feature = "hwencode")]
pub use vp9::{VP9Config, VP9Encoder, VP9EncoderType, VP9InputFormat};

#[cfg(feature = "hwencode")]
pub use jpeg::JpegEncoder;

#[cfg(feature = "hwencode")]
pub(crate) fn select_codec_for_format<F>(
    encoders: &[CodecInfo],
    format: DataFormat,
    preferred: F,
) -> Option<&CodecInfo>
where
    F: Fn(&CodecInfo) -> bool,
{
    encoders
        .iter()
        .find(|codec| codec.format == format && preferred(codec))
        .or_else(|| encoders.iter().find(|codec| codec.format == format))
}

#[cfg(feature = "hwencode")]
pub(crate) fn detect_best_codec_for_format<T, F>(
    encoders: &[CodecInfo],
    format: DataFormat,
    preferred: F,
) -> Option<(T, String)>
where
    T: From<EncoderBackend>,
    F: Fn(&CodecInfo) -> bool,
{
    select_codec_for_format(encoders, format, preferred).map(|codec| {
        (
            T::from(EncoderBackend::from_codec_name(&codec.name)),
            codec.name.clone(),
        )
    })
}
