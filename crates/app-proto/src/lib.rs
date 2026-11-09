//! app-proto: Wire-Typen, Events, ALPNs, Versionierung. Keine I/O.
//!
//! Alle Nachrichten werden mit postcard serialisiert (D12). Schluessel und
//! Signaturen sind hier rohe Bytes; die Kryptografie liegt in `app-core`.

pub mod alpn;
pub mod control;
pub mod event;
pub mod friend_code;
pub mod hello;
pub mod sync;
pub mod video;
pub mod voice;

use serde::{de::DeserializeOwned, Deserialize, Serialize};

/// Aktuelle Protokollversion (steckt zusaetzlich im ALPN).
pub const PROTOCOL_VERSION: u16 = 1;
/// Obergrenze fuer eine einzelne Nachricht auf Control-/Sync-/Hello-Streams.
pub const MAX_MESSAGE_LEN: usize = 1024 * 1024;

pub type PublicKey = [u8; 32];
pub type Hash = [u8; 32];

/// Ed25519-Signatur (64 Bytes). serde kennt keine Arrays > 32, daher eigener Typ.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Signature(pub [u8; 64]);

impl std::fmt::Debug for Signature {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Signature({:02x}{:02x}..)", self.0[0], self.0[1])
    }
}

impl Serialize for Signature {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_bytes(&self.0)
    }
}

impl<'de> Deserialize<'de> for Signature {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl serde::de::Visitor<'_> for V {
            type Value = Signature;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("64 Bytes")
            }
            fn visit_bytes<E: serde::de::Error>(self, v: &[u8]) -> Result<Signature, E> {
                let arr: [u8; 64] = v
                    .try_into()
                    .map_err(|_| E::invalid_length(v.len(), &"64 Bytes"))?;
                Ok(Signature(arr))
            }
        }
        d.deserialize_bytes(V)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum WireError {
    #[error("Nachricht zu gross ({0} Bytes)")]
    TooLarge(usize),
    #[error("Dekodierfehler: {0}")]
    Decode(postcard::Error),
    #[error("Kodierfehler: {0}")]
    Encode(postcard::Error),
    #[error("Unerwartete Restbytes nach der Nachricht")]
    TrailingBytes,
}

/// Serialisiert eine Nachricht (deterministisch).
pub fn encode<T: Serialize>(msg: &T) -> Result<Vec<u8>, WireError> {
    let v = postcard::to_stdvec(msg).map_err(WireError::Encode)?;
    if v.len() > MAX_MESSAGE_LEN {
        return Err(WireError::TooLarge(v.len()));
    }
    Ok(v)
}

/// Deserialisiert eine Nachricht; zu grosse Eingaben und Restbytes werden abgelehnt.
pub fn decode<T: DeserializeOwned>(data: &[u8]) -> Result<T, WireError> {
    if data.len() > MAX_MESSAGE_LEN {
        return Err(WireError::TooLarge(data.len()));
    }
    let (v, rest) = postcard::take_from_bytes(data).map_err(WireError::Decode)?;
    if !rest.is_empty() {
        return Err(WireError::TrailingBytes);
    }
    Ok(v)
}

#[cfg(test)]
mod tests;
