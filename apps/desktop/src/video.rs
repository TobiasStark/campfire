//! Video-Unterstuetzung (M3-02): welche Encoder es gibt. Ohne das Feature `video` enthaelt der
//! Build kein ffmpeg; dann ist die Liste leer.

use app_video::select::EncoderInfo;

/// Probiert alle Encoder aus (dauert einige Sekunden; nicht auf dem UI-Thread aufrufen).
pub fn probe() -> Vec<EncoderInfo> {
    #[cfg(feature = "video")]
    {
        app_video::encode::probe().to_vec()
    }
    #[cfg(not(feature = "video"))]
    {
        Vec::new()
    }
}

/// Ob dieser Build ffmpeg enthaelt.
pub const fn built_in() -> bool {
    cfg!(feature = "video")
}
