//! Sync-Stream: Heads austauschen, fehlende Events anfordern.

use crate::event::{EventEnvelope, SpaceId};
use crate::PublicKey;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpaceHeads {
    pub space_id: SpaceId,
    /// (Autor, hoechste bekannte seq)
    pub authors: Vec<(PublicKey, u64)>,
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
