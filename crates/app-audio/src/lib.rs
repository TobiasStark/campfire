//! Audio-Spike (M0-03): Paketformat, adaptiver Jitter-Buffer mit Opus-FEC/PLC.

pub mod io;
pub mod process;
pub mod service;
pub mod sfx;
pub mod stream_audio;
pub mod voice;

use std::{
    collections::BTreeMap,
    time::{Instant, SystemTime, UNIX_EPOCH},
};

pub const RATE: u32 = 48_000;
/// 20 ms bei 48 kHz, mono.
pub const FRAME: usize = 960;
pub const HEADER_LEN: usize = 10;

/// `[flags u8][stream_id u8][seq u32][ts u32][opus…]` (siehe docs/protocol.md).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    pub flags: u8,
    pub stream_id: u8,
    pub seq: u32,
    /// Aufnahmezeitpunkt des Frames: Unix-Millisekunden (untere 32 Bit).
    pub ts: u32,
}

impl Header {
    pub fn write(&self, payload: &[u8]) -> Vec<u8> {
        let mut v = Vec::with_capacity(HEADER_LEN + payload.len());
        v.push(self.flags);
        v.push(self.stream_id);
        v.extend_from_slice(&self.seq.to_le_bytes());
        v.extend_from_slice(&self.ts.to_le_bytes());
        v.extend_from_slice(payload);
        v
    }

    pub fn parse(data: &[u8]) -> Option<(Header, &[u8])> {
        if data.len() < HEADER_LEN {
            return None;
        }
        Some((
            Header {
                flags: data[0],
                stream_id: data[1],
                seq: u32::from_le_bytes(data[2..6].try_into().ok()?),
                ts: u32::from_le_bytes(data[6..10].try_into().ok()?),
            },
            &data[HEADER_LEN..],
        ))
    }
}

pub fn unix_ms32() -> u32 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u32)
        .unwrap_or(0)
}

#[derive(Debug, Default, Clone)]
pub struct RxStats {
    pub received: u64,
    pub late: u64,
    pub plc: u64,
    pub fec: u64,
    pub accel: u64,
    pub rebuffer: u64,
    pub first_seq: Option<u32>,
    pub max_seq: u32,
    pub latency_ms: Vec<f64>,
    pub jitter_ms: f64,
    pub target: usize,
}

impl RxStats {
    pub fn lost_on_wire(&self) -> u64 {
        match self.first_seq {
            Some(f) => (self.max_seq.wrapping_sub(f) as u64 + 1).saturating_sub(self.received),
            None => 0,
        }
    }
}

#[derive(PartialEq, Eq, Clone, Copy)]
enum State {
    Buffering,
    Playing,
}

/// Kleinste Puffertiefe in Frames (60 ms): genug fuer normales Internet-Jitter.
pub const MIN_TARGET: usize = 3;
/// Groesste Puffertiefe in Frames (240 ms).
pub const MAX_TARGET: usize = 12;
/// Pakete mit hoechstens so vielen Bytes sind Opus-DTX-Stille (Lebenszeichen).
const DTX_MAX_LEN: usize = 2;
/// Luecken bis zu dieser Laenge gelten als Verlust (PLC); laengere als Sendepause.
const MAX_CONCEAL_GAP: u32 = 3;
/// Nach so vielen Frames ohne Pakete wird neu gepuffert (Sprecher pausiert).
const MAX_PLC_RUN: u32 = 5;
/// Abklingfaktor des Jitter-Spitzenwerts je Paket (Halbwertszeit ca. 14 s bei 50 Paketen/s).
const PEAK_DECAY: f64 = 0.999;
/// Unterhalb dieses Pegels darf ein Frame beim Aufholen verworfen werden (hoerbar nur als Stille).
const QUIET_RMS: f32 = 0.004;

/// Empfänger für einen Sprecher: Jitter-Buffer + Opus-Decoder.
pub struct Receiver {
    map: BTreeMap<u32, (u32, Vec<u8>)>, // seq -> (ts, opus)
    next: u32,
    state: State,
    dec: opus::Decoder,
    plc_run: u32,
    jitter: f64,
    /// Langsam abklingender Spitzenwert des Jitters: kurze Ausreisser vergroessern den Puffer
    /// sofort, er schrumpft aber erst nach einer ruhigen Weile wieder.
    peak: f64,
    prev: Option<(f64, f64)>, // (arrival_ms, ts_ms)
    t0: Instant,
    pub stats: RxStats,
    scratch: Vec<f32>,
    /// Zeitstempel des zuletzt abgespielten Frames (Stream-Ton: Medienuhr des Senders).
    last_ts: Option<u32>,
    /// Wie viele `pull`s beim Puffern schon ein Paket wartet (kurze Laute nicht festhalten).
    wait: usize,
}

impl Receiver {
    pub fn new() -> Result<Self, opus::Error> {
        Self::with_channels(opus::Channels::Mono)
    }

    /// Stereo-Empfaenger fuer den Stream-Ton: `pull` liefert dann `2 * FRAME` verschraenkte Samples.
    pub fn new_stereo() -> Result<Self, opus::Error> {
        Self::with_channels(opus::Channels::Stereo)
    }

    fn with_channels(ch: opus::Channels) -> Result<Self, opus::Error> {
        let n = if ch == opus::Channels::Stereo { 2 } else { 1 };
        Ok(Self {
            map: BTreeMap::new(),
            next: 0,
            state: State::Buffering,
            dec: opus::Decoder::new(RATE, ch)?,
            plc_run: 0,
            jitter: 0.0,
            peak: 0.0,
            prev: None,
            t0: Instant::now(),
            stats: RxStats {
                target: MIN_TARGET,
                ..Default::default()
            },
            scratch: vec![0.0; FRAME * n],
            last_ts: None,
            wait: 0,
        })
    }

    /// Zeitstempel des Frames, der zuletzt aus `pull` kam (None, solange nichts spielt).
    pub fn playing_ts(&self) -> Option<u32> {
        self.last_ts
    }

    /// Zieltiefe in Frames aus dem Jitter-Schätzer (RFC 3550): 2·Spitzen-Jitter + 10 ms,
    /// `MIN_TARGET..=MAX_TARGET` Frames.
    fn target(&self) -> usize {
        (((2.0 * self.peak + 10.0) / 20.0).ceil() as usize).clamp(MIN_TARGET, MAX_TARGET)
    }

    /// Erste Folgenummer des juengsten zusammenhaengenden Abschnitts im Puffer. Was davor liegt,
    /// stammt aus einem frueheren Satz und wuerde nur eine Pause mitten in den Anfang legen.
    fn run_start(&self) -> u32 {
        let mut start = *self.map.keys().next().unwrap_or(&0);
        let mut prev = start;
        for &k in self.map.keys().skip(1) {
            if k.wrapping_sub(prev) > MAX_CONCEAL_GAP + 1 {
                start = k;
            }
            prev = k;
        }
        start
    }

    pub fn push(&mut self, h: Header, payload: &[u8]) {
        self.stats.received += 1;
        if self.stats.first_seq.is_none() {
            self.stats.first_seq = Some(h.seq);
            self.stats.max_seq = h.seq;
        }
        if (h.seq.wrapping_sub(self.stats.max_seq) as i32) > 0 {
            self.stats.max_seq = h.seq;
        }
        // Jitter nach RFC 3550 (in ms)
        let arrival = self.t0.elapsed().as_secs_f64() * 1000.0;
        let ts = h.ts as f64;
        if let Some((pa, pt)) = self.prev {
            let mut dt = ts - pt;
            // Wrap der 32-Bit-Zeit abfangen
            if dt.abs() > 2_147_483_648.0 {
                dt -= (dt / 4_294_967_296.0).round() * 4_294_967_296.0;
            }
            let d = ((arrival - pa) - dt).abs();
            self.jitter += (d - self.jitter) / 16.0;
        }
        self.prev = Some((arrival, ts));
        self.peak = self.jitter.max(self.peak * PEAK_DECAY);
        self.stats.jitter_ms = self.jitter;

        if self.state == State::Playing {
            let diff = h.seq.wrapping_sub(self.next) as i32;
            if diff < 0 {
                self.stats.late += 1;
                // Kam zu spaet zum Abspielen: Puffer war zu knapp, Ziel um einen Frame anheben.
                if diff > -50 {
                    let raised = 10.0 * self.target() as f64 + 5.0;
                    self.peak = (self.peak + 10.0).max(raised).min(MAX_TARGET as f64 * 10.0);
                }
                return;
            }
            if diff > 100 {
                // Sender hat neu angefangen / große Lücke: neu synchronisieren
                self.map.clear();
                self.state = State::Buffering;
                self.wait = 0;
            }
        } else if payload.len() <= DTX_MAX_LEN {
            // Beim Puffern sind DTX-Lebenszeichen nur Stille; sie wuerden den naechsten Satz
            // um eine Pause verzoegern.
            return;
        }
        // Nie unbegrenzt anstauen.
        while self.map.len() >= 4 * MAX_TARGET {
            self.map.pop_first();
        }
        self.map.insert(h.seq, (h.ts, payload.to_vec()));
    }

    /// Dekodiert das Paket `seq` nach `out` und holt bei zu vollem Puffer auf.
    fn play(&mut self, seq: u32, ts: u32, p: &[u8], out: &mut [f32], queued_ms: f64) -> bool {
        let ok = self.dec.decode_float(p, out, false).is_ok();
        self.plc_run = 0;
        self.next = seq.wrapping_add(1);
        self.last_ts = Some(ts);
        let lat = unix_ms32().wrapping_sub(ts) as i32 as f64 + queued_ms;
        if (-1000.0..10_000.0).contains(&lat) {
            self.stats.latency_ms.push(lat);
            if self.stats.latency_ms.len() > 4096 {
                self.stats.latency_ms.drain(..2048);
            }
        }
        // Zu viel gepuffert (Jitter ist gesunken): einen Frame ueberspringen, bevorzugt in
        // einer leisen Stelle, damit man das Aufholen nicht hoert.
        let target = self.stats.target;
        let quiet = {
            let e: f32 = out.iter().map(|s| s * s).sum::<f32>() / out.len().max(1) as f32;
            e.sqrt() < QUIET_RMS
        };
        if self.map.len() > target + 2 && (quiet || self.map.len() > 2 * target + 4) {
            if let Some((_, p2)) = self.map.remove(&self.next) {
                let sc = &mut self.scratch;
                let _ = self.dec.decode_float(&p2, sc, false);
                self.next = self.next.wrapping_add(1);
                self.stats.accel += 1;
            }
        }
        ok
    }

    /// Holt den nächsten 20-ms-Frame. `false` = Stille (noch kein Audio).
    /// `queued_ms`: bereits im Ausgabepuffer wartende Audiozeit (für die Latenzmessung).
    pub fn pull(&mut self, out: &mut [f32], queued_ms: f64) -> bool {
        self.stats.target = self.target();
        if self.state == State::Buffering {
            if self.map.is_empty() {
                self.wait = 0;
                return false;
            }
            self.wait += 1;
            // Warten, bis genug vorgepuffert ist; kurze Laute ("ja") aber nicht festhalten.
            if self.map.len() < self.stats.target && self.wait < self.stats.target {
                return false;
            }
            let start = self.run_start();
            self.map = self.map.split_off(&start);
            self.next = start;
            self.state = State::Playing;
            self.plc_run = 0;
            self.wait = 0;
        }
        let next = self.next;
        if let Some((ts, p)) = self.map.remove(&next) {
            return self.play(next, ts, &p, out, queued_ms);
        }
        if let Some((_, p)) = self.map.get(&next.wrapping_add(1)) {
            // Paket `next` fehlt, der Nachfolger ist da: FEC aus dem Nachfolger.
            let ok = self.dec.decode_float(p, out, true).is_ok();
            self.stats.fec += 1;
            self.next = next.wrapping_add(1);
            self.last_ts = self.last_ts.map(|t| t.wrapping_add(20));
            return ok;
        }
        // Groessere Luecke vor bereits wartendem Audio: der Sender hat pausiert (Sprach-
        // aktivierung, DTX). Direkt beim neuen Audio weitermachen statt Stille zu erfinden.
        if let Some(&k) = self.map.keys().next() {
            if k.wrapping_sub(next) > MAX_CONCEAL_GAP {
                let (ts, p) = self.map.remove(&k).expect("vorhanden");
                return self.play(k, ts, &p, out, queued_ms);
            }
        }
        // Nichts da: PLC; nach mehreren Frames ohne Pakete neu puffern. Der Puffer bleibt
        // erhalten, damit der Anfang des naechsten Satzes nicht verloren geht.
        self.plc_run += 1;
        if self.plc_run > MAX_PLC_RUN {
            self.state = State::Buffering;
            self.stats.rebuffer += 1;
            self.last_ts = None;
            self.wait = 0;
            return false;
        }
        self.stats.plc += 1;
        self.next = next.wrapping_add(1);
        self.last_ts = self.last_ts.map(|t| t.wrapping_add(20));
        self.dec.decode_float(&[], out, false).is_ok()
    }
}

pub fn percentile(v: &mut [f64], p: f64) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    v.sort_by(|a, b| a.total_cmp(b));
    v[((v.len() - 1) as f64 * p) as usize]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_roundtrip() {
        let h = Header {
            flags: 1,
            stream_id: 7,
            seq: 0xDEADBEEF,
            ts: 123456,
        };
        let bytes = h.write(&[1, 2, 3]);
        let (p, payload) = Header::parse(&bytes).expect("parse");
        assert_eq!(p, h);
        assert_eq!(payload, &[1, 2, 3]);
        assert!(Header::parse(&[0; 5]).is_none());
    }

    fn encode_frames(n: usize) -> Vec<Vec<u8>> {
        let mut enc =
            opus::Encoder::new(RATE, opus::Channels::Mono, opus::Application::Audio).expect("enc");
        enc.set_inband_fec(true).expect("fec");
        enc.set_packet_loss_perc(10).expect("loss");
        let mut out = vec![];
        for i in 0..n {
            let pcm: Vec<f32> = (0..FRAME)
                .map(|k| {
                    let t = (i * FRAME + k) as f32 / RATE as f32;
                    0.3 * (2.0 * std::f32::consts::PI * 440.0 * t).sin()
                })
                .collect();
            out.push(enc.encode_vec_float(&pcm, 400).expect("encode"));
        }
        out
    }

    fn hdr(i: usize) -> Header {
        Header {
            flags: 0,
            stream_id: 0,
            seq: i as u32,
            ts: unix_ms32(),
        }
    }

    /// Füllt 3 Frames vor, dann je ein Push und ein Pull (wie im Echtbetrieb).
    fn run(n: usize, lost: Option<usize>) -> (Receiver, usize) {
        let frames = encode_frames(n);
        let mut rx = Receiver::new().expect("rx");
        let mut out = vec![0.0; FRAME];
        let mut played = 0;
        for (i, f) in frames.iter().enumerate() {
            if Some(i) != lost {
                rx.push(hdr(i), f);
            }
            if i >= 3 && rx.pull(&mut out, 0.0) {
                played += 1;
            }
        }
        (rx, played)
    }

    #[test]
    fn plays_all_frames_without_loss() {
        let (rx, played) = run(30, None);
        assert_eq!(played, 27);
        assert_eq!(rx.stats.plc, 0);
        assert_eq!(rx.stats.lost_on_wire(), 0);
    }

    #[test]
    fn single_loss_uses_fec() {
        let (rx, _) = run(30, Some(10));
        assert_eq!(rx.stats.lost_on_wire(), 1);
        assert_eq!(rx.stats.fec, 1);
        assert_eq!(rx.stats.plc, 0);
    }

    /// Echtzeit-Simulation: je Takt (20 ms) ein Frame des Senders, dann ein `pull`.
    /// `sent(i)` sagt, ob Frame `i` als Sprache, als DTX-Lebenszeichen oder gar nicht ankommt.
    /// Liefert (Empfaenger, abgespielte Sprach-Frames, gesendete Sprach-Frames).
    fn simulate(n: usize, sent: impl Fn(usize) -> Option<bool>) -> (Receiver, usize, usize) {
        let frames = encode_frames(n);
        let mut rx = Receiver::new().expect("rx");
        let mut out = vec![0.0; FRAME];
        let mut speech = 0;
        for (i, f) in frames.iter().enumerate() {
            match sent(i) {
                Some(true) => {
                    speech += 1;
                    rx.push(hdr(i), f);
                }
                Some(false) => rx.push(hdr(i), &[0x08]),
                None => {}
            }
            rx.pull(&mut out, 0.0);
        }
        // Rest ausspielen.
        for _ in 0..20 {
            rx.pull(&mut out, 0.0);
        }
        let played = rx.stats.latency_ms.len() + rx.stats.accel as usize;
        (rx, played, speech)
    }

    #[test]
    fn speech_after_dtx_pause_keeps_its_beginning() {
        // 30 Frames Sprache, 40 Frames Stille mit DTX-Lebenszeichen, wieder 30 Frames Sprache.
        let (rx, played, speech) = simulate(100, |i| match i {
            0..30 | 70..100 => Some(true),
            _ if i % 20 == 9 => Some(false),
            _ => None,
        });
        assert_eq!(speech, 60);
        assert_eq!(played, speech, "kein Sprach-Frame darf verloren gehen");
        assert!(rx.stats.rebuffer >= 1);
        assert_eq!(rx.stats.late, 0);
    }

    #[test]
    fn short_send_pause_is_skipped_without_losing_audio() {
        // Sprachaktivierung schliesst kurz (5 Frames nicht gesendet): weiter ohne Verlust.
        let (rx, played, speech) = simulate(80, |i| (!(40..45).contains(&i)).then_some(true));
        assert_eq!(played, speech);
        assert_eq!(rx.stats.late, 0);
    }

    #[test]
    fn short_utterance_is_played_even_below_target() {
        // Nur 2 Frames ("ja") und dann nichts mehr: muss trotzdem abgespielt werden.
        let (_, played, speech) = simulate(30, |i| (10..12).contains(&i).then_some(true));
        assert_eq!(speech, 2);
        assert_eq!(played, 2);
    }

    #[test]
    fn late_packet_raises_target() {
        let frames = encode_frames(20);
        let mut rx = Receiver::new().expect("rx");
        let mut out = vec![0.0; FRAME];
        for (i, f) in frames.iter().enumerate().take(10) {
            if i != 5 {
                rx.push(hdr(i), f);
            }
        }
        for _ in 0..8 {
            rx.pull(&mut out, 0.0);
        }
        let before = rx.target();
        rx.push(hdr(5), &frames[5]);
        assert_eq!(rx.stats.late, 1);
        assert!(rx.target() > before);
    }

    #[test]
    fn late_packet_is_dropped() {
        let frames = encode_frames(10);
        let mut rx = Receiver::new().expect("rx");
        for (i, f) in frames.iter().enumerate().skip(1) {
            rx.push(
                Header {
                    flags: 0,
                    stream_id: 0,
                    seq: i as u32,
                    ts: unix_ms32(),
                },
                f,
            );
        }
        let mut out = vec![0.0; FRAME];
        for _ in 0..4 {
            rx.pull(&mut out, 0.0);
        }
        rx.push(
            Header {
                flags: 0,
                stream_id: 0,
                seq: 0,
                ts: unix_ms32(),
            },
            &frames[0],
        );
        assert_eq!(rx.stats.late, 1);
    }
}
