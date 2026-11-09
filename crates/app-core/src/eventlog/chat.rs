//! Lesesicht auf Nachrichten fuer die Chat-Ansicht (M2-02): seitenweise, neueste zuerst geladen.

use app_proto::event::{ChannelId, EventId, SpaceId};
use app_proto::PublicKey;
use rusqlite::{params, Connection, OptionalExtension};

/// Position in der Anzeigereihenfolge (HLC, dann Event-ID).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cursor {
    pub wall_ms: i64,
    pub counter: i64,
    pub id: EventId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reply {
    pub author: PublicKey,
    /// Auf 80 Zeichen gekuerzt; leer, wenn die Nachricht geloescht wurde.
    pub preview: String,
    pub deleted: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReactionCount {
    pub emoji: String,
    pub count: u32,
    pub mine: bool,
}

/// Eine an eine Nachricht angehaengte Datei.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attachment {
    pub hash: [u8; 32],
    pub name: String,
    pub size: u64,
    pub mime: String,
    /// Vorschaubild (kleiner eigener Blob), falls der Absender eines erzeugt hat.
    pub thumb: Option<[u8; 32]>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatMessage {
    pub id: EventId,
    pub author: PublicKey,
    pub cursor: Cursor,
    pub body: String,
    pub edited: bool,
    pub deleted: bool,
    pub reply_to: Option<EventId>,
    pub reply: Option<Reply>,
    pub reactions: Vec<ReactionCount>,
    pub attachments: Vec<Attachment>,
    /// Nur bei eigenen Nachrichten in Spaces mit weiteren Mitgliedern.
    pub delivery: Option<Delivery>,
}

/// Wie viele der anderen Mitglieder die Nachricht nachweislich haben.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Delivery {
    pub acked: u32,
    pub total: u32,
}

fn preview(body: &str) -> String {
    let one: String = body.split_whitespace().collect::<Vec<_>>().join(" ");
    if one.chars().count() > 80 {
        let mut s: String = one.chars().take(79).collect();
        s.push('…');
        s
    } else {
        one
    }
}

/// Bis zu `limit` Nachrichten vor `before` (oder die neuesten), aufsteigend sortiert.
/// Leer zurueck heisst: es gibt nichts Aelteres.
pub fn page(
    conn: &Connection,
    space: &SpaceId,
    channel: &ChannelId,
    before: Option<&Cursor>,
    limit: usize,
    me: &PublicKey,
) -> rusqlite::Result<Vec<ChatMessage>> {
    let (w, c, id) = match before {
        Some(b) => (b.wall_ms, b.counter, b.id.to_vec()),
        None => (i64::MAX, i64::MAX, vec![0xFF; 32]),
    };
    let mut st = conn.prepare_cached(
        "SELECT event_id, author, hlc_wall, hlc_counter, body, edited_hlc, deleted, reply_to
         FROM messages
         WHERE channel_id = ?1 AND space_id = ?2
           AND (hlc_wall, hlc_counter, event_id) < (?3, ?4, ?5)
         ORDER BY hlc_wall DESC, hlc_counter DESC, event_id DESC
         LIMIT ?6",
    )?;
    let mut out: Vec<ChatMessage> = st
        .query_map(params![channel, space, w, c, id, limit as i64], |r| {
            let reply_to: Option<EventId> = r.get(7)?;
            Ok(ChatMessage {
                id: r.get(0)?,
                author: r.get(1)?,
                cursor: Cursor {
                    wall_ms: r.get(2)?,
                    counter: r.get(3)?,
                    id: r.get(0)?,
                },
                body: r.get(4)?,
                edited: r.get::<_, Option<i64>>(5)?.is_some(),
                deleted: r.get::<_, i64>(6)? != 0,
                reply_to,
                reply: None,
                reactions: Vec::new(),
                attachments: Vec::new(),
                delivery: None,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    out.reverse();
    let mut rep =
        conn.prepare_cached("SELECT author, body, deleted FROM messages WHERE event_id = ?1")?;
    let mut rx = conn.prepare_cached(
        "SELECT emoji, COUNT(*), MAX(author = ?2) FROM reactions
         WHERE message_event_id = ?1 GROUP BY emoji ORDER BY MIN(rowid)",
    )?;
    let mut att = conn.prepare_cached(
        "SELECT blob_hash, name, size, mime, thumb_hash FROM attachments WHERE message_event_id = ?1 ORDER BY rowid",
    )?;
    let mut seq_of = conn.prepare_cached("SELECT seq FROM events WHERE event_id = ?1")?;
    let mut total_q = conn
        .prepare_cached("SELECT count(*) FROM members WHERE space_id = ?1 AND user_pubkey != ?2")?;
    let mut acked_q = conn.prepare_cached(
        "SELECT count(*) FROM members m JOIN peer_acks a
           ON a.space_id = m.space_id AND a.peer_user = m.user_pubkey AND a.author = ?2
         WHERE m.space_id = ?1 AND m.user_pubkey != ?2 AND a.seq >= ?3",
    )?;
    let total: i64 = total_q.query_row(params![space, me], |r| r.get(0))?;
    for m in &mut out {
        if m.author == *me && total > 0 {
            if let Some(seq) = seq_of
                .query_row([m.id], |r| r.get::<_, i64>(0))
                .optional()?
            {
                let acked: i64 = acked_q.query_row(params![space, me, seq], |r| r.get(0))?;
                m.delivery = Some(Delivery {
                    acked: acked as u32,
                    total: total as u32,
                });
            }
        }
        if let Some(t) = &m.reply_to {
            m.reply = rep
                .query_row([t], |r| {
                    let deleted = r.get::<_, i64>(2)? != 0;
                    Ok(Reply {
                        author: r.get(0)?,
                        preview: if deleted {
                            String::new()
                        } else {
                            preview(&r.get::<_, String>(1)?)
                        },
                        deleted,
                    })
                })
                .optional()?;
        }
        if !m.deleted {
            m.attachments = att
                .query_map([m.id], |r| {
                    Ok(Attachment {
                        hash: r.get(0)?,
                        name: r.get(1)?,
                        size: r.get::<_, i64>(2)? as u64,
                        mime: r.get(3)?,
                        thumb: r.get(4)?,
                    })
                })?
                .collect::<rusqlite::Result<_>>()?;
            m.reactions = rx
                .query_map(params![m.id, me], |r| {
                    Ok(ReactionCount {
                        emoji: r.get(0)?,
                        count: r.get::<_, i64>(1)? as u32,
                        mine: r.get::<_, i64>(2)? != 0,
                    })
                })?
                .collect::<rusqlite::Result<_>>()?;
        }
    }
    Ok(out)
}

/// Hash und Groesse der juengsten Anhaenge (nicht geloeschte Nachrichten), neueste zuerst.
/// Nur aus Spaces, in denen `me` Mitglied ist: ein Freund kann Events fremder Spaces schicken,
/// deren Dateien sollen nicht automatisch geladen werden.
pub fn recent_attachments(
    conn: &Connection,
    me: &PublicKey,
    limit: usize,
) -> rusqlite::Result<Vec<([u8; 32], u64)>> {
    let mut st = conn.prepare_cached(
        "SELECT a.blob_hash, a.size FROM attachments a
           JOIN messages m ON m.event_id = a.message_event_id AND m.deleted = 0
           JOIN members mine ON mine.space_id = m.space_id AND mine.user_pubkey = ?2
         ORDER BY m.hlc_wall DESC, m.hlc_counter DESC LIMIT ?1",
    )?;
    let rows = st.query_map(params![limit as i64, me], |r| {
        Ok((r.get(0)?, r.get::<_, i64>(1)? as u64))
    })?;
    rows.collect()
}

/// Hash und Groesse aller Anhaenge, aelteste Nachricht zuerst (auch geloeschte Nachrichten:
/// deren Dateien braucht niemand mehr). Jede Datei einmal.
pub fn attachments_oldest_first(conn: &Connection) -> rusqlite::Result<Vec<([u8; 32], u64)>> {
    let mut st = conn.prepare_cached(
        "SELECT a.blob_hash, a.size FROM attachments a
           JOIN messages m ON m.event_id = a.message_event_id
         ORDER BY m.deleted DESC, m.hlc_wall ASC, m.hlc_counter ASC",
    )?;
    let rows = st.query_map([], |r| Ok((r.get(0)?, r.get::<_, i64>(1)? as u64)))?;
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for row in rows {
        let (h, size) = row?;
        if seen.insert(h) {
            out.push((h, size));
        }
    }
    Ok(out)
}

/// Darf `user` die Datei (oder ihr Vorschaubild) `hash` von `me` bekommen? Ja, wenn sie in einem
/// Space angehaengt ist (nicht geloeschte Nachricht), in dem **beide** Mitglied sind. Ohne die
/// eigene Mitgliedschaft koennte ein Freund einen eigenen Space mit einem `FileShare` auf einen
/// bekannten Hash schicken und sich so Dateien aus Spaces holen, aus denen er entfernt wurde.
pub fn blob_allowed(
    conn: &Connection,
    me: &PublicKey,
    user: &PublicKey,
    hash: &[u8; 32],
) -> rusqlite::Result<bool> {
    conn.query_row(
        "SELECT EXISTS (
            SELECT 1 FROM attachments a
              JOIN messages m ON m.event_id = a.message_event_id AND m.deleted = 0
              JOIN members mem ON mem.space_id = m.space_id AND mem.user_pubkey = ?2
              JOIN members mine ON mine.space_id = m.space_id AND mine.user_pubkey = ?3
            WHERE a.blob_hash = ?1 OR a.thumb_hash = ?1)",
        params![hash, user, me],
        |r| r.get::<_, i64>(0).map(|v| v != 0),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::open_connection;

    #[test]
    fn blobs_go_only_to_members_of_the_space_that_has_them() {
        let tmp = tempfile::tempdir().unwrap();
        let conn = open_connection(&tmp.path().join("c.db")).unwrap();
        let (space_a, space_b) = ([1u8; 16], [2u8; 16]);
        let (alice, bob, carol) = ([10u8; 32], [11u8; 32], [12u8; 32]);
        let (h_a, h_b, t_a) = ([0xAA; 32], [0xBB; 32], [0xAD; 32]);
        for (sp, u) in [(space_a, alice), (space_a, bob), (space_b, carol)] {
            conn.execute(
                "INSERT INTO members (space_id, user_pubkey, added_by) VALUES (?1, ?2, ?2)",
                params![sp, u],
            )
            .unwrap();
        }
        for (i, (sp, h)) in [(space_a, h_a), (space_b, h_b)].into_iter().enumerate() {
            let ev = id(i as u32);
            conn.execute(
                "INSERT INTO messages (space_id, channel_id, event_id, author, hlc_wall, hlc_counter, body)
                 VALUES (?1, ?2, ?3, ?4, 1, 0, '')",
                params![sp, [3u8; 16], ev, alice],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO attachments (message_event_id, blob_hash, name, size, mime, thumb_hash) VALUES (?1, ?2, 'f', 1, 'x', ?3)",
                params![ev, h, (sp == space_a).then_some(t_a)],
            )
            .unwrap();
        }
        assert!(blob_allowed(&conn, &alice, &bob, &h_a).unwrap());
        assert!(
            !blob_allowed(&conn, &alice, &carol, &h_a).unwrap(),
            "anderer Space"
        );
        assert!(!blob_allowed(&conn, &alice, &bob, &h_b).unwrap());
        // Bob ist in Space A, aber ich (Carol) nicht: dann gebe ich die Datei auch nicht heraus,
        // selbst wenn ein Space mit diesem Anhang bei mir gespeichert ist.
        assert!(!blob_allowed(&conn, &carol, &bob, &h_a).unwrap());
        // Das Vorschaubild folgt denselben Regeln wie die Datei.
        assert!(blob_allowed(&conn, &alice, &bob, &t_a).unwrap());
        assert!(!blob_allowed(&conn, &alice, &carol, &t_a).unwrap());
        assert!(
            !blob_allowed(&conn, &alice, &bob, &[0xCC; 32]).unwrap(),
            "unbekannte Datei"
        );
        // Geloeschte Nachricht gibt die Datei nicht mehr frei.
        conn.execute("UPDATE messages SET deleted = 1", []).unwrap();
        assert!(!blob_allowed(&conn, &alice, &bob, &h_a).unwrap());
    }

    #[test]
    fn recent_attachments_are_newest_first_limited_and_skip_deleted() {
        let tmp = tempfile::tempdir().unwrap();
        let conn = open_connection(&tmp.path().join("c.db")).unwrap();
        conn.execute(
            "INSERT INTO members (space_id, user_pubkey, added_by) VALUES (?1, ?2, ?2)",
            params![[1u8; 16], [9u8; 32]],
        )
        .unwrap();
        for i in 0..6u32 {
            // Nachricht 5 liegt in einem Space, in dem ich nicht Mitglied bin.
            let space = if i == 5 { [8u8; 16] } else { [1u8; 16] };
            conn.execute(
                "INSERT INTO messages (space_id, channel_id, event_id, author, hlc_wall, hlc_counter, body, deleted)
                 VALUES (?1, ?2, ?3, ?4, ?5, 0, '', ?6)",
                params![space, [2u8; 16], id(i), [9u8; 32], i64::from(i), i64::from(i == 3)],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO attachments (message_event_id, blob_hash, name, size, mime) VALUES (?1, ?2, 'f', ?3, 'x')",
                params![id(i), [i as u8; 32], i64::from(i) * 10],
            )
            .unwrap();
        }
        let r = recent_attachments(&conn, &[9u8; 32], 3).unwrap();
        assert_eq!(r, vec![([4u8; 32], 40), ([2u8; 32], 20), ([1u8; 32], 10)]);
    }

    #[test]
    fn attachments_come_oldest_first_deleted_ones_before_everything() {
        let tmp = tempfile::tempdir().unwrap();
        let conn = open_connection(&tmp.path().join("c.db")).unwrap();
        for i in 0..4u32 {
            conn.execute(
                "INSERT INTO messages (space_id, channel_id, event_id, author, hlc_wall, hlc_counter, body, deleted)
                 VALUES (?1, ?2, ?3, ?4, ?5, 0, '', ?6)",
                params![[1u8; 16], [2u8; 16], id(i), [9u8; 32], i64::from(i), i64::from(i == 2)],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO attachments (message_event_id, blob_hash, name, size, mime) VALUES (?1, ?2, 'f', ?3, 'x')",
                params![id(i), [i as u8; 32], i64::from(i) * 10],
            )
            .unwrap();
        }
        let r: Vec<u8> = attachments_oldest_first(&conn)
            .unwrap()
            .into_iter()
            .map(|(h, _)| h[0])
            .collect();
        assert_eq!(r, vec![2, 0, 1, 3]);
    }

    #[test]
    fn page_lists_attachments() {
        let tmp = tempfile::tempdir().unwrap();
        let conn = open_connection(&tmp.path().join("c.db")).unwrap();
        let ev = id(1);
        conn.execute(
            "INSERT INTO messages (space_id, channel_id, event_id, author, hlc_wall, hlc_counter, body)
             VALUES (?1, ?2, ?3, ?4, 1, 0, '')",
            params![[1u8; 16], [2u8; 16], ev, [9u8; 32]],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO attachments (message_event_id, blob_hash, name, size, mime) VALUES (?1, ?2, 'a.png', 5, 'image/png')",
            params![ev, [7u8; 32]],
        )
        .unwrap();
        let p = page(&conn, &[1u8; 16], &[2u8; 16], None, 10, &[9u8; 32]).unwrap();
        assert_eq!(p[0].attachments.len(), 1);
        assert_eq!(p[0].attachments[0].name, "a.png");
        assert_eq!(p[0].attachments[0].size, 5);
    }

    use std::time::Instant;

    fn id(n: u32) -> EventId {
        let mut x = [0u8; 32];
        x[..4].copy_from_slice(&n.to_be_bytes());
        x
    }

    fn setup(n: u32) -> (tempfile::TempDir, Connection) {
        let tmp = tempfile::tempdir().unwrap();
        let conn = open_connection(&tmp.path().join("c.db")).unwrap();
        conn.execute_batch("BEGIN").unwrap();
        {
            let mut ins = conn
                .prepare(
                    "INSERT INTO messages (space_id, channel_id, event_id, author, hlc_wall, hlc_counter, body, reply_to)
                     VALUES (?1, ?2, ?3, ?4, ?5, 0, ?6, ?7)",
                )
                .unwrap();
            for i in 0..n {
                let reply = (i % 10 == 5).then(|| id(i - 1));
                ins.execute(params![
                    [1u8; 16],
                    [2u8; 16],
                    id(i),
                    [(i % 5) as u8; 32],
                    1_800_000_000_000i64 + i as i64,
                    format!("Nachricht {i}"),
                    reply
                ])
                .unwrap();
            }
        }
        conn.execute_batch("COMMIT").unwrap();
        (tmp, conn)
    }

    #[test]
    fn pages_walk_back_through_history_without_gaps() {
        let (_t, conn) = setup(120);
        let me = [0u8; 32];
        let mut seen = Vec::new();
        let mut before: Option<Cursor> = None;
        loop {
            let p = page(&conn, &[1; 16], &[2; 16], before.as_ref(), 50, &me).unwrap();
            if p.is_empty() {
                break;
            }
            assert!(p
                .windows(2)
                .all(|w| w[0].cursor.wall_ms < w[1].cursor.wall_ms));
            before = Some(p[0].cursor.clone());
            seen.splice(0..0, p.into_iter().map(|m| m.id));
        }
        assert_eq!(seen, (0..120).map(id).collect::<Vec<_>>());
    }

    #[test]
    fn reactions_replies_and_deleted_messages() {
        let (_t, conn) = setup(10);
        let me = [0u8; 32];
        for (a, e) in [([0u8; 32], "👍"), ([1u8; 32], "👍"), ([1u8; 32], "🎉")] {
            conn.execute(
                "INSERT INTO reactions (message_event_id, author, emoji) VALUES (?1, ?2, ?3)",
                params![id(9), a, e],
            )
            .unwrap();
        }
        conn.execute(
            "UPDATE messages SET body = '', deleted = 1 WHERE event_id = ?1",
            [id(4)],
        )
        .unwrap();
        let p = page(&conn, &[1; 16], &[2; 16], None, 50, &me).unwrap();
        let last = p.last().unwrap();
        assert_eq!(last.reactions.len(), 2);
        assert_eq!((last.reactions[0].count, last.reactions[0].mine), (2, true));
        assert_eq!(
            (last.reactions[1].count, last.reactions[1].mine),
            (1, false)
        );
        // Nachricht 5 antwortet auf die geloeschte 4.
        assert!(p[5].reply.as_ref().unwrap().deleted);
        assert!(p[4].deleted && p[4].body.is_empty());
        assert_eq!(preview("a\n\n  b"), "a b");
    }

    #[test]
    fn delivery_counts_members_who_have_the_message() {
        let (_t, conn) = setup(3);
        let me = [0u8; 32];
        let (b, c) = ([7u8; 32], [8u8; 32]);
        for u in [me, b, c] {
            conn.execute(
                "INSERT INTO members (space_id, user_pubkey, added_by) VALUES (?1, ?2, ?2)",
                params![[1u8; 16], u],
            )
            .unwrap();
        }
        // Nachricht 0 gehoert `me` (Autor 0 ist [0;32]) und hat seq 1; Nachricht 3 gibt es nicht.
        conn.execute(
            "INSERT INTO events (event_id, space_id, author, seq, hlc_wall, hlc_counter, kind, payload, signature, received_at)
             VALUES (?1, ?2, ?3, 5, 0, 0, 'message', x'', x'', 0)",
            params![id(0), [1u8; 16], me],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO author_heads (space_id, author, max_seq) VALUES (?1, ?2, 5)",
            params![[1u8; 16], me],
        )
        .unwrap();
        let get =
            |conn: &Connection| page(conn, &[1; 16], &[2; 16], None, 50, &me).unwrap()[0].delivery;
        assert_eq!(get(&conn), Some(Delivery { acked: 0, total: 2 }));
        let heads = |seq| {
            vec![app_proto::sync::SpaceHeads {
                space_id: [1; 16],
                authors: vec![(me, seq)],
            }]
        };
        crate::eventlog::record_acks(&conn, &b, &heads(4)).unwrap();
        assert_eq!(get(&conn), Some(Delivery { acked: 0, total: 2 }));
        crate::eventlog::record_acks(&conn, &b, &heads(5)).unwrap();
        assert_eq!(get(&conn), Some(Delivery { acked: 1, total: 2 }));
        // Der Wert wird nie kleiner; ein Nicht-Mitglied zaehlt nicht.
        crate::eventlog::record_acks(&conn, &b, &heads(1)).unwrap();
        crate::eventlog::record_acks(&conn, &[9u8; 32], &heads(9)).unwrap();
        assert_eq!(get(&conn), Some(Delivery { acked: 1, total: 2 }));
        crate::eventlog::record_acks(&conn, &c, &heads(6)).unwrap();
        assert_eq!(get(&conn), Some(Delivery { acked: 2, total: 2 }));
        // Gespeichert wird hoechstens der eigene Stand, nichts fuer Nicht-Mitglieder oder
        // erfundene Spaces/Autoren.
        let acks = |conn: &Connection| -> Vec<(Vec<u8>, i64)> {
            conn.prepare("SELECT peer_user, seq FROM peer_acks ORDER BY peer_user")
                .unwrap()
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap()
        };
        crate::eventlog::record_acks(
            &conn,
            &c,
            &[app_proto::sync::SpaceHeads {
                space_id: [6; 16],
                authors: vec![(me, 3), ([5; 32], 3)],
            }],
        )
        .unwrap();
        assert_eq!(acks(&conn), vec![(b.to_vec(), 5), (c.to_vec(), 5)]);
        // Fremde Nachrichten haben keinen Status.
        assert_eq!(
            page(&conn, &[1; 16], &[2; 16], None, 50, &me).unwrap()[1].delivery,
            None
        );
    }

    #[test]
    fn newest_page_of_100k_messages_loads_fast() {
        let (_t, conn) = setup(100_000);
        let me = [0u8; 32];
        let t = Instant::now();
        let p = page(&conn, &[1; 16], &[2; 16], None, 50, &me).unwrap();
        let ms = t.elapsed().as_millis();
        assert_eq!(p.len(), 50);
        assert_eq!(p.last().unwrap().id, id(99_999));
        assert!(ms <= 100, "{ms} ms");
        println!("100k: neueste Seite in {ms} ms");
    }
}
