//! Voice-Kern ohne Geraete: Encoder (Opus, FEC, DTX), Mixer mit Jitter-Buffer pro Sprecher,
//! Pegel fuer "wer spricht". Alles synchron und deterministisch testbar.

use crate::stream_audio::STREAM_AUDIO_ID;
use crate::{unix_ms32, Header, Receiver, RxSummary, FRAME};
use app_proto::voice::VoiceHeader;
use app_proto::PublicKey;
use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

pub const MIN_KBPS: u32 = 6;
pub const MAX_KBPS: u32 = 510;
pub const DEFAULT_KBPS: u32 = 64;
/// Audio-Stream-ID im Voice-Header (Video/Anderes nutzt andere IDs).
pub const AUDIO_STREAM: u8 = 0;
/// Hoechste Zahl gleichzeitig gemischter Sprecher.
pub const MAX_SPEAKERS: usize = 32;
/// Sprecher ohne Pakete werden nach dieser Zeit entfernt.
const PEER_IDLE: Duration = Duration::from_secs(10);
/// Bei Stille (DTX) mindestens alle so viele Frames ein Lebenszeichen senden.
const DTX_KEEPALIVE_FRAMES: u32 = 20;
/// Pegel (RMS), ab dem jemand als "spricht" gilt.
pub const SPEAKING_LEVEL: f32 = 0.01;

#[derive(Debug, thiserror::Error)]
pub enum VoiceError {
    #[error("Opus: {0}")]
    Opus(#[from] opus::Error),
}

struct Peer {
    rx: Receiver,
    volume: f32,
    level: f32,
    last_packet: Instant,
}

pub struct VoiceCore {
    enc: opus::Encoder,
    kbps: u32,
    dtx: bool,
    seq: u32,
    silent_run: u32,
    muted: bool,
    deafened: bool,
    transmitting: bool,
    input_gain: f32,
    output_volume: f32,
    local_level: f32,
    peers: HashMap<PublicKey, Peer>,
    scratch: Vec<f32>,
    enc_buf: Vec<u8>,
    mix_calls: u32,
    monitor: bool,
    monitor_q: VecDeque<f32>,
    /// Lautstaerke je Geraet; gilt auch, bevor dessen erstes Paket eintrifft.
    volumes: HashMap<PublicKey, f32>,
    /// Signaltoene, werden direkt in den Mix gelegt (unabhaengig von Taub).
    sfx: VecDeque<f32>,
    /// Geraet, dessen Stream-Ton wir hoeren (M3-07); alles andere wird verworfen.
    stream_src: Option<PublicKey>,
    stream_rx: Option<Receiver>,
    stream_vol: f32,
    stream_last: Option<Instant>,
    stream_buf: Vec<f32>,
}

/// Ohne Pakete so lange gilt der Stream-Ton als beendet.
const STREAM_IDLE: Duration = Duration::from_secs(2);

fn rms(x: &[f32]) -> f32 {
    if x.is_empty() {
        return 0.0;
    }
    (x.iter().map(|s| s * s).sum::<f32>() / x.len() as f32).sqrt()
}

/// Weiches Begrenzen oberhalb von 0,9, damit gemischte Sprecher nicht hart uebersteuern.
fn soft_clip(x: f32) -> f32 {
    let a = x.abs();
    if a <= 0.9 {
        x
    } else {
        x.signum() * (0.9 + 0.1 * ((a - 0.9) / 0.1).tanh())
    }
}

impl VoiceCore {
    pub fn new(kbps: u32) -> Result<Self, VoiceError> {
        let mut enc =
            opus::Encoder::new(crate::RATE, opus::Channels::Mono, opus::Application::Voip)?;
        enc.set_inband_fec(true)?;
        enc.set_packet_loss_perc(10)?;
        enc.set_dtx(true)?;
        let mut c = Self {
            enc,
            kbps: 0,
            dtx: true,
            seq: 0,
            silent_run: 0,
            muted: false,
            deafened: false,
            transmitting: true,
            input_gain: 1.0,
            output_volume: 1.0,
            local_level: 0.0,
            peers: HashMap::new(),
            scratch: vec![0.0; FRAME],
            enc_buf: vec![0; 1500],
            mix_calls: 0,
            monitor: false,
            monitor_q: VecDeque::new(),
            volumes: HashMap::new(),
            sfx: VecDeque::new(),
            stream_src: None,
            stream_rx: None,
            stream_vol: 1.0,
            stream_last: None,
            stream_buf: vec![0.0; FRAME * 2],
        };
        c.set_bitrate(kbps)?;
        Ok(c)
    }

    /// Bitrate in kbit/s (6 bis 510); gilt ab dem naechsten Frame.
    pub fn set_bitrate(&mut self, kbps: u32) -> Result<(), VoiceError> {
        let kbps = kbps.clamp(MIN_KBPS, MAX_KBPS);
        self.enc
            .set_bitrate(opus::Bitrate::Bits(kbps as i32 * 1000))?;
        self.kbps = kbps;
        Ok(())
    }

    pub fn bitrate(&self) -> u32 {
        self.kbps
    }

    pub fn set_dtx(&mut self, on: bool) -> Result<(), VoiceError> {
        self.enc.set_dtx(on)?;
        self.dtx = on;
        Ok(())
    }

    pub fn set_muted(&mut self, m: bool) {
        self.muted = m;
    }

    pub fn set_deafened(&mut self, d: bool) {
        self.deafened = d;
    }

    /// Push-to-Talk / Sprachaktivierung: `false` = nichts senden.
    pub fn set_transmitting(&mut self, t: bool) {
        self.transmitting = t;
    }

    pub fn set_input_gain(&mut self, g: f32) {
        self.input_gain = g.clamp(0.0, 4.0);
    }

    pub fn set_output_volume(&mut self, v: f32) {
        self.output_volume = v.clamp(0.0, 2.0);
    }

    /// Lautstaerke 0 bis 2 (0 = stumm) fuer ein Geraet, nur fuer die lokale Wiedergabe.
    pub fn set_peer_volume(&mut self, device: &PublicKey, v: f32) {
        let v = v.clamp(0.0, 2.0);
        self.volumes.insert(*device, v);
        if let Some(p) = self.peers.get_mut(device) {
            p.volume = v;
        }
    }

    /// Legt einen Signalton (48 kHz mono) mit Lautstaerke `vol` in die Wiedergabe.
    pub fn play_sfx(&mut self, samples: &[f32], vol: f32) {
        // Hoechstens 3 s vormerken, damit schnelles Umschalten nichts aufstaut.
        if self.sfx.len() > 3 * crate::RATE as usize {
            self.sfx.clear();
        }
        // Laeuft schon ein Ton, wird der neue dazugemischt.
        for (i, s) in samples.iter().enumerate() {
            let v = s * vol;
            match self.sfx.get_mut(i) {
                Some(x) => *x += v,
                None => self.sfx.push_back(v),
            }
        }
    }

    pub fn sfx_pending(&self) -> bool {
        !self.sfx.is_empty()
    }

    /// "Mich selbst hoeren": verarbeitete Mikrofon-Frames landen im eigenen Mix.
    pub fn set_monitor(&mut self, on: bool) {
        self.monitor = on;
        if !on {
            self.monitor_q.clear();
        }
    }

    pub fn feed_monitor(&mut self, pcm: &[f32]) {
        if self.monitor {
            self.monitor_q.extend(pcm);
            while self.monitor_q.len() > 3 * FRAME {
                self.monitor_q.pop_front();
            }
        }
    }

    /// Ein Frame wurde nicht gesendet (Sprachaktivierung geschlossen): Folgenummer weiterzaehlen,
    /// damit die Gegenseite die Luecke als Stille erkennt.
    pub fn skip_frame(&mut self) {
        self.seq = self.seq.wrapping_add(1);
        self.silent_run = 0;
    }

    /// Kodiert einen 20-ms-Frame (`FRAME` Samples, mono, 48 kHz) zu einem Datagramm.
    /// `None`, wenn nichts gesendet wird (stumm, nicht sendend, DTX-Stille).
    /// Die Folgenummer zaehlt Frames, nicht gesendete Pakete.
    pub fn encode_frame(&mut self, pcm: &[f32]) -> Option<Vec<u8>> {
        debug_assert_eq!(pcm.len(), FRAME);
        let seq = self.seq;
        self.seq = self.seq.wrapping_add(1);
        let g = self.input_gain;
        for (d, s) in self.scratch.iter_mut().zip(pcm) {
            *d = (s * g).clamp(-1.0, 1.0);
        }
        let lvl = rms(&self.scratch);
        self.local_level = lvl.max(self.local_level * 0.8);
        if self.muted || !self.transmitting {
            self.silent_run = 0;
            return None;
        }
        let n = self
            .enc
            .encode_float(&self.scratch, &mut self.enc_buf)
            .ok()?;
        // DTX liefert in der Stille 1-2 Byte grosse Pakete: nur gelegentlich senden.
        if self.dtx && n <= 2 {
            self.silent_run += 1;
            if !self.silent_run.is_multiple_of(DTX_KEEPALIVE_FRAMES) {
                return None;
            }
        } else {
            self.silent_run = 0;
        }
        Some(
            VoiceHeader {
                flags: 0,
                stream_id: AUDIO_STREAM,
                seq,
                ts: unix_ms32(),
            }
            .write(&self.enc_buf[..n]),
        )
    }

    /// Empfangenes Datagramm eines Geraets einsortieren.
    pub fn on_datagram(&mut self, device: &PublicKey, data: &[u8]) {
        let Some((h, payload)) = VoiceHeader::parse(data) else {
            return;
        };
        if h.stream_id == STREAM_AUDIO_ID {
            if self.stream_src.as_ref() != Some(device) || payload.is_empty() {
                return;
            }
            if self.stream_rx.is_none() {
                self.stream_rx = Receiver::new_stereo().ok();
            }
            if let Some(rx) = self.stream_rx.as_mut() {
                self.stream_last = Some(Instant::now());
                rx.push(
                    Header {
                        flags: h.flags,
                        stream_id: h.stream_id,
                        seq: h.seq,
                        ts: h.ts,
                    },
                    payload,
                );
            }
            return;
        }
        if h.stream_id != AUDIO_STREAM || payload.is_empty() {
            return;
        }
        if !self.peers.contains_key(device) {
            if self.peers.len() >= MAX_SPEAKERS {
                return;
            }
            let Ok(rx) = Receiver::new() else { return };
            self.peers.insert(
                *device,
                Peer {
                    rx,
                    volume: self.volumes.get(device).copied().unwrap_or(1.0),
                    level: 0.0,
                    last_packet: Instant::now(),
                },
            );
        }
        let p = self.peers.get_mut(device).expect("eingefuegt");
        p.last_packet = Instant::now();
        p.rx.push(
            Header {
                flags: h.flags,
                stream_id: h.stream_id,
                seq: h.seq,
                ts: h.ts,
            },
            payload,
        );
    }

    /// Stream-Ton dieses Geraets abspielen (`None` = keinen).
    pub fn set_stream_source(&mut self, device: Option<PublicKey>) {
        if self.stream_src != device {
            self.stream_src = device;
            self.stream_rx = None;
            self.stream_last = None;
        }
    }

    /// Lautstaerke des Stream-Tons, getrennt vom Voice (0 bis 1).
    pub fn set_stream_volume(&mut self, v: f32) {
        self.stream_vol = v.clamp(0.0, 1.0);
    }

    /// Medienzeit (Sender-Uhr, ms) des Stream-Tons, der gerade gemischt wird. Nach ihr richtet
    /// sich die Anzeige des Bildes. `None`, solange kein Ton laeuft.
    pub fn stream_position(&self) -> Option<u32> {
        self.stream_rx.as_ref().and_then(Receiver::playing_ts)
    }

    /// Wie `mix`, aber stereo (verschraenkt, `2 * FRAME` Samples): Voice mittig, dazu der
    /// Stream-Ton in Stereo. Taub schaltet auch den Stream-Ton stumm.
    pub fn mix_stereo(&mut self, out: &mut [f32]) {
        debug_assert_eq!(out.len(), FRAME * 2);
        let mut voice = [0.0f32; FRAME];
        self.mix(&mut voice);
        if self.stream_last.is_some_and(|t| t.elapsed() > STREAM_IDLE) {
            self.stream_rx = None;
            self.stream_last = None;
        }
        let mut got = false;
        if let Some(rx) = self.stream_rx.as_mut() {
            got = rx.pull(&mut self.stream_buf, 0.0);
        }
        let g = if self.deafened {
            0.0
        } else {
            self.stream_vol * self.output_volume
        };
        for (i, v) in voice.iter().enumerate() {
            let (l, r) = if got {
                (self.stream_buf[2 * i] * g, self.stream_buf[2 * i + 1] * g)
            } else {
                (0.0, 0.0)
            };
            out[2 * i] = soft_clip(v + l);
            out[2 * i + 1] = soft_clip(v + r);
        }
    }

    pub fn remove_peer(&mut self, device: &PublicKey) {
        self.peers.remove(device);
    }

    pub fn clear_peers(&mut self) {
        self.peers.clear();
    }

    /// Mischt den naechsten 20-ms-Frame aller Sprecher nach `out` (`FRAME` Samples).
    pub fn mix(&mut self, out: &mut [f32]) {
        debug_assert_eq!(out.len(), FRAME);
        out.fill(0.0);
        self.mix_calls = self.mix_calls.wrapping_add(1);
        if self.mix_calls.is_multiple_of(50) {
            self.peers
                .retain(|_, p| p.last_packet.elapsed() < PEER_IDLE);
        }
        let mut tmp = std::mem::take(&mut self.scratch);
        for p in self.peers.values_mut() {
            if p.rx.pull(&mut tmp, 0.0) {
                let lvl = rms(&tmp) * p.volume;
                p.level = lvl.max(p.level * 0.8);
                if !self.deafened {
                    for (o, s) in out.iter_mut().zip(&tmp) {
                        *o += s * p.volume;
                    }
                }
            } else {
                p.level *= 0.8;
            }
        }
        self.scratch = tmp;
        for o in out.iter_mut() {
            match self.sfx.pop_front() {
                Some(v) => *o += v,
                None => break,
            }
        }
        if self.monitor && self.monitor_q.len() >= FRAME {
            for o in out.iter_mut() {
                *o += self.monitor_q.pop_front().unwrap_or(0.0);
            }
        }
        let v = self.output_volume;
        for o in out.iter_mut() {
            *o = soft_clip(*o * v);
        }
    }

    /// (eigener Pegel, Pegel je Sprecher), 0.0 bis 1.0.
    pub fn levels(&self) -> (f32, Vec<(PublicKey, f32)>) {
        (
            self.local_level,
            self.peers.iter().map(|(d, p)| (*d, p.level)).collect(),
        )
    }

    pub fn speaking(&self) -> Vec<PublicKey> {
        self.peers
            .iter()
            .filter(|(_, p)| p.level > SPEAKING_LEVEL)
            .map(|(d, _)| *d)
            .collect()
    }

    pub fn peer_count(&self) -> usize {
        self.peers.len()
    }

    /// Empfangswerte je Sprecher (Diagnose).
    pub fn rx_stats(&self) -> Vec<(PublicKey, RxSummary)> {
        self.peers
            .iter()
            .map(|(d, p)| (*d, p.rx.stats.summary()))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RATE;

    fn tone(freq: f32, frame: usize, amp: f32) -> Vec<f32> {
        (0..FRAME)
            .map(|k| {
                let t = (frame * FRAME + k) as f32 / RATE as f32;
                amp * (2.0 * std::f32::consts::PI * freq * t).sin()
            })
            .collect()
    }

    /// Energie bei `freq` (Goertzel), normiert.
    fn power_at(x: &[f32], freq: f32) -> f32 {
        let w = 2.0 * std::f32::consts::PI * freq / RATE as f32;
        let (mut re, mut im) = (0.0f32, 0.0f32);
        for (i, s) in x.iter().enumerate() {
            re += s * (w * i as f32).cos();
            im += s * (w * i as f32).sin();
        }
        (re * re + im * im).sqrt() / x.len() as f32
    }

    fn run(senders: &[(PublicKey, f32)], frames: usize) -> (VoiceCore, Vec<f32>) {
        let mut rx = VoiceCore::new(64).unwrap();
        let mut encs: Vec<VoiceCore> = senders
            .iter()
            .map(|_| VoiceCore::new(64).unwrap())
            .collect();
        let mut all = Vec::new();
        let mut out = vec![0.0; FRAME];
        for i in 0..frames {
            for ((dev, f), e) in senders.iter().zip(encs.iter_mut()) {
                if let Some(p) = e.encode_frame(&tone(*f, i, 0.2)) {
                    rx.on_datagram(dev, &p);
                }
            }
            if i >= 4 {
                rx.mix(&mut out);
                all.extend_from_slice(&out);
            }
        }
        (rx, all)
    }

    #[test]
    fn mixes_several_speakers_and_reports_levels() {
        let (a, b, c) = ([1u8; 32], [2u8; 32], [3u8; 32]);
        let (rx, pcm) = run(&[(a, 440.0), (b, 880.0), (c, 1320.0)], 60);
        let tail = &pcm[pcm.len() / 2..];
        for f in [440.0, 880.0, 1320.0] {
            assert!(power_at(tail, f) > 0.03, "{f} Hz fehlt");
        }
        assert!(power_at(tail, 2000.0) < 0.01);
        assert!(tail.iter().all(|s| s.abs() <= 1.0));
        assert_eq!(rx.speaking().len(), 3);
        assert_eq!(rx.peer_count(), 3);
    }

    #[test]
    fn per_peer_volume_applies_even_before_first_packet() {
        let (loud, quiet, muted) = ([1u8; 32], [2u8; 32], [3u8; 32]);
        let mut rx = VoiceCore::new(64).unwrap();
        rx.set_peer_volume(&quiet, 0.25);
        rx.set_peer_volume(&muted, 0.0);
        let mut reference = VoiceCore::new(64).unwrap();
        let mut enc: Vec<VoiceCore> = (0..3).map(|_| VoiceCore::new(64).unwrap()).collect();
        let (mut a, mut r) = (Vec::new(), Vec::new());
        let mut out = vec![0.0; FRAME];
        for i in 0..50 {
            for (n, (dev, e)) in [loud, quiet, muted].iter().zip(enc.iter_mut()).enumerate() {
                if let Some(p) = e.encode_frame(&tone(440.0, i, 0.2)) {
                    rx.on_datagram(dev, &p);
                    if n == 0 {
                        reference.on_datagram(dev, &p);
                    }
                }
            }
            if i >= 10 {
                rx.mix(&mut out);
                a.extend_from_slice(&out);
                reference.mix(&mut out);
                r.extend_from_slice(&out);
            }
        }
        // Summe = 1,0 + 0,25 + 0 = 1,25-facher Pegel gegenueber dem lauten Sprecher allein.
        let ratio = power_at(&a, 440.0) / power_at(&r, 440.0);
        assert!((1.2..1.3).contains(&ratio), "Verhaeltnis {ratio}");
    }

    #[test]
    fn sfx_plays_through_mix_even_when_deafened_and_scales_with_volume() {
        let mut c = VoiceCore::new(64).unwrap();
        c.set_deafened(true);
        let tone = crate::sfx::render(crate::sfx::Sound::Join);
        c.play_sfx(&tone, 0.5);
        assert!(c.sfx_pending());
        let mut got = Vec::new();
        let mut out = vec![0.0; FRAME];
        for _ in 0..20 {
            c.mix(&mut out);
            got.extend_from_slice(&out);
        }
        assert!(!c.sfx_pending());
        let peak = got.iter().fold(0.0f32, |m, v| m.max(v.abs()));
        assert!((0.12..0.17).contains(&peak), "Spitze {peak}");
        // Zwei gleichzeitige Toene werden addiert statt abgeschnitten.
        c.play_sfx(&tone, 1.0);
        c.play_sfx(&tone, 1.0);
        c.mix(&mut out);
        assert!(out.iter().any(|v| v.abs() > 0.4));
    }

    #[test]
    fn mixing_many_loud_speakers_never_exceeds_full_scale() {
        let senders: Vec<(PublicKey, f32)> = (0..10u8)
            .map(|i| ([i + 1; 32], 300.0 + 90.0 * i as f32))
            .collect();
        let (_, pcm) = run(&senders, 40);
        assert!(pcm.iter().all(|s| s.abs() <= 1.0));
        assert!(pcm.iter().any(|s| s.abs() > 0.3));
    }

    #[test]
    fn mute_deafen_and_gating() {
        let mut tx = VoiceCore::new(64).unwrap();
        let f = tone(440.0, 0, 0.3);
        assert!(tx.encode_frame(&f).is_some());
        tx.set_muted(true);
        assert!(tx.encode_frame(&f).is_none());
        tx.set_muted(false);
        tx.set_transmitting(false);
        assert!(tx.encode_frame(&f).is_none());
        // Eigener Pegel wird auch ohne Senden angezeigt.
        assert!(tx.levels().0 > 0.05);
        tx.set_transmitting(true);
        assert!(tx.encode_frame(&f).is_some());

        let (_, pcm_open) = run(&[([9; 32], 440.0)], 30);
        assert!(pcm_open.iter().any(|s| s.abs() > 0.05));
        let mut rx = VoiceCore::new(64).unwrap();
        rx.set_deafened(true);
        let mut out = vec![0.0; FRAME];
        for i in 0..30 {
            if let Some(p) = tx.encode_frame(&tone(440.0, i, 0.3)) {
                rx.on_datagram(&[9; 32], &p);
            }
            rx.mix(&mut out);
            assert!(out.iter().all(|s| *s == 0.0));
        }
    }

    #[test]
    fn dtx_suppresses_silence_but_keeps_alive() {
        let mut tx = VoiceCore::new(64).unwrap();
        let silent = vec![0.0f32; FRAME];
        let sent = (0..200)
            .filter(|_| tx.encode_frame(&silent).is_some())
            .count();
        assert!(sent < 40, "{sent} Pakete in 4 s Stille");
        assert!(sent >= 5, "Lebenszeichen fehlen ({sent})");
        // Sprache danach wird wieder durchgehend gesendet.
        let talking = (0..50)
            .filter(|i| tx.encode_frame(&tone(500.0, *i, 0.3)).is_some())
            .count();
        assert!(talking >= 49);
    }

    #[test]
    fn bitrate_is_clamped_and_changes_packet_size() {
        let mut tx = VoiceCore::new(500).unwrap();
        let noise: Vec<f32> = (0..FRAME)
            .map(|i| ((i * 7919) % 200) as f32 / 400.0 - 0.25)
            .collect();
        let size = |tx: &mut VoiceCore| {
            for _ in 0..5 {
                tx.encode_frame(&noise);
            }
            tx.encode_frame(&noise).map_or(0, |p| p.len())
        };
        tx.set_bitrate(1000).unwrap();
        assert_eq!(tx.bitrate(), MAX_KBPS);
        let big = size(&mut tx);
        tx.set_bitrate(8).unwrap();
        let small = size(&mut tx);
        tx.set_bitrate(1).unwrap();
        assert_eq!(tx.bitrate(), MIN_KBPS);
        assert!(big > small * 5, "{big} vs {small}");
        // 510 kbps = 1275 Byte je 20 ms; muss in ein Datagramm passen.
        assert!(big <= 1275 + 10);
    }

    #[test]
    fn loss_is_concealed_and_idle_peer_is_dropped() {
        let dev = [5u8; 32];
        let mut tx = VoiceCore::new(64).unwrap();
        let mut rx = VoiceCore::new(64).unwrap();
        let mut out = vec![0.0; FRAME];
        for i in 0..60 {
            let p = tx.encode_frame(&tone(440.0, i, 0.3)).unwrap();
            if i % 10 != 5 {
                rx.on_datagram(&dev, &p);
            }
            if i >= 4 {
                rx.mix(&mut out);
            }
        }
        assert!(rx.speaking().contains(&dev));
        rx.remove_peer(&dev);
        assert_eq!(rx.peer_count(), 0);
        // Fremde Pakete (falsche Stream-ID, zu kurz) werden ignoriert.
        rx.on_datagram(&dev, &[1, 2, 3]);
        rx.on_datagram(
            &dev,
            &VoiceHeader {
                flags: 0,
                stream_id: 7,
                seq: 1,
                ts: 1,
            }
            .write(&[1, 2]),
        );
        assert_eq!(rx.peer_count(), 0);
    }

    /// CPU-Kosten: Dekodieren und Mischen von 5 Sprechern je Sekunde Audio.
    #[test]
    fn five_speakers_cost_little_cpu() {
        let devs: Vec<PublicKey> = (0..5u8).map(|i| [i + 1; 32]).collect();
        let mut encs: Vec<VoiceCore> = devs.iter().map(|_| VoiceCore::new(64).unwrap()).collect();
        let mut rx = VoiceCore::new(64).unwrap();
        let mut out = vec![0.0; FRAME];
        let frames = 500; // 10 s Audio
        let mut cpu = Duration::ZERO;
        for i in 0..frames {
            let pk: Vec<Vec<u8>> = encs
                .iter_mut()
                .enumerate()
                .map(|(k, e)| {
                    e.encode_frame(&tone(300.0 + 100.0 * k as f32, i, 0.2))
                        .unwrap()
                })
                .collect();
            let t = Instant::now();
            for (d, p) in devs.iter().zip(&pk) {
                rx.on_datagram(d, p);
            }
            rx.mix(&mut out);
            cpu += t.elapsed();
        }
        let pct = cpu.as_secs_f64() / 10.0 * 100.0;
        println!("5 Sprecher: {pct:.2} % eines Kerns (Dekodieren + Mischen, Testbuild)");
        // Debug-Builds sind viel langsamer als Release; hier nur eine grobe Obergrenze.
        assert!(pct < 25.0, "{pct}");
    }
}
