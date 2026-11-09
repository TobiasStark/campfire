//! Video-Transport: Control-Nachrichten und Frame-Kopf.
//!
//! Jeder Frame geht auf einem eigenen unidirektionalen QUIC-Stream. Der Kopf ist eine feste
//! Big-Endian-Struktur (kein postcard), danach folgen die kodierten Nutzdaten.

use serde::{Deserialize, Serialize};

pub const HEADER_LEN: usize = 16;
/// Groesster erlaubter Frame (Nutzdaten). Ein 4K-Keyframe mit 50 Mbit/s bleibt weit darunter.
pub const MAX_FRAME_LEN: usize = 8 * 1024 * 1024;

/// Flag: Keyframe (jederzeit als Einstieg dekodierbar).
pub const FLAG_KEY: u8 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum VideoCodec {
    H264,
    Hevc,
    Av1,
}

impl VideoCodec {
    pub const fn to_byte(self) -> u8 {
        match self {
            Self::H264 => 1,
            Self::Hevc => 2,
            Self::Av1 => 3,
        }
    }

    pub const fn from_byte(b: u8) -> Option<Self> {
        match b {
            1 => Some(Self::H264),
            2 => Some(Self::Hevc),
            3 => Some(Self::Av1),
            _ => None,
        }
    }
}

/// Kopf eines Video-Frames: `[stream_id u8][layer u8][flags u8][codec u8][frame_no u32][ts u32][len u32]`.
///
/// `ts` sind Millisekunden auf der Medienuhr des Senders; dieselbe Uhr liefert den Voice-`ts`
/// (dort in 48-kHz-Samples, also `ts_voice / 48`). So lassen sich Bild und Ton abgleichen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameHeader {
    pub stream_id: u8,
    pub layer: u8,
    pub flags: u8,
    pub codec: VideoCodec,
    pub frame_no: u32,
    pub ts: u32,
    pub len: u32,
}

impl FrameHeader {
    pub fn is_key(&self) -> bool {
        self.flags & FLAG_KEY != 0
    }

    pub fn write(&self) -> [u8; HEADER_LEN] {
        let mut h = [0u8; HEADER_LEN];
        h[0] = self.stream_id;
        h[1] = self.layer;
        h[2] = self.flags;
        h[3] = self.codec.to_byte();
        h[4..8].copy_from_slice(&self.frame_no.to_be_bytes());
        h[8..12].copy_from_slice(&self.ts.to_be_bytes());
        h[12..16].copy_from_slice(&self.len.to_be_bytes());
        h
    }

    /// `None` bei zu kurzem Kopf, unbekanntem Codec oder unzulaessiger Laenge.
    pub fn parse(data: &[u8]) -> Option<Self> {
        if data.len() < HEADER_LEN {
            return None;
        }
        let len = u32::from_be_bytes(data[12..16].try_into().ok()?);
        if len as usize > MAX_FRAME_LEN {
            return None;
        }
        Some(Self {
            stream_id: data[0],
            layer: data[1],
            flags: data[2],
            codec: VideoCodec::from_byte(data[3])?,
            frame_no: u32::from_be_bytes(data[4..8].try_into().ok()?),
            ts: u32::from_be_bytes(data[8..12].try_into().ok()?),
            len,
        })
    }
}

/// Eine Qualitaetsstufe eines Streams (Layer 0 = hoechste).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LayerInfo {
    pub layer: u8,
    pub codec: VideoCodec,
    pub width: u16,
    pub height: u16,
    pub fps: u8,
    pub kbps: u32,
}

/// Messwerte eines Zuschauers ueber ein Zeitfenster (M3-04 passt damit die Bitrate an).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReceiverReport {
    pub stream_id: u8,
    /// Laenge des Fensters in Millisekunden.
    pub interval_ms: u32,
    /// Vollstaendig angekommene Frames.
    pub received: u32,
    /// Frames, die laut `frame_no` fehlen oder abgebrochen wurden.
    pub lost: u32,
    /// Frames, die zu spaet ankamen und verworfen wurden.
    pub late: u32,
    /// Schwankung der Ankunftsabstaende in Millisekunden.
    pub jitter_ms: u32,
    /// Durchsatz der angekommenen Nutzdaten in kbit/s.
    pub kbps: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum VideoMsg {
    /// Sender kuendigt einen Stream an (oder ersetzt ihn, z. B. nach Quellenwechsel).
    Announce {
        stream_id: u8,
        layers: Vec<LayerInfo>,
        /// Hat der Stream Ton (Prozess-Loopback)?
        audio: bool,
    },
    /// Sender beendet den Stream.
    Stop {
        stream_id: u8,
    },
    /// Zuschauer abonniert eine Stufe und nennt, was er dekodieren kann.
    Subscribe {
        stream_id: u8,
        layer: u8,
        decoders: Vec<VideoCodec>,
    },
    Unsubscribe {
        stream_id: u8,
    },
    /// Zuschauer braucht einen Keyframe (Einstieg oder nach Verlust).
    KeyframeRequest {
        stream_id: u8,
    },
    Report(ReceiverReport),
    /// Sender lehnt ein Abo ab (z. B. Upload-Budget erschoepft, M3-04).
    Refused {
        stream_id: u8,
        reason: RefuseReason,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RefuseReason {
    NoSuchStream,
    /// Das Upload-Budget des Senders ist aufgebraucht.
    Capacity,
    /// Kein gemeinsamer Codec.
    NoCommonCodec,
    NotAllowed,
}
