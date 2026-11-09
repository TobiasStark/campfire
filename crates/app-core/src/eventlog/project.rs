//! Projektion: Event -> Tabellen. Prueft dabei die Berechtigung.
//!
//! `apply` wird in Anzeigereihenfolge aufgerufen. Nicht erlaubte Events bleiben im Log,
//! wirken aber nicht (alle Clients entscheiden dabei gleich).

use app_proto::event::{ChannelKind, EventBody, EventId, EventKind, SpaceId};
use app_proto::PublicKey;
use rusqlite::{params, Connection};

/// Loescht die Projektion eines Spaces (der `spaces`-Eintrag bleibt: lokale Felder).
pub fn reset(conn: &Connection, space: &SpaceId) -> rusqlite::Result<()> {
    conn.execute(
        "DELETE FROM reactions WHERE message_event_id IN (SELECT event_id FROM messages WHERE space_id = ?1)",
        [space],
    )?;
    conn.execute(
        "DELETE FROM attachments WHERE message_event_id IN (SELECT event_id FROM messages WHERE space_id = ?1)",
        [space],
    )?;
    conn.execute("DELETE FROM messages WHERE space_id = ?1", [space])?;
    conn.execute("DELETE FROM channels WHERE space_id = ?1", [space])?;
    conn.execute("DELETE FROM members WHERE space_id = ?1", [space])?;
    Ok(())
}

fn role_of(
    conn: &Connection,
    space: &SpaceId,
    user: &PublicKey,
) -> rusqlite::Result<Option<String>> {
    use rusqlite::OptionalExtension;
    conn.prepare_cached("SELECT role FROM members WHERE space_id = ?1 AND user_pubkey = ?2")?
        .query_row(params![space, user], |r| r.get(0))
        .optional()
}

fn channel_kind(
    conn: &Connection,
    space: &SpaceId,
    ch: &[u8; 16],
) -> rusqlite::Result<Option<String>> {
    use rusqlite::OptionalExtension;
    conn.prepare_cached("SELECT kind FROM channels WHERE space_id = ?1 AND channel_id = ?2")?
        .query_row(params![space, ch], |r| r.get(0))
        .optional()
}

/// (Autor, geloescht) der Nachricht, falls sie in diesem Space existiert.
fn message(
    conn: &Connection,
    space: &SpaceId,
    id: &EventId,
) -> rusqlite::Result<Option<(PublicKey, bool)>> {
    use rusqlite::OptionalExtension;
    conn.prepare_cached(
        "SELECT author, deleted FROM messages WHERE space_id = ?1 AND event_id = ?2",
    )?
    .query_row(params![space, id], |r| {
        Ok((r.get(0)?, r.get::<_, i64>(1)? != 0))
    })
    .optional()
}

/// Wendet ein Event an. `false`, wenn es nicht erlaubt ist (dann bleibt alles unveraendert).
pub fn apply(conn: &Connection, b: &EventBody, id: &EventId, now_s: i64) -> rusqlite::Result<bool> {
    let space = &b.space_id;
    if let EventKind::SpaceCreate { name } = &b.kind {
        if role_of(conn, space, &b.author)?.is_some()
            || conn
                .prepare_cached("SELECT 1 FROM members WHERE space_id = ?1 LIMIT 1")?
                .exists([space])?
        {
            return Ok(false);
        }
        conn.execute(
            "INSERT INTO spaces (space_id, name, joined_at) VALUES (?1, ?2, ?3)
             ON CONFLICT (space_id) DO UPDATE SET name = ?2",
            params![space, name, now_s],
        )?;
        conn.execute(
            "INSERT INTO members (space_id, user_pubkey, added_by, role) VALUES (?1, ?2, ?2, 'admin')",
            params![space, b.author],
        )?;
        return Ok(true);
    }
    let Some(role) = role_of(conn, space, &b.author)? else {
        return Ok(false);
    };
    let admin = role == "admin";
    match &b.kind {
        EventKind::SpaceCreate { .. } => unreachable!(),
        EventKind::MemberAdd { user } => {
            if !admin || role_of(conn, space, user)?.is_some() {
                return Ok(false);
            }
            conn.execute(
                "INSERT INTO members (space_id, user_pubkey, added_by, role) VALUES (?1, ?2, ?3, 'member')",
                params![space, user, b.author],
            )?;
        }
        EventKind::MemberRemove { user } => {
            let allowed = admin || *user == b.author;
            match role_of(conn, space, user)? {
                Some(r) if allowed && r != "admin" => {
                    conn.execute(
                        "DELETE FROM members WHERE space_id = ?1 AND user_pubkey = ?2",
                        params![space, user],
                    )?;
                }
                _ => return Ok(false),
            }
        }
        EventKind::ChannelCreate {
            channel,
            kind,
            name,
            position,
        } => {
            if !admin || channel_kind(conn, space, channel)?.is_some() {
                return Ok(false);
            }
            let k = match kind {
                ChannelKind::Voice => "voice",
                ChannelKind::Text => "text",
            };
            conn.execute(
                "INSERT INTO channels (space_id, channel_id, kind, name, position) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![space, channel, k, name, position],
            )?;
        }
        EventKind::ChannelUpdate {
            channel,
            name,
            position,
        } => {
            if !admin || channel_kind(conn, space, channel)?.is_none() {
                return Ok(false);
            }
            conn.execute(
                "UPDATE channels SET name = coalesce(?3, name), position = coalesce(?4, position)
                 WHERE space_id = ?1 AND channel_id = ?2",
                params![space, channel, name, position],
            )?;
        }
        EventKind::ChannelDelete { channel } => {
            if !admin || channel_kind(conn, space, channel)?.is_none() {
                return Ok(false);
            }
            conn.execute(
                "DELETE FROM channels WHERE space_id = ?1 AND channel_id = ?2",
                params![space, channel],
            )?;
        }
        EventKind::Message {
            channel,
            body,
            reply_to,
        } => {
            if channel_kind(conn, space, channel)?.as_deref() != Some("text") {
                return Ok(false);
            }
            conn.prepare_cached(
                "INSERT INTO messages (space_id, channel_id, event_id, author, hlc_wall, hlc_counter, body, reply_to)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            )?
            .execute(params![
                space,
                channel,
                id,
                b.author,
                b.hlc.wall_ms as i64,
                b.hlc.counter as i64,
                body,
                reply_to
            ])?;
        }
        EventKind::MessageEdit { target, body } => match message(conn, space, target)? {
            Some((author, false)) if author == b.author => {
                conn.execute(
                    "UPDATE messages SET body = ?2, edited_hlc = ?3 WHERE event_id = ?1",
                    params![target, body, b.hlc.wall_ms as i64],
                )?;
            }
            _ => return Ok(false),
        },
        EventKind::MessageDelete { target } => match message(conn, space, target)? {
            Some((author, false)) if author == b.author || admin => {
                conn.execute(
                    "UPDATE messages SET body = '', deleted = 1 WHERE event_id = ?1",
                    [target],
                )?;
                conn.execute(
                    "DELETE FROM reactions WHERE message_event_id = ?1",
                    [target],
                )?;
                conn.execute(
                    "DELETE FROM attachments WHERE message_event_id = ?1",
                    [target],
                )?;
            }
            _ => return Ok(false),
        },
        EventKind::Reaction { target, emoji } => match message(conn, space, target)? {
            Some((_, false)) => {
                conn.execute(
                    "INSERT OR IGNORE INTO reactions (message_event_id, author, emoji) VALUES (?1, ?2, ?3)",
                    params![target, b.author, emoji],
                )?;
            }
            _ => return Ok(false),
        },
        EventKind::ReactionRemove { target, emoji } => {
            let n = conn.execute(
                "DELETE FROM reactions WHERE message_event_id = ?1 AND author = ?2 AND emoji = ?3",
                params![target, b.author, emoji],
            )?;
            if n == 0 {
                return Ok(false);
            }
        }
        EventKind::FileShare {
            channel,
            blob_hash,
            name,
            size,
            mime,
            thumb_hash,
        } => {
            if channel_kind(conn, space, channel)?.as_deref() != Some("text") {
                return Ok(false);
            }
            conn.execute(
                "INSERT INTO messages (space_id, channel_id, event_id, author, hlc_wall, hlc_counter, body)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, '')",
                params![space, channel, id, b.author, b.hlc.wall_ms as i64, b.hlc.counter as i64],
            )?;
            conn.execute(
                "INSERT INTO attachments (message_event_id, blob_hash, name, size, mime, thumb_hash)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![id, blob_hash, name, *size as i64, mime, thumb_hash],
            )?;
        }
    }
    Ok(true)
}
