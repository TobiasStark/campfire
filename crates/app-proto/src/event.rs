//! Event-Envelope und Event-Arten.

use crate::{Hash, PublicKey, Signature};
use serde::{Deserialize, Serialize};

pub type SpaceId = [u8; 16];
pub type ChannelId = [u8; 16];
/// blake3 ueber die serialisierte `EventBody`.
pub type EventId = Hash;

/// Hybrid Logical Clock: Wanduhr (ms) plus Zaehler.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
pub struct Hlc {
    pub wall_ms: u64,
    pub counter: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChannelKind {
    Voice,
    Text,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum EventKind {
    SpaceCreate {
        name: String,
    },
    MemberAdd {
        user: PublicKey,
    },
    MemberRemove {
        user: PublicKey,
    },
    ChannelCreate {
        channel: ChannelId,
        kind: ChannelKind,
        name: String,
        position: u32,
    },
    ChannelUpdate {
        channel: ChannelId,
        name: Option<String>,
        position: Option<u32>,
    },
    ChannelDelete {
        channel: ChannelId,
    },
    Message {
        channel: ChannelId,
        body: String,
        reply_to: Option<EventId>,
    },
    MessageEdit {
        target: EventId,
        body: String,
    },
    MessageDelete {
        target: EventId,
    },
    Reaction {
        target: EventId,
        emoji: String,
    },
    ReactionRemove {
        target: EventId,
        emoji: String,
    },
    FileShare {
        channel: ChannelId,
        blob_hash: Hash,
        name: String,
        size: u64,
        mime: String,
        thumb_hash: Option<Hash>,
    },
}

/// Der signierte Teil eines Events.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventBody {
    pub space_id: SpaceId,
    pub author: PublicKey,
    /// Pro Autor und Space lueckenlos aufsteigend (ab 1).
    pub seq: u64,
    pub hlc: Hlc,
    pub kind: EventKind,
}

impl EventBody {
    /// Bytes, die der Autor signiert (deterministisches postcard).
    pub fn signing_bytes(&self) -> Vec<u8> {
        postcard::to_stdvec(self).expect("EventBody ist immer serialisierbar")
    }

    pub fn id(&self) -> EventId {
        *blake3::hash(&self.signing_bytes()).as_bytes()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventEnvelope {
    pub body: EventBody,
    pub signature: Signature,
}

impl EventEnvelope {
    pub fn id(&self) -> EventId {
        self.body.id()
    }
}

/// Anzeigereihenfolge: HLC, bei Gleichstand event_id.
pub fn display_order(a: (&Hlc, &EventId), b: (&Hlc, &EventId)) -> std::cmp::Ordering {
    a.0.cmp(b.0).then_with(|| a.1.cmp(b.1))
}
