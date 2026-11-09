//! Stream-Ton (M3-07): Spielsound als Opus-Stereo, getrennt vom Voice.
//!
//! Das Paketformat ist das der Voice-Datagramme, aber mit `stream_id` [`STREAM_AUDIO_ID`]. Der
//! Zeitstempel `ts` ist hier nicht die Uhrzeit, sondern Millisekunden auf der Medienuhr des
//! Streams – dieselbe, die im Video-Frame-Kopf steht. So kann der Zuschauer Bild und Ton
//! gegeneinander ausrichten.

use crate::FRAME;
use app_proto::voice::VoiceHeader;

/// `stream_id` im Voice-Header fuer den Ton eines Streams (0 = Voice).
pub const STREAM_AUDIO_ID: u8 = 1;
/// Samples je Frame: 20 ms, zwei Kanaele, verschraenkt.
pub const STEREO_FRAME: usize = FRAME * 2;
pub const MIN_KBPS: u32 = 96;
pub const MAX_KBPS: u32 = 192;
pub const DEFAULT_KBPS: u32 = 160;

pub struct StreamEncoder {
    enc: opus::Encoder,
    seq: u32,
    buf: Vec<u8>,
    kbps: u32,
}

impl StreamEncoder {
    /// `kbps` wird auf 96 bis 192 begrenzt.
    pub fn new(kbps: u32) -> Result<Self, opus::Error> {
        let kbps = kbps.clamp(MIN_KBPS, MAX_KBPS);
        let mut enc = opus::Encoder::new(
            crate::RATE,
            opus::Channels::Stereo,
            opus::Application::Audio,
        )?;
        enc.set_bitrate(opus::Bitrate::Bits(kbps as i32 * 1000))?;
        enc.set_inband_fec(true)?;
        enc.set_packet_loss_perc(5)?;
        Ok(Self {
            enc,
            seq: 0,
            buf: vec![0; 1500],
            kbps,
        })
    }

    pub fn kbps(&self) -> u32 {
        self.kbps
    }

    /// Kodiert 20 ms Stereo (`STEREO_FRAME` verschraenkte Samples). `ts_ms`: Medienuhr des Streams.
    pub fn encode(&mut self, pcm: &[f32], ts_ms: u32) -> Option<Vec<u8>> {
        if pcm.len() != STEREO_FRAME {
            return None;
        }
        let n = self.enc.encode_float(pcm, &mut self.buf).ok()?;
        let seq = self.seq;
        self.seq = self.seq.wrapping_add(1);
        Some(
            VoiceHeader {
                flags: 0,
                stream_id: STREAM_AUDIO_ID,
                seq,
                ts: ts_ms,
            }
            .write(&self.buf[..n]),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Header, Receiver};

    fn tone(frame: usize) -> Vec<f32> {
        let mut v = Vec::with_capacity(STEREO_FRAME);
        for k in 0..FRAME {
            let t = (frame * FRAME + k) as f32 / crate::RATE as f32;
            // Links 440 Hz, rechts 880 Hz: die Kanaele muessen getrennt bleiben.
            v.push(0.4 * (2.0 * std::f32::consts::PI * 440.0 * t).sin());
            v.push(0.4 * (2.0 * std::f32::consts::PI * 880.0 * t).sin());
        }
        v
    }

    fn zero_crossings(x: impl Iterator<Item = f32>) -> usize {
        let mut prev = 0.0f32;
        let mut n = 0;
        for s in x {
            if (prev < 0.0) != (s < 0.0) {
                n += 1;
            }
            prev = s;
        }
        n
    }

    #[test]
    fn stereo_roundtrip_keeps_channels_apart_and_timestamps() {
        let mut enc = StreamEncoder::new(160).unwrap();
        let mut rx = Receiver::new_stereo().unwrap();
        let mut out = vec![0.0; STEREO_FRAME];
        let (mut left, mut right) = (Vec::new(), Vec::new());
        let mut played = 0;
        for i in 0..40usize {
            let pkt = enc.encode(&tone(i), 1000 + i as u32 * 20).unwrap();
            let (h, payload) = VoiceHeader::parse(&pkt).unwrap();
            assert_eq!((h.stream_id, h.seq), (STREAM_AUDIO_ID, i as u32));
            rx.push(
                Header {
                    flags: h.flags,
                    stream_id: h.stream_id,
                    seq: h.seq,
                    ts: h.ts,
                },
                payload,
            );
            if i >= 3 && rx.pull(&mut out, 0.0) {
                played += 1;
                if played > 5 {
                    left.extend(out.iter().step_by(2).copied());
                    right.extend(out.iter().skip(1).step_by(2).copied());
                }
            }
        }
        assert!(played >= 30, "nur {played} Frames gespielt");
        // 440 Hz -> ~880 Nulldurchgaenge/s, 880 Hz -> ~1760/s.
        let secs = left.len() as f32 / crate::RATE as f32;
        let (zl, zr) = (
            zero_crossings(left.iter().copied()) as f32 / secs,
            zero_crossings(right.iter().copied()) as f32 / secs,
        );
        assert!((800.0..960.0).contains(&zl), "links {zl}");
        assert!((1650.0..1870.0).contains(&zr), "rechts {zr}");
        // Die Abspielposition folgt dem Zeitstempel des Senders.
        let pos = rx.playing_ts().unwrap();
        assert!((1000..1800).contains(&pos), "{pos}");
    }

    #[test]
    fn bitrate_is_limited_and_wrong_sizes_are_rejected() {
        assert_eq!(StreamEncoder::new(10).unwrap().kbps(), MIN_KBPS);
        assert_eq!(StreamEncoder::new(900).unwrap().kbps(), MAX_KBPS);
        let mut e = StreamEncoder::new(128).unwrap();
        assert!(e.encode(&[0.0; 100], 0).is_none());
        // Ein Paket bei 160 kbit/s ist etwa 400 Byte gross, nie ueber ein Datagramm.
        let big = e.encode(&tone(0), 0).unwrap();
        assert!(big.len() < 1200, "{}", big.len());
    }

    #[test]
    fn core_plays_stream_audio_only_from_the_chosen_device_and_follows_volume_and_deafen() {
        use crate::voice::VoiceCore;
        let (a, b) = ([1u8; 32], [2u8; 32]);
        let mut core = VoiceCore::new(64).unwrap();
        let mut enc = StreamEncoder::new(160).unwrap();
        let mut out = vec![0.0; STEREO_FRAME];
        let mut peak = |core: &mut VoiceCore, from: [u8; 32], base: usize| {
            let mut p = 0.0f32;
            for i in 0..20usize {
                let pkt = enc.encode(&tone(base + i), (base + i) as u32 * 20).unwrap();
                core.on_datagram(&from, &pkt);
                core.mix_stereo(&mut out);
                if i > 10 {
                    p = p.max(out.iter().fold(0.0, |m, x| f32::max(m, x.abs())));
                }
            }
            p
        };
        // Ohne gewaehlte Quelle bleibt es still.
        assert_eq!(peak(&mut core, a, 0), 0.0);
        core.set_stream_source(Some(a));
        // Falsches Geraet: still.
        assert_eq!(peak(&mut core, b, 20), 0.0);
        // Richtiges Geraet: Ton, und die Position folgt dem Zeitstempel.
        assert!(peak(&mut core, a, 40) > 0.2);
        assert!(core.stream_position().unwrap() >= 40 * 20);
        // Lautstaerke getrennt vom Voice.
        core.set_stream_volume(0.5);
        let half = peak(&mut core, a, 60);
        assert!((0.15..0.25).contains(&half), "{half}");
        // Taub = auch der Stream ist stumm.
        core.set_deafened(true);
        assert_eq!(peak(&mut core, a, 80), 0.0);
        core.set_deafened(false);
        // Quelle abwaehlen beendet den Ton.
        core.set_stream_source(None);
        assert_eq!(core.stream_position(), None);
    }
}
