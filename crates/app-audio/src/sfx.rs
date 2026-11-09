//! Kurze Signaltoene (Join, Leave, Stumm, Taub, PTT, Klingeln), selbst erzeugt: keine Dateien.

use crate::RATE;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Sound {
    Join,
    Leave,
    Mute,
    Unmute,
    Deafen,
    Undeafen,
    PttOn,
    PttOff,
    Ring,
}

pub const ALL: [Sound; 9] = [
    Sound::Join,
    Sound::Leave,
    Sound::Mute,
    Sound::Unmute,
    Sound::Deafen,
    Sound::Undeafen,
    Sound::PttOn,
    Sound::PttOff,
    Sound::Ring,
];

/// (Frequenz in Hz, Dauer in ms); Frequenz 0 = Pause.
fn notes(s: Sound) -> &'static [(f32, u32)] {
    match s {
        Sound::Join => &[(660.0, 70), (880.0, 90)],
        Sound::Leave => &[(880.0, 70), (587.0, 90)],
        Sound::Mute => &[(494.0, 60), (370.0, 80)],
        Sound::Unmute => &[(370.0, 60), (494.0, 80)],
        Sound::Deafen => &[(494.0, 55), (370.0, 55), (277.0, 80)],
        Sound::Undeafen => &[(277.0, 55), (370.0, 55), (494.0, 80)],
        Sound::PttOn => &[(1100.0, 35)],
        Sound::PttOff => &[(740.0, 35)],
        Sound::Ring => &[
            (880.0, 160),
            (660.0, 160),
            (0.0, 80),
            (880.0, 160),
            (660.0, 160),
        ],
    }
}

/// Rendert den Ton als 48-kHz-Mono-Samples (Spitzenpegel ca. 0,3; weiche An-/Abklingphase).
pub fn render(s: Sound) -> Vec<f32> {
    let mut out = Vec::new();
    for &(f, ms) in notes(s) {
        let n = (RATE as usize * ms as usize) / 1000;
        let fade = (RATE as usize * 6 / 1000).min(n / 2).max(1);
        for i in 0..n {
            let env = if i < fade {
                i as f32 / fade as f32
            } else if i + fade > n {
                (n - i) as f32 / fade as f32
            } else {
                1.0
            };
            let v = if f == 0.0 {
                0.0
            } else {
                0.3 * env * (2.0 * std::f32::consts::PI * f * i as f32 / RATE as f32).sin()
            };
            out.push(v);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tones_are_short_quiet_click_free_and_distinct() {
        let mut seen: Vec<Vec<f32>> = Vec::new();
        for s in ALL {
            let x = render(s);
            let ms = x.len() * 1000 / RATE as usize;
            assert!((30..=900).contains(&ms), "{s:?}: {ms} ms");
            assert!(x.iter().all(|v| v.abs() <= 0.31));
            assert!(x.iter().any(|v| v.abs() > 0.25), "{s:?} zu leise");
            assert!(
                x[0].abs() < 0.02 && x[x.len() - 1].abs() < 0.02,
                "{s:?} knackt"
            );
            assert!(!seen.contains(&x), "{s:?} doppelt");
            seen.push(x);
        }
    }
}
