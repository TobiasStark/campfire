//! Mikrofon-Verarbeitung vor dem Encoder: Verstaerkung, Rauschunterdrueckung (RNNoise),
//! automatische Lautstaerke, Sprachaktivierung mit Vorlaufpuffer und Nachlauf.

use crate::FRAME;
use nnnoiseless::DenoiseState;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;

/// Vorlauf bei Sprachaktivierung: so viele 20-ms-Frames vor dem Ausloeser gehen mit raus (120 ms).
pub const PREROLL_FRAMES: usize = 6;
const AGC_TARGET: f32 = 0.1;
const AGC_FLOOR: f32 = 0.003;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct InputSettings {
    pub denoise: bool,
    /// `true`: nur senden, wenn der Pegel ueber der Schwelle liegt.
    pub voice_activation: bool,
    /// Schwelle in dBFS (-80 bis 0).
    pub threshold_db: f32,
    pub agc: bool,
    /// Nachlauf, nachdem der Pegel wieder unter die Schwelle fiel.
    pub hangover_ms: u32,
    /// Eingangsverstaerkung (linear, 0 bis 4).
    pub gain: f32,
}

impl Default for InputSettings {
    fn default() -> Self {
        Self {
            denoise: true,
            voice_activation: false,
            threshold_db: -45.0,
            agc: false,
            hangover_ms: 200,
            gain: 1.0,
        }
    }
}

/// Was mit einem Eingangs-Frame passieren soll (in Reihenfolge ausfuehren).
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// Nicht gesendet; nur die Folgenummer weiterzaehlen.
    Skip,
    /// Kodieren und senden.
    Frame(Vec<f32>),
}

pub struct Processor {
    s: InputSettings,
    dn: Box<DenoiseState<'static>>,
    ring: VecDeque<Vec<f32>>,
    open: bool,
    hang: u32,
    agc: f32,
    level: f32,
    buf: Vec<f32>,
    tmp_in: Vec<f32>,
    tmp_out: Vec<f32>,
}

fn rms(x: &[f32]) -> f32 {
    (x.iter().map(|s| s * s).sum::<f32>() / x.len().max(1) as f32).sqrt()
}

impl Processor {
    pub fn new(s: InputSettings) -> Self {
        Self {
            s,
            dn: DenoiseState::new(),
            ring: VecDeque::new(),
            open: false,
            hang: 0,
            agc: 1.0,
            level: 0.0,
            buf: vec![0.0; FRAME],
            tmp_in: vec![0.0; DenoiseState::FRAME_SIZE],
            tmp_out: vec![0.0; DenoiseState::FRAME_SIZE],
        }
    }

    pub fn set_settings(&mut self, s: InputSettings) {
        self.s = s;
    }

    /// Pegel (RMS, 0 bis 1) des verarbeiteten Signals, langsam abklingend.
    pub fn level(&self) -> f32 {
        self.level
    }

    pub fn is_open(&self) -> bool {
        self.open || !self.s.voice_activation
    }

    pub fn process(&mut self, pcm: &[f32]) -> Vec<Action> {
        debug_assert_eq!(pcm.len(), FRAME);
        self.buf.copy_from_slice(pcm);
        if self.s.denoise {
            let h = DenoiseState::FRAME_SIZE;
            for half in 0..FRAME / h {
                let r = half * h..(half + 1) * h;
                for (d, s) in self.tmp_in.iter_mut().zip(&self.buf[r.clone()]) {
                    *d = s * 32768.0;
                }
                self.dn.process_frame(&mut self.tmp_out, &self.tmp_in);
                for (d, s) in self.buf[r].iter_mut().zip(&self.tmp_out) {
                    *d = s / 32768.0;
                }
            }
        }
        let g = self.s.gain.clamp(0.0, 4.0);
        if (g - 1.0).abs() > f32::EPSILON {
            self.buf.iter_mut().for_each(|s| *s *= g);
        }
        if self.s.agc {
            let r = rms(&self.buf);
            if r > AGC_FLOOR {
                let want = (AGC_TARGET / r).clamp(0.5, 6.0);
                // Schnell leiser, langsam lauter.
                let k = if want < self.agc { 0.3 } else { 0.02 };
                self.agc += (want - self.agc) * k;
            }
            let a = self.agc;
            self.buf.iter_mut().for_each(|s| *s *= a);
        }
        self.buf.iter_mut().for_each(|s| *s = s.clamp(-1.0, 1.0));

        let r = rms(&self.buf);
        self.level = r.max(self.level * 0.8);

        if !self.s.voice_activation {
            self.open = true;
            self.ring.clear();
            return vec![Action::Frame(self.buf.clone())];
        }
        let db = 20.0 * (r + 1e-9).log10();
        let hang_frames = self.s.hangover_ms.div_ceil(20);
        let loud = db > self.s.threshold_db;
        let mut out = Vec::new();
        if loud {
            self.hang = hang_frames;
            if !self.open {
                self.open = true;
                out.extend(self.ring.drain(..).map(Action::Frame));
            }
            out.push(Action::Frame(self.buf.clone()));
        } else if self.open {
            if self.hang > 0 {
                self.hang -= 1;
                out.push(Action::Frame(self.buf.clone()));
            } else {
                self.open = false;
                self.ring.push_back(self.buf.clone());
            }
        } else {
            self.ring.push_back(self.buf.clone());
        }
        while self.ring.len() > PREROLL_FRAMES {
            self.ring.pop_front();
            out.push(Action::Skip);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RATE;

    fn tone(frame: usize, amp: f32) -> Vec<f32> {
        (0..FRAME)
            .map(|k| {
                let t = (frame * FRAME + k) as f32 / RATE as f32;
                amp * (2.0 * std::f32::consts::PI * 300.0 * t).sin()
            })
            .collect()
    }

    fn va() -> InputSettings {
        InputSettings {
            denoise: false,
            voice_activation: true,
            threshold_db: -40.0,
            hangover_ms: 100,
            ..InputSettings::default()
        }
    }

    #[test]
    fn gate_keeps_word_onsets_with_preroll() {
        let mut p = Processor::new(va());
        let mut frames = 0;
        let mut skips = 0;
        // 20 Frames Stille (leises Rauschen), dann Sprache.
        for i in 0..20 {
            for a in p.process(&tone(i, 0.001)) {
                match a {
                    Action::Skip => skips += 1,
                    Action::Frame(_) => frames += 1,
                }
            }
        }
        assert_eq!(frames, 0);
        assert_eq!(skips, 20 - PREROLL_FRAMES);
        let out = p.process(&tone(20, 0.3));
        let sent = out.iter().filter(|a| matches!(a, Action::Frame(_))).count();
        assert_eq!(
            sent,
            PREROLL_FRAMES + 1,
            "Vorlauf >= 100 ms muss mitgesendet werden"
        );
        const { assert!(PREROLL_FRAMES * 20 >= 100) };
        assert!(!out.contains(&Action::Skip));
    }

    #[test]
    fn gate_closes_after_hangover() {
        let mut p = Processor::new(va());
        p.process(&tone(0, 0.3));
        assert!(p.is_open());
        let mut sent = 0;
        for i in 1..30 {
            sent += p
                .process(&tone(i, 0.0005))
                .iter()
                .filter(|a| matches!(a, Action::Frame(_)))
                .count();
        }
        // 100 ms Nachlauf = 5 Frames, danach zu.
        assert_eq!(sent, 5);
        assert!(!p.is_open());
    }

    #[test]
    fn always_on_passes_every_frame_and_applies_gain() {
        let mut p = Processor::new(InputSettings {
            denoise: false,
            gain: 2.0,
            ..InputSettings::default()
        });
        let Action::Frame(f) = &p.process(&tone(0, 0.1))[0] else {
            panic!()
        };
        assert!((rms(f) - 2.0 * rms(&tone(0, 0.1))).abs() < 1e-4);
    }

    #[test]
    fn agc_raises_quiet_speech() {
        let mut p = Processor::new(InputSettings {
            denoise: false,
            agc: true,
            ..InputSettings::default()
        });
        let mut last = 0.0;
        for i in 0..400 {
            if let Action::Frame(f) = &p.process(&tone(i, 0.02))[0] {
                last = rms(f);
            }
        }
        assert!(last > 0.05, "AGC hebt leise Sprache nicht an: {last}");
    }

    #[test]
    fn denoiser_removes_steady_noise_and_costs_little_cpu() {
        // Weisses Rauschen (Luefter) wird deutlich leiser; Messung der Zusatz-CPU.
        let mut seed = 12345u32;
        let mut noise = move || {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            ((seed >> 8) as f32 / 8_388_608.0 - 1.0) * 0.05
        };
        let mut p = Processor::new(InputSettings::default());
        let (mut before, mut after) = (0.0f32, 0.0f32);
        let n = 500;
        let start = std::time::Instant::now();
        for i in 0..n {
            let f: Vec<f32> = (0..FRAME).map(|_| noise()).collect();
            let Action::Frame(o) = &p.process(&f)[0] else {
                panic!()
            };
            if i >= 100 {
                before += rms(&f);
                after += rms(o);
            }
        }
        let used = start.elapsed().as_secs_f64() / (n as f64 * 0.02) * 100.0;
        println!(
            "Rauschunterdrueckung: {used:.2} % eines Kerns, Pegel {:.1} dB",
            20.0 * (after / before).log10()
        );
        assert!(
            after < before * 0.5,
            "Rauschen nur von {before} auf {after}"
        );
        if !cfg!(debug_assertions) {
            assert!(used < 3.0);
        }
    }
}
