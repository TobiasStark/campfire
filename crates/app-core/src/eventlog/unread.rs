//! Lesestand (M2-05): ungelesene Nachrichten und Erwaehnungen je Channel und Space.
//!
//! Der Lesestand ist die HLC-Zeit (ms) der zuletzt gelesenen Nachricht je Channel und liegt in
//! `read_state`; er bleibt damit ueber Neustarts erhalten. Eigene und geloeschte Nachrichten
//! zaehlen nie als ungelesen.

use app_proto::event::{ChannelId, SpaceId};
use app_proto::PublicKey;
use rusqlite::{params, Connection, OptionalExtension};
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Unread {
    pub unread: u32,
    pub mentions: u32,
}

/// LIKE-Muster fuer `@name` (Gross-/Kleinschreibung egal bei ASCII), Sonderzeichen entschaerft.
fn mention_pattern(name: &str) -> String {
    let mut p = String::from("%@");
    for c in name.chars() {
        if matches!(c, '%' | '_' | '!') {
            p.push('!');
        }
        p.push(c);
    }
    p.push('%');
    p
}

/// Ob `body` den Namen erwaehnt (`@name`, nicht mitten in einem laengeren Wort).
pub fn mentions(body: &str, name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    let (b, n) = (body.to_lowercase(), format!("@{}", name.to_lowercase()));
    let mut from = 0;
    while let Some(i) = b[from..].find(&n) {
        let end = from + i + n.len();
        if !b[end..].chars().next().is_some_and(char::is_alphanumeric) {
            return true;
        }
        from = end;
    }
    false
}

pub fn last_read(conn: &Connection, channel: &ChannelId) -> rusqlite::Result<i64> {
    Ok(conn
        .query_row(
            "SELECT last_read_hlc FROM read_state WHERE channel_id = ?1",
            [channel],
            |r| r.get(0),
        )
        .optional()?
        .unwrap_or(0))
}

/// Setzt den Lesestand (nie rueckwaerts).
pub fn mark_read(conn: &Connection, channel: &ChannelId, wall_ms: i64) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO read_state (channel_id, last_read_hlc) VALUES (?1, ?2)
         ON CONFLICT (channel_id) DO UPDATE SET last_read_hlc = max(last_read_hlc, excluded.last_read_hlc)",
        params![channel, wall_ms],
    )?;
    Ok(())
}

/// Neueste Nachricht des Channels (HLC-ms), fuer `mark_read`.
pub fn newest(conn: &Connection, channel: &ChannelId) -> rusqlite::Result<Option<i64>> {
    conn.query_row(
        "SELECT max(hlc_wall) FROM messages WHERE channel_id = ?1",
        [channel],
        |r| r.get(0),
    )
}

/// Zaehlt je Gruppe (Channel oder Space) die ungelesenen Nachrichten, die `my_name` erwaehnen.
/// SQL grenzt per LIKE grob ein, `mentions` prueft genau (`@tobias` ist keine Erwaehnung von `tobi`).
fn count_mentions<K: std::hash::Hash + Eq + rusqlite::types::FromSql>(
    conn: &Connection,
    sql: &str,
    args: &[&dyn rusqlite::ToSql],
    my_name: &str,
) -> rusqlite::Result<HashMap<K, u32>> {
    let mut st = conn.prepare_cached(sql)?;
    let mut out = HashMap::new();
    let mut rows = st.query(args)?;
    while let Some(r) = rows.next()? {
        let (key, body): (K, String) = (r.get(0)?, r.get(1)?);
        if mentions(&body, my_name) {
            *out.entry(key).or_insert(0) += 1;
        }
    }
    Ok(out)
}

/// Ungelesenes je Channel eines Spaces (nur Channels mit ungelesenen Nachrichten).
pub fn channel_unread(
    conn: &Connection,
    space: &SpaceId,
    me: &PublicKey,
    my_name: &str,
) -> rusqlite::Result<HashMap<ChannelId, Unread>> {
    let mut st = conn.prepare_cached(
        "SELECT m.channel_id, COUNT(*)
         FROM messages m LEFT JOIN read_state r ON r.channel_id = m.channel_id
         WHERE m.space_id = ?1 AND m.author != ?2 AND m.deleted = 0
           AND m.hlc_wall > COALESCE(r.last_read_hlc, 0)
         GROUP BY m.channel_id",
    )?;
    let mut out: HashMap<ChannelId, Unread> = st
        .query_map(params![space, me], |r| {
            Ok((
                r.get::<_, ChannelId>(0)?,
                Unread {
                    unread: r.get::<_, i64>(1)? as u32,
                    mentions: 0,
                },
            ))
        })?
        .collect::<rusqlite::Result<_>>()?;
    let pat = mention_pattern(my_name);
    let ments: HashMap<ChannelId, u32> = count_mentions(
        conn,
        "SELECT m.channel_id, m.body
         FROM messages m LEFT JOIN read_state r ON r.channel_id = m.channel_id
         WHERE m.space_id = ?1 AND m.author != ?2 AND m.deleted = 0
           AND m.hlc_wall > COALESCE(r.last_read_hlc, 0) AND m.body LIKE ?3 ESCAPE '!'",
        &[space, me, &pat],
        my_name,
    )?;
    for (k, n) in ments {
        if let Some(u) = out.get_mut(&k) {
            u.mentions = n;
        }
    }
    Ok(out)
}

/// Summe je Space (fuer die Space-Leiste).
pub fn space_unread(
    conn: &Connection,
    me: &PublicKey,
    my_name: &str,
) -> rusqlite::Result<HashMap<SpaceId, Unread>> {
    let mut st = conn.prepare_cached(
        "SELECT m.space_id, COUNT(*)
         FROM messages m LEFT JOIN read_state r ON r.channel_id = m.channel_id
         JOIN members mine ON mine.space_id = m.space_id AND mine.user_pubkey = ?1
         WHERE m.author != ?1 AND m.deleted = 0
           AND m.hlc_wall > COALESCE(r.last_read_hlc, 0)
         GROUP BY m.space_id",
    )?;
    let mut out: HashMap<SpaceId, Unread> = st
        .query_map([me], |r| {
            Ok((
                r.get::<_, SpaceId>(0)?,
                Unread {
                    unread: r.get::<_, i64>(1)? as u32,
                    mentions: 0,
                },
            ))
        })?
        .collect::<rusqlite::Result<_>>()?;
    let pat = mention_pattern(my_name);
    let ments: HashMap<SpaceId, u32> = count_mentions(
        conn,
        "SELECT m.space_id, m.body
         FROM messages m LEFT JOIN read_state r ON r.channel_id = m.channel_id
         JOIN members mine ON mine.space_id = m.space_id AND mine.user_pubkey = ?1
         WHERE m.author != ?1 AND m.deleted = 0
           AND m.hlc_wall > COALESCE(r.last_read_hlc, 0) AND m.body LIKE ?2 ESCAPE '!'",
        &[me, &pat],
        my_name,
    )?;
    for (k, n) in ments {
        if let Some(u) = out.get_mut(&k) {
            u.mentions = n;
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::open_connection;

    const SP: SpaceId = [1; 16];
    const CH: ChannelId = [2; 16];
    const ME: PublicKey = [9; 32];
    const OTHER: PublicKey = [5; 32];

    fn add(conn: &Connection, n: u8, author: PublicKey, wall: i64, body: &str, ch: ChannelId) {
        conn.execute(
            "INSERT INTO messages (space_id, channel_id, event_id, author, hlc_wall, hlc_counter, body)
             VALUES (?1, ?2, ?3, ?4, ?5, 0, ?6)",
            params![SP, ch, [n; 32], author, wall, body],
        )
        .unwrap();
    }

    fn db() -> (tempfile::TempDir, Connection) {
        let t = tempfile::tempdir().unwrap();
        let c = open_connection(&t.path().join("u.db")).unwrap();
        (t, c)
    }

    #[test]
    fn counts_unread_and_mentions_and_ignores_own_and_deleted() {
        let (_t, c) = db();
        add(&c, 1, OTHER, 100, "hallo", CH);
        add(&c, 2, OTHER, 200, "hey @Tobi wie gehts", CH);
        add(&c, 3, ME, 300, "eigene", CH);
        add(&c, 4, OTHER, 400, "@tobias nicht ich", CH);
        add(&c, 5, OTHER, 500, "weg", CH);
        c.execute(
            "UPDATE messages SET deleted = 1 WHERE event_id = ?1",
            [[5u8; 32]],
        )
        .unwrap();
        add(&c, 6, OTHER, 600, "andere @tobi", [3; 16]);
        let u = channel_unread(&c, &SP, &ME, "tobi").unwrap();
        assert_eq!(
            u[&CH],
            Unread {
                unread: 3,
                mentions: 1
            }
        );
        assert_eq!(
            u[&[3; 16]],
            Unread {
                unread: 1,
                mentions: 1
            }
        );
        assert!(mentions("hey @Tobi wie gehts", "tobi"));
        assert!(!mentions("@tobias", "tobi"));
        assert!(mentions("@tobi, du", "Tobi"));
        assert!(!mentions("mail@tobi.de", "") && !mentions("x", ""));
        // Spaces, in denen ich nicht Mitglied bin (fremd zugeschickt), zaehlen nicht.
        assert!(space_unread(&c, &ME, "tobi").unwrap().is_empty());
        c.execute(
            "INSERT INTO members (space_id, user_pubkey, added_by) VALUES (?1, ?2, ?2)",
            params![SP, ME],
        )
        .unwrap();
        let s = space_unread(&c, &ME, "tobi").unwrap();
        assert_eq!(s[&SP].unread, 4);
    }

    #[test]
    fn read_state_moves_forward_only_and_filters() {
        let (_t, c) = db();
        for i in 1..=5u8 {
            add(&c, i, OTHER, i as i64 * 100, "x", CH);
        }
        assert_eq!(last_read(&c, &CH).unwrap(), 0);
        mark_read(&c, &CH, 300).unwrap();
        assert_eq!(channel_unread(&c, &SP, &ME, "x").unwrap()[&CH].unread, 2);
        mark_read(&c, &CH, 100).unwrap();
        assert_eq!(last_read(&c, &CH).unwrap(), 300);
        mark_read(&c, &CH, newest(&c, &CH).unwrap().unwrap()).unwrap();
        assert!(channel_unread(&c, &SP, &ME, "x").unwrap().is_empty());
    }

    #[test]
    fn special_characters_in_names_do_not_break_the_pattern() {
        let (_t, c) = db();
        add(&c, 1, OTHER, 100, "@a_b hallo", CH);
        add(&c, 2, OTHER, 200, "@axb hallo", CH);
        let u = channel_unread(&c, &SP, &ME, "a_b").unwrap();
        assert_eq!(
            u[&CH],
            Unread {
                unread: 2,
                mentions: 1
            }
        );
    }

    #[test]
    fn read_state_survives_reopening_the_database() {
        let t = tempfile::tempdir().unwrap();
        let path = t.path().join("r.db");
        {
            let c = open_connection(&path).unwrap();
            add(&c, 1, OTHER, 100, "x", CH);
            mark_read(&c, &CH, 100).unwrap();
        }
        let c = open_connection(&path).unwrap();
        assert_eq!(last_read(&c, &CH).unwrap(), 100);
        assert!(channel_unread(&c, &SP, &ME, "x").unwrap().is_empty());
    }
}
