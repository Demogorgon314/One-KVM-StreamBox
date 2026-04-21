pub mod atx;
pub mod audio;
pub mod auth;
pub mod config;
pub mod error;
pub mod events;
pub mod extensions;
pub mod hid;
pub mod modules;
pub mod msd;
pub mod otg;
#[cfg(feature = "hwencode")]
pub mod rtsp;
#[cfg(feature = "hwencode")]
pub mod rustdesk;
pub mod state;
pub mod stream;
pub mod update;
pub mod utils;
pub mod video;
pub mod web;
#[cfg(feature = "hwencode")]
pub mod webrtc;

/// Auto-generated secrets module (from secrets.toml at compile time)
pub mod secrets {
    include!(concat!(env!("OUT_DIR"), "/secrets_generated.rs"));
}

pub use error::{AppError, Result};
