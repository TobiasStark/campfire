//! Event-Envelope und Event-Arten.

use crate::{Hash, PublicKey, Signature};
use serde::{Deserialize, Serialize};

pub type SpaceId = [u8; 16];
pub type ChannelId = [u8; 16];
/// blake3 ueber die signierten Bytes ([`EventBody::signing_bytes`]); fuer ein Event und seinen
/// Platzhalter gleich.
pub type EventId = Hash;

/// Groesste Datei, die geteilt werden kann (4 GB). Groessere `FileShare` werden verworfen.
pub const MAX_FILE_BYTES: u64 = 4 * 1024 * 1024 * 1024;

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
    /// Entfernt `user` (Admin) oder tritt aus (`user` = Autor). `last_seq` ist die letzte `seq`
    /// von `user` in diesem Space, die noch gilt (beim Austritt die `seq` dieses Events). Spaetere
    /// Events von `user` werden abgelehnt, egal welchen HLC sie tragen (M-1).
    MemberRemove {
        user: PublicKey,
        last_seq: u64,
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
    /// Platzhalter fuer ein aufgeraeumtes Event (Speichergrenze): nur Art und Inhalts-Hash.
    /// Signatur und ID bleiben gueltig, weil der Autor genau diese beiden Werte signiert. Nur
    /// fuer Nachrichten-Arten erlaubt ([`prunable`]), nie fuer Mitglieder oder Channels.
    Pruned {
        tag: u8,
        content: Hash,
    },
}

/// Feste Kennung je Art (Teil der signierten Bytes, darf sich nie aendern).
pub mod tag {
    pub const SPACE_CREATE: u8 = 0;
    pub const MEMBER_ADD: u8 = 1;
    pub const MEMBER_REMOVE: u8 = 2;
    pub const CHANNEL_CREATE: u8 = 3;
    pub const CHANNEL_UPDATE: u8 = 4;
    pub const CHANNEL_DELETE: u8 = 5;
    pub const MESSAGE: u8 = 6;
    pub const MESSAGE_EDIT: u8 = 7;
    pub const MESSAGE_DELETE: u8 = 8;
    pub const REACTION: u8 = 9;
    pub const REACTION_REMOVE: u8 = 10;
    pub const FILE_SHARE: u8 = 11;
}

/// Arten, die zu einem Platzhalter werden duerfen: Nachrichten und alles, was sich auf eine
/// bezieht. Mitglieder-, Space- und Channel-Events bleiben immer vollstaendig.
pub fn prunable(t: u8) -> bool {
    (tag::MESSAGE..=tag::FILE_SHARE).contains(&t)
}

impl EventKind {
    pub fn tag(&self) -> u8 {
        match self {
            EventKind::SpaceCreate { .. } => tag::SPACE_CREATE,
            EventKind::MemberAdd { .. } => tag::MEMBER_ADD,
            EventKind::MemberRemove { .. } => tag::MEMBER_REMOVE,
            EventKind::ChannelCreate { .. } => tag::CHANNEL_CREATE,
            EventKind::ChannelUpdate { .. } => tag::CHANNEL_UPDATE,
            EventKind::ChannelDelete { .. } => tag::CHANNEL_DELETE,
            EventKind::Message { .. } => tag::MESSAGE,
            EventKind::MessageEdit { .. } => tag::MESSAGE_EDIT,
            EventKind::MessageDelete { .. } => tag::MESSAGE_DELETE,
            EventKind::Reaction { .. } => tag::REACTION,
            EventKind::ReactionRemove { .. } => tag::REACTION_REMOVE,
            EventKind::FileShare { .. } => tag::FILE_SHARE,
            EventKind::Pruned { tag, .. } => *tag,
        }
    }

    /// Hash des Inhalts, den der Autor signiert.
    pub fn content_hash(&self) -> Hash {
        match self {
            EventKind::Pruned { content, .. } => *content,
            k => {
                let bytes = postcard::to_stdvec(k).expect("EventKind ist immer serialisierbar");
                *blake3::Hasher::new_derive_key("campfire 2026-11 event content")
                    .update(&bytes)
                    .finalize()
                    .as_bytes()
            }
        }
    }

    /// Platzhalter mit derselben Signatur und ID.
    pub fn pruned(&self) -> EventKind {
        EventKind::Pruned {
            tag: self.tag(),
            content: self.content_hash(),
        }
    }

    pub fn is_pruned(&self) -> bool {
        matches!(self, EventKind::Pruned { .. })
    }
}

/// Was der Autor signiert: Kopf, Art und Inhalts-Hash, nicht der Inhalt selbst.
#[derive(Serialize)]
struct Signed<'a> {
    domain: &'static str,
    space_id: &'a SpaceId,
    author: &'a PublicKey,
    seq: u64,
    hlc: Hlc,
    tag: u8,
    content: Hash,
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
    /// Bytes, die der Autor signiert (deterministisches postcard). Enthaelt statt des Inhalts
    /// nur dessen Hash, damit ein Platzhalter ([`EventKind::Pruned`]) pruefbar bleibt.
    pub fn signing_bytes(&self) -> Vec<u8> {
        postcard::to_stdvec(&Signed {
            domain: "app/event/2",
            space_id: &self.space_id,
            author: &self.author,
            seq: self.seq,
            hlc: self.hlc,
            tag: self.kind.tag(),
            content: self.kind.content_hash(),
        })
        .expect("EventBody ist immer serialisierbar")
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
