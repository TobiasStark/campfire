//! `app/hello/1`: Freundschaftsanfragen von unbekannten Geraeten.

use crate::control::DeviceCert;
use crate::{PublicKey, Signature};
use serde::{Deserialize, Serialize};

/// Schluessel des Rendezvous-Endpoints eines Geraets (M-5): eigener iroh-Endpoint nur fuer
/// `app/hello/1`, nur ueber Relay. Seine ID steht im Freundescode; die eigentliche Geraete-ID
/// (und damit direkte Pfade samt IP) erfahren nur angenommene Freunde.
pub fn rendezvous_secret(device_secret: &[u8; 32]) -> [u8; 32] {
    blake3::derive_key("campfire 2026-11 rendezvous endpoint key", device_secret)
}

/// Signiert vom Geraeteschluessel: dieser Rendezvous-Endpoint gehoert zu diesem Geraet.
pub fn binding_bytes(device: &PublicKey, rendezvous: &PublicKey) -> Vec<u8> {
    let mut v = b"app/rendezvous/1".to_vec();
    v.extend_from_slice(device);
    v.extend_from_slice(rendezvous);
    v
}

pub const MAX_NAME_LEN: usize = 64;
pub const MAX_NOTE_LEN: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum HelloMessage {
    Request {
        cert: DeviceCert,
        display_name: String,
        note: String,
        /// Einladungsgeheimnis aus dem Freundescode des Empfaengers (N-1).
        secret: [u8; crate::friend_code::SECRET_LEN],
        /// Signatur von `cert.device` ueber [`binding_bytes`] mit dem Endpoint der Verbindung.
        binding: Signature,
        /// Schluesselwechsel auf `cert.user` (M-7): ersetzt beim Empfaenger einen Freund mit
        /// dem alten Schluessel, ohne dass dieser neu annehmen muss.
        rotation: Option<Box<crate::recovery::Rotation>>,
    },
    Accept {
        cert: DeviceCert,
        display_name: String,
        /// Wie bei `Request`.
        binding: Signature,
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
