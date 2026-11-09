//! Video- und Capture-Funktionen (Spike).

#[cfg(feature = "capture")]
pub mod capture;
#[cfg(feature = "ffmpeg")]
pub mod decode;
#[cfg(feature = "ffmpeg")]
pub mod encode;
pub mod frame;
pub mod loopback;
pub mod select;
pub mod thumb;
