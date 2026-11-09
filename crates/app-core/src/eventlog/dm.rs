//! Direktnachrichten (M2-07): ein Space mit genau zwei Mitgliedern und einem Text-Channel.
//!
//! Es gibt kein eigenes Ereignis dafuer. Wer zuerst schreibt, legt einen Space mit dem
//! reservierten Namen `DM_NAME` an, fuegt den Freund hinzu und erstellt den Channel. Der Name
//! enthaelt ein Zeichen, das man nicht eintippen kann, damit kein normaler Space so heisst.

use app_proto::event::{ChannelId, SpaceId};
use app_proto::PublicKey;
use rusqlite::{params, Connection};
use std::collections::HashMap;

/// Reservierter Space-Name (U+200B + "dm"); `trim()` entfernt das Zeichen nicht.
pub const DM_NAME: &str = "\u{200b}dm";
/// Name des Channels in einer Direktnachricht.
pub const DM_CHANNEL: &str = "chat";

pub fn is_dm(space_name: &str) -> bool {
    space_name == DM_NAME
}

/// Alle Direktnachrichten: Gegenueber -> (Space, Channel). Der Channel fehlt, solange die
/// Gegenseite ihn noch nicht synchronisiert hat. Gibt es versehentlich mehrere Spaces mit
/// demselben Freund (beide haben gleichzeitig angefangen), gilt der mit der kleinsten ID,
/// auf den sich beide Seiten ohne Absprache einigen.
///
/// Als DM zaehlt nur ein Space mit **genau zwei** Mitgliedern, von denen eines wir selbst
/// sind. Der Name allein reicht nicht: Sonst koennte ein Dritter einen "DM"-Space mit uns
/// und einem anderen Freund (und sich selbst als Gruender) anlegen und mitlesen.
pub fn dm_index(
    conn: &Connection,
    me: &PublicKey,
) -> rusqlite::Result<HashMap<PublicKey, (SpaceId, Option<ChannelId>)>> {
    let mut st = conn.prepare_cached(
        "SELECT s.space_id, other.user_pubkey,
                (SELECT c.channel_id FROM channels c
                  WHERE c.space_id = s.space_id AND c.kind = 'text'
                  ORDER BY c.position, c.channel_id LIMIT 1)
         FROM spaces s
         JOIN members mine ON mine.space_id = s.space_id AND mine.user_pubkey = ?1
         JOIN members other ON other.space_id = s.space_id AND other.user_pubkey != ?1
         WHERE s.name = ?2
           AND (SELECT count(*) FROM members n WHERE n.space_id = s.space_id) = 2
         ORDER BY s.space_id",
    )?;
    let mut out = HashMap::new();
    let mut rows = st.query(params![me, DM_NAME])?;
    while let Some(r) = rows.next()? {
        out.entry(r.get::<_, PublicKey>(1)?)
            .or_insert((r.get::<_, SpaceId>(0)?, r.get::<_, Option<ChannelId>>(2)?));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::open_connection;

    const ME: PublicKey = [9; 32];
    const ANNA: PublicKey = [5; 32];
    const BEN: PublicKey = [6; 32];

    fn space(c: &Connection, id: u8, name: &str, members: &[PublicKey], channel: bool) {
        c.execute(
            "INSERT INTO spaces (space_id, name, joined_at) VALUES (?1, ?2, 0)",
            params![[id; 16], name],
        )
        .unwrap();
        for m in members {
            c.execute(
                "INSERT INTO members (space_id, user_pubkey, added_by) VALUES (?1, ?2, ?2)",
                params![[id; 16], m],
            )
            .unwrap();
        }
        if channel {
            c.execute(
                "INSERT INTO channels (space_id, channel_id, kind, name) VALUES (?1, ?2, 'text', 'chat')",
                params![[id; 16], [id + 100; 16]],
            )
            .unwrap();
        }
    }

    #[test]
    fn index_maps_friend_to_lowest_space_and_ignores_normal_spaces() {
        let t = tempfile::tempdir().unwrap();
        let c = open_connection(&t.path().join("d.db")).unwrap();
        space(&c, 1, "Freunde", &[ME, ANNA], true);
        space(&c, 7, DM_NAME, &[ME, ANNA], true);
        space(&c, 3, DM_NAME, &[ANNA, ME], true);
        space(&c, 4, DM_NAME, &[ME, BEN], false);
        let idx = dm_index(&c, &ME).unwrap();
        assert_eq!(idx.len(), 2);
        assert_eq!(idx[&ANNA], ([3; 16], Some([103; 16])));
        assert_eq!(idx[&BEN], ([4; 16], None));
        assert!(is_dm(DM_NAME) && !is_dm("dm") && DM_NAME.trim() == DM_NAME);
    }

    #[test]
    fn third_party_cannot_hijack_a_dm() {
        const MALLORY: PublicKey = [7; 32];
        let t = tempfile::tempdir().unwrap();
        let c = open_connection(&t.path().join("d.db")).unwrap();
        // Die echte DM mit Anna.
        space(&c, 9, DM_NAME, &[ME, ANNA], true);
        // Mallory legt einen "DM"-Space mit kleinerer ID an und packt uns und Anna hinein.
        space(&c, 1, DM_NAME, &[MALLORY, ME, ANNA], true);
        // Und einen, in dem wir gar nicht Mitglied sind (fremder Space per Sync gepusht).
        space(&c, 2, DM_NAME, &[MALLORY, BEN], true);
        let idx = dm_index(&c, &ME).unwrap();
        assert_eq!(idx.len(), 1, "{idx:?}");
        assert_eq!(idx[&ANNA], ([9; 16], Some([109; 16])));
        assert!(!idx.contains_key(&MALLORY) && !idx.contains_key(&BEN));
    }
}
