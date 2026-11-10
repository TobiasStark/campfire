//! Speichergrenze fuer Nachrichten.
//!
//! Wird der Platz fuer Nachrichten groesser als eingestellt, werden die aeltesten Nachrichten
//! des Autors aufgeraeumt, der am meisten Platz belegt (eigene zuletzt). So verdraengt ein
//! Spammer zuerst seine eigenen Nachrichten und nicht die Historie aller anderen.
//!
//! Aufgeraeumte Events bleiben als Platzhalter ([`EventKind::Pruned`]) im Log: gleiche ID,
//! gleiche Signatur, ohne Inhalt. Die Folge eines Autors bleibt damit lueckenlos und fuer
//! andere pruefbar, nur der Inhalt fehlt. Mitglieder-, Space- und Channel-Events werden nie
//! aufgeraeumt. Mit einer Nachricht verschwindet alles, was sich auf sie bezieht
//! (Bearbeitungen, Loeschung, Reaktionen, auch von anderen).

use app_proto::event::{EventId, EventKind, SpaceId};
use app_proto::PublicKey;
use rusqlite::{params, Connection, OptionalExtension};
use std::collections::{HashMap, HashSet};

/// Standard der Speichergrenze (MB).
pub const LIMIT_MB_DEFAULT: u64 = 1024;
/// Kleinste einstellbare Grenze (MB).
pub const LIMIT_MB_MIN: u64 = 100;

/// Geschaetzter Platz je Event-Zeile ohne Inhalt (Schluessel, Signatur, Indizes).
const EVENT_ROW: u64 = 350;
/// Geschaetzter Platz je Zeile der Projektion (Nachricht, Anhang, Reaktion) ohne Text.
const PROJECTION_ROW: u64 = 120;
/// Events je Transaktion.
const BATCH: i64 = 1000;

/// Arten, die als Erstes aufgeraeumt werden duerfen. `MessageDelete` nur zusammen mit seiner
/// Nachricht: allein aufgeraeumt kaeme die geloeschte Nachricht zurueck.
const CANDIDATES: &str = "('Message', 'MessageEdit', 'Reaction', 'ReactionRemove', 'FileShare')";

#[derive(Debug, Default, PartialEq, Eq)]
pub struct PruneOutcome {
    /// Zu Platzhaltern gewordene Events.
    pub pruned: usize,
    /// Spaces, deren Ansicht sich geaendert hat.
    pub spaces: HashSet<SpaceId>,
}

/// Geschaetzter Platz der Nachrichten-Daten (Events und Projektion) in Bytes. Die Datei kann
/// etwas groesser sein (freie Seiten werden wiederverwendet, aber nicht zurueckgegeben).
pub fn stored_bytes(conn: &Connection) -> rusqlite::Result<u64> {
    let q = |sql: &str| conn.query_row(sql, [], |r| r.get::<_, i64>(0));
    let total = q(&format!(
        "SELECT COALESCE(SUM(length(payload)), 0) + COUNT(*) * {EVENT_ROW} FROM events"
    ))? + q(&format!(
        "SELECT COALESCE(SUM(length(body)), 0) + COUNT(*) * {PROJECTION_ROW} FROM messages"
    ))? + q(&format!(
        "SELECT COALESCE(SUM(length(name) + length(mime)), 0) + COUNT(*) * {PROJECTION_ROW}
           FROM attachments"
    ))? + q(&format!(
        "SELECT COALESCE(SUM(length(emoji)), 0) + COUNT(*) * {PROJECTION_ROW} FROM reactions"
    ))?;
    Ok(total.max(0) as u64)
}

/// Raeumt auf, bis der Platz unter 90 % von `max_bytes` liegt. `me`: eigene Nachrichten
/// kommen zuletzt dran.
pub fn enforce(
    conn: &mut Connection,
    me: &PublicKey,
    max_bytes: u64,
) -> rusqlite::Result<PruneOutcome> {
    let mut out = PruneOutcome::default();
    let mut used = stored_bytes(conn)?;
    if used <= max_bytes {
        return Ok(out);
    }
    let target = max_bytes / 10 * 9;
    // Was je (Space, Autor) frei werden kann; einmal zaehlen, danach mitfuehren.
    let mut groups: HashMap<(SpaceId, PublicKey), u64> = HashMap::new();
    {
        let mut st = conn.prepare(&format!(
            "SELECT space_id, author, SUM(length(payload)) + COUNT(*) * {PROJECTION_ROW}
               FROM events WHERE pruned = 0 AND kind IN {CANDIDATES}
              GROUP BY space_id, author"
        ))?;
        let rows = st.query_map([], |r| {
            Ok(((r.get(0)?, r.get(1)?), r.get::<_, i64>(2)?.max(0) as u64))
        })?;
        for row in rows {
            let (k, v) = row?;
            groups.insert(k, v);
        }
    }
    while used > target {
        // Fremde vor eigenen, dann wer am meisten belegt.
        let Some(key) = groups
            .iter()
            .filter(|(_, v)| **v > 0)
            .max_by_key(|((_, author), v)| (author != me, **v))
            .map(|(k, _)| *k)
        else {
            break;
        };
        let tx = conn.transaction()?;
        let (freed, by_group) = prune_oldest(&tx, &key.0, &key.1, used - target, &mut out)?;
        tx.commit()?;
        if by_group.is_empty() {
            // Nichts mehr da (Schaetzung war zu hoch): diese Gruppe ist erledigt.
            groups.remove(&key);
            continue;
        }
        for (k, n) in by_group {
            if let Some(v) = groups.get_mut(&k) {
                *v = v.saturating_sub(n);
            }
        }
        used = used.saturating_sub(freed);
    }
    Ok(out)
}

/// Raeumt die aeltesten Events eines Autors in einem Space auf (hoechstens `BATCH`, nur bis
/// `need` Bytes frei sind), samt allem, was sich auf aufgeraeumte Nachrichten bezieht. Gibt die
/// frei gewordenen Bytes und je Gruppe die abgezogenen Bytes zurueck.
#[allow(clippy::type_complexity)]
fn prune_oldest(
    tx: &Connection,
    space: &SpaceId,
    author: &PublicKey,
    need: u64,
    out: &mut PruneOutcome,
) -> rusqlite::Result<(u64, HashMap<(SpaceId, PublicKey), u64>)> {
    let first: Vec<(EventId, Vec<u8>)> = tx
        .prepare(&format!(
            "SELECT event_id, payload FROM events
              WHERE space_id = ?1 AND author = ?2 AND pruned = 0 AND kind IN {CANDIDATES}
              ORDER BY seq LIMIT ?3"
        ))?
        .query_map(params![space, author, BATCH], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })?
        .collect::<rusqlite::Result<_>>()?;
    let mut freed = 0u64;
    let mut by_group: HashMap<(SpaceId, PublicKey), u64> = HashMap::new();
    let mut roots: Vec<EventId> = Vec::new();
    for (id, payload) in first {
        if freed >= need {
            break;
        }
        let n = prune_one(tx, &id, &payload, out)?;
        freed += n;
        *by_group.entry((*space, *author)).or_default() += n;
        match kind_of(&payload) {
            Some(EventKind::Message { .. } | EventKind::FileShare { .. }) => {
                freed += drop_message(tx, &id)?;
                roots.push(id);
            }
            Some(EventKind::Reaction { target, emoji }) => {
                freed += drop_reaction(tx, &target, author, &emoji)?;
            }
            _ => {}
        }
    }
    for root in &roots {
        // Alles, was sich auf die Nachricht bezieht, auch von anderen.
        let deps: Vec<(EventId, Vec<u8>, SpaceId, PublicKey)> = tx
            .prepare_cached(
                "SELECT event_id, payload, space_id, author FROM events
                  WHERE target = ?1 AND pruned = 0",
            )?
            .query_map([root], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
            .collect::<rusqlite::Result<_>>()?;
        for (id, payload, sp, au) in deps {
            let n = prune_one(tx, &id, &payload, out)?;
            freed += n;
            *by_group.entry((sp, au)).or_default() += n;
        }
    }
    if !by_group.is_empty() {
        out.spaces.insert(*space);
    }
    Ok((freed, by_group))
}

fn kind_of(payload: &[u8]) -> Option<EventKind> {
    postcard::from_bytes(payload).ok()
}

/// Ersetzt den Inhalt eines Events durch den Platzhalter; gibt die gesparten Bytes zurueck.
fn prune_one(
    tx: &Connection,
    id: &EventId,
    payload: &[u8],
    out: &mut PruneOutcome,
) -> rusqlite::Result<u64> {
    let Some(kind) = kind_of(payload) else {
        return Ok(0);
    };
    let stub = postcard::to_stdvec(&kind.pruned()).expect("Platzhalter ist serialisierbar");
    tx.prepare_cached("UPDATE events SET payload = ?2, pruned = 1 WHERE event_id = ?1")?
        .execute(params![id, stub])?;
    out.pruned += 1;
    Ok((payload.len().saturating_sub(stub.len()) as u64) + PROJECTION_ROW)
}

/// Entfernt eine Nachricht samt Anhaengen und Reaktionen aus der Ansicht.
fn drop_message(tx: &Connection, id: &EventId) -> rusqlite::Result<u64> {
    let body: Option<i64> = tx
        .prepare_cached("SELECT length(body) FROM messages WHERE event_id = ?1")?
        .query_row([id], |r| r.get(0))
        .optional()?;
    let mut freed = body.map_or(0, |b| b.max(0) as u64 + PROJECTION_ROW);
    for sql in [
        "DELETE FROM attachments WHERE message_event_id = ?1",
        "DELETE FROM reactions WHERE message_event_id = ?1",
        "DELETE FROM messages WHERE event_id = ?1",
    ] {
        freed += tx.prepare_cached(sql)?.execute([id])? as u64 * PROJECTION_ROW;
    }
    Ok(freed)
}

/// Entfernt eine aufgeraeumte Reaktion aus der Ansicht (ihre Nachricht bleibt).
fn drop_reaction(
    tx: &Connection,
    target: &EventId,
    author: &PublicKey,
    emoji: &str,
) -> rusqlite::Result<u64> {
    let n = tx
        .prepare_cached(
            "DELETE FROM reactions WHERE message_event_id = ?1 AND author = ?2 AND emoji = ?3",
        )?
        .execute(params![target, author, emoji])?;
    Ok(n as u64 * PROJECTION_ROW)
}
