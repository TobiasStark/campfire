//! Sync-Stream: Heads austauschen, fehlende Events anfordern.

use crate::event::{EventEnvelope, EventId, SpaceId};
use crate::PublicKey;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpaceHeads {
    pub space_id: SpaceId,
    /// (Autor, hoechste bekannte seq, ID des Events mit dieser seq). Die ID deckt auf, wenn
    /// ein Autor zwei verschiedene Events mit derselben seq signiert hat (N-14).
    pub authors: Vec<(PublicKey, u64, EventId)>,
    /// Einladung des Empfaengers in diesen Space (fehlt beim Gruender).
    pub invite: Option<Box<Invite>>,
}

/// Eintrittskarte: Gruendung und `MemberAdd` des Empfaengers, beide vom Gruender signiert. Nur
/// damit nimmt ein Knoten Events eines Spaces an, in dem er (noch) kein Mitglied ist.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Invite {
    pub create: EventEnvelope,
    pub add: EventEnvelope,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Want {
    pub space_id: SpaceId,
    pub author: PublicKey,
    pub from_seq: u64,
    pub to_seq: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SyncMessage {
    Heads(Vec<SpaceHeads>),
    Want(Vec<Want>),
    Events(Vec<EventEnvelope>),
    /// Ende des Sync-Laufs.
    Done,
}
