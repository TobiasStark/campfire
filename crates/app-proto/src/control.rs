//! Control-Stream auf `app/peer/1`: Handshake, Presence, Ping, Voice-Signale, Stream-Abos.

use crate::event::{ChannelId, SpaceId};
use crate::{Hash, PublicKey, Signature};
use serde::{Deserialize, Serialize};

/// `sign_user(device_pubkey, created_at)`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceCert {
    pub user: PublicKey,
    pub device: PublicKey,
    pub created_at: u64,
    pub signature: Signature,
}

impl DeviceCert {
    /// Bytes, die der User-Key signiert.
    pub fn signing_bytes(user: &PublicKey, device: &PublicKey, created_at: u64) -> Vec<u8> {
        let mut v = Vec::with_capacity(17 + 64 + 8);
        v.extend_from_slice(b"app/device-cert/1");
        v.extend_from_slice(user);
        v.extend_from_slice(device);
        v.extend_from_slice(&created_at.to_be_bytes());
        v
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Handshake {
    /// Vom Absender unterstuetzte Protokollversionen (aufsteigend).
    pub versions: Vec<u16>,
    pub cert: DeviceCert,
    pub display_name: String,
    pub avatar_hash: Option<Hash>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum HandshakeReply {
    Accept {
        version: u16,
        handshake: Box<Handshake>,
    },
    Reject {
        reason: RejectReason,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RejectReason {
    UnknownUser,
    InvalidCert,
    VersionMismatch,
    Blocked,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PresenceStatus {
    Online,
    Away,
    DoNotDisturb,
    Invisible,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Presence {
    pub status: PresenceStatus,
    pub voice: Option<(SpaceId, ChannelId)>,
    pub streaming: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum VoiceSignal {
    Join {
        space_id: SpaceId,
        channel: ChannelId,
        stream_id: u8,
    },
    Leave {
        space_id: SpaceId,
        channel: ChannelId,
    },
    State {
        muted: bool,
        deafened: bool,
    },
    /// Direktanruf: Einladung an alle Geraete des Freundes (M1-14).
    CallInvite {
        call_id: [u8; 16],
    },
    CallAccept {
        call_id: [u8; 16],
    },
    CallDecline {
        call_id: [u8; 16],
        busy: bool,
    },
    /// Anrufer legt auf, bevor angenommen wurde (oder ein anderes Geraet hat angenommen).
    CallCancel {
        call_id: [u8; 16],
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ControlMessage {
    Presence(Presence),
    Ping(u64),
    Pong(u64),
    Voice(VoiceSignal),
    /// Stream-Abo (Video); Layer 0 = hoechste Qualitaet.
    Subscribe {
        stream_id: u8,
        layer: u8,
    },
    Unsubscribe {
        stream_id: u8,
    },
    /// Keyframe anfordern.
    RequestKeyframe {
        stream_id: u8,
    },
    /// Video-Steuerung (M3-03). Die drei Varianten oben sind reserviert und werden nicht
    /// mehr benutzt; neue Varianten stehen am Ende, damit die Kodierung kompatibel bleibt.
    Video(crate::video::VideoMsg),
}
