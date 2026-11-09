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

/// Empfänger für einen Sprecher: Jitter-Buffer + Opus-Decoder.
pub struct Receiver {
    map: BTreeMap<u32, (u32, Vec<u8>)>, // seq -> (ts, opus)
    next: u32,
    state: State,
    dec: opus::Decoder,
    plc_run: u32,
    jitter: f64,
    prev: Option<(f64, f64)>, // (arrival_ms, ts_ms)
    t0: Instant,
    pub stats: RxStats,
    scratch: Vec<f32>,
    /// Zeitstempel des zuletzt abgespielten Frames (Stream-Ton: Medienuhr des Senders).
    last_ts: Option<u32>,
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
            prev: None,
            t0: Instant::now(),
            stats: RxStats {
                target: 3,
                ..Default::default()
            },
            scratch: vec![0.0; FRAME * n],
            last_ts: None,
        })
    }

    /// Zeitstempel des Frames, der zuletzt aus `pull` kam (None, solange nichts spielt).
    pub fn playing_ts(&self) -> Option<u32> {
        self.last_ts
    }

    /// Zieltiefe in Frames aus dem Jitter-Schätzer (RFC 3550): 2·Jitter + 10 ms, 2..10 Frames.
    fn target(&self) -> usize {
        (((2.0 * self.jitter + 10.0) / 20.0).ceil() as usize).clamp(2, 10)
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
        self.stats.jitter_ms = self.jitter;

        if self.state == State::Playing {
            let diff = h.seq.wrapping_sub(self.next) as i32;
            if diff < 0 {
                self.stats.late += 1;
                return;
            }
            if diff > 100 {
                // Sender hat neu angefangen / große Lücke: neu synchronisieren
                self.map.clear();
                self.state = State::Buffering;
            }
        }
        self.map.insert(h.seq, (h.ts, payload.to_vec()));
    }

    /// Holt den nächsten 20-ms-Frame. `false` = Stille (noch kein Audio).
    /// `queued_ms`: bereits im Ausgabepuffer wartende Audiozeit (für die Latenzmessung).
    pub fn pull(&mut self, out: &mut [f32], queued_ms: f64) -> bool {
        self.stats.target = self.target();
        if self.state == State::Buffering {
            if self.map.len() >= self.stats.target {
                self.next = *self.map.keys().next().unwrap_or(&0);
                self.state = State::Playing;
            } else {
                return false;
            }
        }
        let next = self.next;
        if let Some((ts, p)) = self.map.remove(&next) {
            let ok = self.dec.decode_float(&p, out, false).is_ok();
            self.plc_run = 0;
            self.next = next.wrapping_add(1);
            self.last_ts = Some(ts);
            let lat = unix_ms32().wrapping_sub(ts) as i32 as f64 + queued_ms;
            if (-1000.0..10_000.0).contains(&lat) {
                self.stats.latency_ms.push(lat);
            }
            // Zu viel gepuffert (Jitter ist gesunken): einen Frame überspringen.
            if self.map.len() > self.stats.target + 3 {
                if let Some((_, p2)) = self.map.remove(&self.next) {
                    let sc = &mut self.scratch;
                    let _ = self.dec.decode_float(&p2, sc, false);
                    self.next = self.next.wrapping_add(1);
                    self.stats.accel += 1;
                }
            }
            return ok;
        }
        if let Some((_, p)) = self.map.get(&next.wrapping_add(1)) {
            // Paket `next` fehlt, der Nachfolger ist da: FEC aus dem Nachfolger.
            let ok = self.dec.decode_float(p, out, true).is_ok();
            self.stats.fec += 1;
            self.next = next.wrapping_add(1);
            self.last_ts = self.last_ts.map(|t| t.wrapping_add(20));
            return ok;
        }
        // Nichts da: PLC; nach mehreren Frames Stille neu puffern.
        self.plc_run += 1;
        if self.plc_run > 6 {
            self.state = State::Buffering;
            self.stats.rebuffer += 1;
            self.map.clear();
            self.last_ts = None;
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
