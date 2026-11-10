//! Freunde: Codes, Anfragen ueber `app/hello/1`, Allowlist.

pub mod directory;
pub mod limiter;
pub mod service;
pub mod store;

pub use directory::FriendsDirectory;
pub use service::{FriendEvent, FriendsError, FriendsService, ServiceConfig};
pub use store::{Friend, FriendStatus};

use app_proto::friend_code::SECRET_LEN;

/// Einladungsgeheimnis der Code-Generation `epoch` (N-1): wer den aktuellen Freundescode nicht
/// kennt, kann keine Anfrage stellen; ein neuer Code (`epoch + 1`) macht alte wertlos.
pub fn invite_secret(key: &[u8; 32], epoch: u64) -> [u8; SECRET_LEN] {
    let h = blake3::keyed_hash(key, &epoch.to_be_bytes());
    let mut out = [0u8; SECRET_LEN];
    out.copy_from_slice(&h.as_bytes()[..SECRET_LEN]);
    out
}

/// Vergleich ohne fruehen Abbruch.
fn ct_eq(a: &[u8; SECRET_LEN], b: &[u8; SECRET_LEN]) -> bool {
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests;
