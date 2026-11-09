//! `app/hello/1`: Freundschaftsanfragen von unbekannten Geraeten.

use crate::control::DeviceCert;
use serde::{Deserialize, Serialize};

pub const MAX_NAME_LEN: usize = 64;
pub const MAX_NOTE_LEN: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum HelloMessage {
    Request {
        cert: DeviceCert,
        display_name: String,
        note: String,
    },
    Accept {
        cert: DeviceCert,
        display_name: String,
    },
    Decline,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum HelloReply {
    /// Anfrage gespeichert bzw. verarbeitet.
    Received,
    RateLimited,
    Blocked,
    Invalid,
}

impl HelloMessage {
    /// Grobe Laengenpruefung vor jeder weiteren Verarbeitung.
    pub fn is_sane(&self) -> bool {
        match self {
            Self::Request {
                display_name, note, ..
            } => display_name.len() <= MAX_NAME_LEN && note.len() <= MAX_NOTE_LEN,
            Self::Accept { display_name, .. } => display_name.len() <= MAX_NAME_LEN,
            Self::Decline => true,
        }
    }
}
