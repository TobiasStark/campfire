//! Allowlist fuer `app/peer/1`, gespeist aus der Freundestabelle.

use super::store;
use app_net::{Access, PeerDirectory};
use app_proto::lock::RwLocked;
use app_proto::PublicKey;
use rusqlite::Connection;
use std::collections::HashMap;
use std::sync::RwLock;

#[derive(Default)]
pub struct FriendsDirectory {
    map: RwLock<HashMap<PublicKey, Access>>,
}

impl FriendsDirectory {
    /// Liest die Tabelle neu ein (auf dem DB-Thread aufrufen).
    pub fn reload(&self, conn: &Connection) -> rusqlite::Result<()> {
        let m = store::access_map(conn)?;
        *self.map.write_locked() = m;
        Ok(())
    }
}

impl PeerDirectory for FriendsDirectory {
    fn check(&self, user: &PublicKey) -> Access {
        self.map
            .read_locked()
            .get(user)
            .copied()
            .unwrap_or(Access::Unknown)
    }
}
