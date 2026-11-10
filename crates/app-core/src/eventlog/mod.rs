//! Event-Log: signierte, pro Autor lueckenlose Events.
//!
//! `events` ist die Wahrheit. Alle Space-Tabellen sind Projektionen, die durch
//! Abspielen der Events in Anzeigereihenfolge (HLC, dann event_id) entstehen.
//! Dadurch sind sie unabhaengig von der Ankunftsreihenfolge identisch.

pub mod chat;
pub mod consent;
pub mod dm;
pub mod gate;
mod project;
pub mod prune;
pub mod sync;
#[cfg(test)]
mod tests;
pub mod unread;

use app_proto::event::{EventBody, EventEnvelope, EventId, EventKind, Hlc, SpaceId};
use app_proto::lock::Locked;
use app_proto::sync::{SpaceHeads, Want};
use app_proto::{PublicKey, Signature};
use ed25519_dalek::VerifyingKey;
use rusqlite::{params, Connection, OptionalExtension};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Mutex;

/// Wie weit ein HLC der Wanduhr voraus sein darf (Uhrenabweichung zwischen zwei Rechnern).
/// Zugleich die Grenze, um die ein Mitglied die Uhren aller anderen vorstellen kann: jedes
/// angenommene Event zieht die eigene HLC nach (Kausalitaet; ohne das kaemen Antworten eines
/// Mitglieds mit nachgehender Uhr vor seiner eigenen Aufnahme in den Space und wuerden
/// abgelehnt). Deshalb klein halten (N-4, frueher 3 h): eine Stunde deckt einen Sommerzeit-
/// oder Zeitzonenfehler ab. Events von weiter vorgehenden Uhren werden angenommen, sobald die
/// eigene Uhr aufgeholt hat.
pub const MAX_FUTURE_MS: u64 = 60 * 60 * 1000;
pub const MAX_BODY_BYTES: usize = 8000;
const MAX_NAME_CHARS: usize = 100;
/// Obergrenze fuer die Nutzlast eines Sync-Batches (Rahmenlimit ist 1 MiB).
const BATCH_BYTES: usize = 400 * 1024;
/// Hoechstens so viele Events je empfangenem Stapel (M-4); ehrliche Stapel (~400 KiB) bleiben
/// weit darunter.
pub const MAX_BATCH_EVENTS: usize = 5000;
/// Durchlaeufe je Stapel: Gruender -> Einladung -> Events des Mitglieds braucht zwei (M-4).
const MAX_PASSES: usize = 4;

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum Reject {
    #[error("Invalid signature")]
    BadSignature,
    #[error("Event is oversized or malformed")]
    Malformed,
    #[error("HLC is too far in the future")]
    FutureHlc,
    #[error("Gap in the author's sequence (expected {expected}, got {got})")]
    Gap { expected: u64, got: u64 },
    #[error("Second, differing event with the same sequence number")]
    Equivocation,
    #[error("Author is not a member")]
    NotMember,
    #[error("Not allowed: {0}")]
    Unauthorized(&'static str),
    #[error("Quota exceeded, try again later")]
    Quota,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Reject(#[from] Reject),
    #[error("SQLite: {0}")]
    Db(#[from] rusqlite::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

/// Hybrid Logical Clock (Wanduhr plus Zaehler), monoton und kausal.
#[derive(Debug, Default)]
pub struct HlcClock(Mutex<Hlc>);

impl HlcClock {
    /// Startet oberhalb des hoechsten gespeicherten HLC.
    pub fn load(conn: &Connection) -> rusqlite::Result<Self> {
        let max: Option<(i64, i64)> = conn
            .query_row(
                "SELECT hlc_wall, hlc_counter FROM events ORDER BY hlc_wall DESC, hlc_counter DESC LIMIT 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let h = max.map_or(Hlc::default(), |(w, c)| Hlc {
            wall_ms: w as u64,
            counter: c as u32,
        });
        Ok(Self(Mutex::new(h)))
    }

    /// Neuer HLC fuer ein lokales Event.
    pub fn send(&self, now_ms: u64) -> Hlc {
        let mut g = self.0.locked();
        *g = if now_ms > g.wall_ms {
            Hlc {
                wall_ms: now_ms,
                counter: 0,
            }
        } else {
            match g.counter.checked_add(1) {
                Some(counter) => Hlc {
                    wall_ms: g.wall_ms,
                    counter,
                },
                // Zaehler voll (z. B. nach einem fremden u32::MAX): auf die Wanduhr uebertragen,
                // statt umzulaufen oder (Debug) mit gehaltenem Mutex zu paniken.
                None => Hlc {
                    wall_ms: g.wall_ms.saturating_add(1),
                    counter: 0,
                },
            }
        };
        *g
    }

    /// Fremden HLC uebernehmen, damit spaetere lokale Events dahinter liegen. Werte weiter als
    /// `MAX_FUTURE_MS` vor der eigenen Wanduhr (die `ingest` ohnehin ablehnt) werden gekappt.
    pub fn observe(&self, remote: Hlc, now_ms: u64) {
        let limit = now_ms.saturating_add(MAX_FUTURE_MS);
        let remote = if remote.wall_ms > limit {
            Hlc {
                wall_ms: limit,
                counter: 0,
            }
        } else {
            remote
        };
        let mut g = self.0.locked();
        if remote > *g {
            *g = remote;
        }
    }
}

/// Space-ID ist an den Gruender und den HLC der Gruendung gebunden (kein Vorab-Belegen).
pub fn derive_space_id(author: &PublicKey, hlc: &Hlc) -> SpaceId {
    let mut h = blake3::Hasher::new();
    h.update(b"app/space/1");
    h.update(author);
    h.update(&hlc.wall_ms.to_be_bytes());
    h.update(&hlc.counter.to_be_bytes());
    h.finalize().as_bytes()[..16].try_into().unwrap()
}

/// Name der Art fuer die Spalte `kind`; ein Platzhalter behaelt den Namen des Originals.
fn kind_name(k: &EventKind) -> &'static str {
    use app_proto::event::tag;
    match k.tag() {
        tag::SPACE_CREATE => "SpaceCreate",
        tag::MEMBER_ADD => "MemberAdd",
        tag::MEMBER_REMOVE => "MemberRemove",
        tag::CHANNEL_CREATE => "ChannelCreate",
        tag::CHANNEL_UPDATE => "ChannelUpdate",
        tag::CHANNEL_DELETE => "ChannelDelete",
        tag::MESSAGE => "Message",
        tag::MESSAGE_EDIT => "MessageEdit",
        tag::MESSAGE_DELETE => "MessageDelete",
        tag::REACTION => "Reaction",
        tag::REACTION_REMOVE => "ReactionRemove",
        tag::FILE_SHARE => "FileShare",
        _ => "Unknown",
    }
}

/// Worauf sich ein Event bezieht (Bearbeitung, Loeschung, Reaktion): wird mit aufgeraeumt.
fn target(k: &EventKind) -> Option<EventId> {
    match k {
        EventKind::MessageEdit { target, .. }
        | EventKind::MessageDelete { target }
        | EventKind::Reaction { target, .. }
        | EventKind::ReactionRemove { target, .. } => Some(*target),
        _ => None,
    }
}

fn subject(k: &EventKind) -> Option<PublicKey> {
    match k {
        EventKind::MemberAdd { user } | EventKind::MemberRemove { user, .. } => Some(*user),
        _ => None,
    }
}

fn ref_seq(k: &EventKind) -> Option<i64> {
    match k {
        EventKind::MemberRemove { last_seq, .. } => Some((*last_seq).min(i64::MAX as u64) as i64),
        _ => None,
    }
}

fn name_ok(s: &str) -> bool {
    !s.trim().is_empty() && s.chars().count() <= MAX_NAME_CHARS
}

fn sizes_ok(k: &EventKind) -> bool {
    match k {
        EventKind::SpaceCreate { name } | EventKind::ChannelCreate { name, .. } => name_ok(name),
        EventKind::ChannelUpdate { name, .. } => name.as_deref().is_none_or(name_ok),
        EventKind::Message { body, .. } | EventKind::MessageEdit { body, .. } => {
            body.len() <= MAX_BODY_BYTES
        }
        EventKind::Reaction { emoji, .. } | EventKind::ReactionRemove { emoji, .. } => {
            !emoji.is_empty() && emoji.len() <= 32
        }
        EventKind::FileShare { name, mime, .. } => {
            !name.is_empty() && name.len() <= 255 && mime.len() <= 100
        }
        // Platzhalter nur fuer Nachrichten-Arten; Mitglieder und Channels bleiben vollstaendig.
        EventKind::Pruned { tag, .. } => app_proto::event::prunable(*tag),
        _ => true,
    }
}

/// Signatur, Groessen und Zeit pruefen (ohne Datenbank).
pub fn verify(env: &EventEnvelope, now_ms: u64) -> std::result::Result<(), Reject> {
    if env.body.seq == 0 || !sizes_ok(&env.body.kind) {
        return Err(Reject::Malformed);
    }
    if env.body.hlc.wall_ms > now_ms.saturating_add(MAX_FUTURE_MS) {
        return Err(Reject::FutureHlc);
    }
    let vk = VerifyingKey::from_bytes(&env.body.author).map_err(|_| Reject::BadSignature)?;
    let sig = ed25519_dalek::Signature::from_bytes(&env.signature.0);
    vk.verify_strict(&env.body.signing_bytes(), &sig)
        .map_err(|_| Reject::BadSignature)
}

pub fn head(conn: &Connection, space: &SpaceId, author: &PublicKey) -> rusqlite::Result<u64> {
    Ok(conn
        .query_row(
            "SELECT max_seq FROM author_heads WHERE space_id = ?1 AND author = ?2",
            params![space, author],
            |r| r.get::<_, i64>(0),
        )
        .optional()?
        .unwrap_or(0) as u64)
}

/// Mitgliedschaft des Autors zum Zeitpunkt (HLC) seines Events, allein aus dem Log bestimmt.
#[derive(Debug, PartialEq, Eq)]
enum Membership {
    /// Gruender oder vom Gruender hinzugefuegt (und seitdem nicht entfernt).
    Member,
    /// Zuletzt entfernt (vom Gruender oder selbst ausgetreten).
    Removed,
    /// Keine gueltige Einladung bekannt (kann noch kommen).
    Unknown,
}

/// Prueft, ob `env` von einem Mitglied stammt. Zaehlt nur Einladungen und Entfernungen, die die
/// Projektion auch gelten laesst: hinzufuegen und entfernen darf nur der Gruender (einziger
/// Admin), austreten jeder selbst. So kann ein normales Mitglied keine Wegwerf-Schluessel
/// anmelden, und Events eines Entfernten, die nach dem Entfernen entstanden sind, werden weder
/// gespeichert noch weitergereicht.
///
/// "Nach dem Entfernen" bestimmt die `seq`, nicht der (vom Autor frei gewaehlte) HLC: liegt die
/// `seq` ueber dem `last_seq` einer Entfernung, gilt das Event nur, wenn der Gruender den Autor
/// danach wieder eingeladen hat (HLC zwischen Entfernung und Event). Sonst koennte ein
/// Entfernter rueckdatierte Events in die Zeit seiner Mitgliedschaft schreiben (M-1).
fn membership(conn: &Connection, env: &EventEnvelope) -> rusqlite::Result<Membership> {
    let b = &env.body;
    let creator: Option<PublicKey> = conn
        .prepare_cached(
            "SELECT author FROM events WHERE space_id = ?1 AND kind = 'SpaceCreate' LIMIT 1",
        )?
        .query_row([b.space_id], |r| r.get(0))
        .optional()?;
    let Some(creator) = creator else {
        return Ok(Membership::Unknown);
    };
    if creator == b.author {
        return Ok(Membership::Member);
    }
    let (wall, counter, id, seq) = (
        b.hlc.wall_ms as i64,
        b.hlc.counter as i64,
        env.id(),
        b.seq.min(i64::MAX as u64) as i64,
    );
    // Juengste Entfernung, deren Obergrenze dieses Event ueberschreitet.
    let cut: Option<(i64, i64, [u8; 32])> = conn
        .prepare_cached(
            "SELECT hlc_wall, hlc_counter, event_id FROM events
             WHERE space_id = ?1 AND kind = 'MemberRemove' AND subject = ?2
               AND (author = ?3 OR author = ?2) AND ref_seq < ?4
             ORDER BY hlc_wall DESC, hlc_counter DESC, event_id DESC LIMIT 1",
        )?
        .query_row(params![b.space_id, b.author, creator, seq], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })
        .optional()?;
    if let Some((cw, cc, cid)) = cut {
        let readded = conn
            .prepare_cached(
                "SELECT 1 FROM events
                 WHERE space_id = ?1 AND kind = 'MemberAdd' AND subject = ?2 AND author = ?3
                   AND (hlc_wall, hlc_counter, event_id) > (?4, ?5, ?6)
                   AND (hlc_wall, hlc_counter, event_id) < (?7, ?8, ?9)
                 LIMIT 1",
            )?
            .exists(params![
                b.space_id, b.author, creator, cw, cc, cid, wall, counter, id
            ])?;
        if !readded {
            return Ok(Membership::Removed);
        }
    }
    // Letzte gueltige Einladung/Entfernung vor dem Event (Anzeigereihenfolge wie die Projektion).
    // Entfernungen, die dieses Event noch abdecken (`seq <= last_seq`), zaehlen hier nicht: es
    // entstand vor der Entfernung und wird gespeichert; die Projektion laesst es wirkungslos,
    // wenn sein HLC hinter der Entfernung liegt. So speichern alle Knoten dasselbe, egal ob die
    // Entfernung frueher oder spaeter ankam.
    let last: Option<String> = conn
        .prepare_cached(
            "SELECT kind FROM events
             WHERE space_id = ?1 AND kind IN ('MemberAdd', 'MemberRemove') AND subject = ?2
               AND (author = ?3 OR (kind = 'MemberRemove' AND author = ?2))
               AND (kind = 'MemberAdd' OR ref_seq < ?7)
               AND (hlc_wall, hlc_counter, event_id) < (?4, ?5, ?6)
             ORDER BY hlc_wall DESC, hlc_counter DESC, event_id DESC LIMIT 1",
        )?
        .query_row(
            params![b.space_id, b.author, creator, wall, counter, id, seq],
            |r| r.get(0),
        )
        .optional()?;
    Ok(match last.as_deref() {
        Some("MemberAdd") => Membership::Member,
        Some(_) => Membership::Removed,
        None => Membership::Unknown,
    })
}

fn space_known(conn: &Connection, space: &SpaceId) -> rusqlite::Result<bool> {
    conn.prepare_cached("SELECT 1 FROM events WHERE space_id = ?1 LIMIT 1")?
        .exists([space])
}

/// Ergebnis des Speicherns eines einzelnen Events.
#[derive(Debug, PartialEq, Eq)]
enum Stored {
    Duplicate,
    /// Neu gespeichert; `purged`: dabei wurden spaetere Events eines Entfernten geloescht
    /// (Neuaufbau noetig, M-1).
    New {
        purged: bool,
    },
}

/// Prueft Folge und Mitgliedschaft und schreibt das Event (ohne Projektion).
fn store(conn: &Connection, env: &EventEnvelope, now_s: i64) -> Result<Stored> {
    let b = &env.body;
    let id = env.id();
    if conn
        .prepare_cached("SELECT 1 FROM events WHERE event_id = ?1")?
        .exists([id])?
    {
        return Ok(Stored::Duplicate);
    }
    let h = head(conn, &b.space_id, &b.author)?;
    if b.seq <= h {
        record_equivocation(conn, env, &id, now_s)?;
        return Err(Reject::Equivocation.into());
    }
    if b.seq != h + 1 {
        return Err(Reject::Gap {
            expected: h + 1,
            got: b.seq,
        }
        .into());
    }
    // HLC je Autor streng steigend (M-1): sonst liesse sich ein Event beliebig weit in die
    // Vergangenheit legen, und absteigende Folgen erzwaengen staendige Neuaufbauten (M-4).
    if h > 0 {
        let prev: Option<(i64, i64)> = conn
            .prepare_cached(
                "SELECT hlc_wall, hlc_counter FROM events
                 WHERE space_id = ?1 AND author = ?2 AND seq = ?3",
            )?
            .query_row(params![b.space_id, b.author, h as i64], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .optional()?;
        if let Some((w, c)) = prev {
            let prev = Hlc {
                wall_ms: w as u64,
                counter: c as u32,
            };
            if b.hlc <= prev {
                return Err(
                    Reject::Unauthorized("HLC is not after the author's previous event").into(),
                );
            }
        }
    }
    if let EventKind::MemberRemove { user, last_seq } = &b.kind {
        if *user == b.author && *last_seq != b.seq {
            return Err(Reject::Malformed.into());
        }
    }
    if let EventKind::SpaceCreate { .. } = b.kind {
        if b.space_id != derive_space_id(&b.author, &b.hlc) {
            return Err(Reject::Unauthorized("Space ID does not match the founder").into());
        }
        if space_known(conn, &b.space_id)? {
            return Err(Reject::Unauthorized("Space already exists").into());
        }
    } else {
        match membership(conn, env)? {
            Membership::Member => {}
            Membership::Unknown => return Err(Reject::NotMember.into()),
            Membership::Removed => {
                return Err(Reject::Unauthorized("Author was removed from the space").into())
            }
        }
    }
    let payload = postcard::to_stdvec(&b.kind).map_err(|_| Reject::Malformed)?;
    conn.prepare_cached(
        "INSERT INTO events (event_id, space_id, author, seq, hlc_wall, hlc_counter, kind,
                             payload, signature, received_at, subject, ref_seq, target, pruned)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
    )?
    .execute(params![
        id,
        b.space_id,
        b.author,
        b.seq as i64,
        b.hlc.wall_ms as i64,
        b.hlc.counter as i64,
        kind_name(&b.kind),
        payload,
        env.signature.0.as_slice(),
        now_s,
        subject(&b.kind),
        ref_seq(&b.kind),
        target(&b.kind),
        b.kind.is_pruned(),
    ])?;
    conn.prepare_cached(
        "INSERT INTO author_heads (space_id, author, max_seq) VALUES (?1, ?2, ?3)
         ON CONFLICT (space_id, author) DO UPDATE SET max_seq = ?3",
    )?
    .execute(params![b.space_id, b.author, b.seq as i64])?;
    let purged = match &b.kind {
        EventKind::MemberRemove { user, last_seq } if *user != b.author => {
            purge_after_remove(conn, &b.space_id, user, *last_seq)?
        }
        _ => false,
    };
    Ok(Stored::New { purged })
}

/// Ein gueltig signiertes Event mit einer seq, die bei uns schon ein anderes Event hat: beide
/// als Beweis behalten (N-14). Das zuerst gesehene bleibt im Log, damit die Folge eindeutig ist.
fn record_equivocation(
    conn: &Connection,
    env: &EventEnvelope,
    id: &EventId,
    now_s: i64,
) -> rusqlite::Result<()> {
    let b = &env.body;
    let kept: Option<EventId> = conn
        .prepare_cached(
            "SELECT event_id FROM events WHERE space_id = ?1 AND author = ?2 AND seq = ?3",
        )?
        .query_row(params![b.space_id, b.author, b.seq as i64], |r| r.get(0))
        .optional()?;
    let Some(kept) = kept.filter(|k| k != id) else {
        return Ok(());
    };
    let Ok(other) = postcard::to_stdvec(env) else {
        return Ok(());
    };
    conn.prepare_cached(
        "INSERT OR IGNORE INTO equivocations (space_id, author, seq, kept_event, other, detected_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
    )?
    .execute(params![b.space_id, b.author, b.seq as i64, kept, other, now_s])?;
    Ok(())
}

/// Autoren, die in `space` nachweislich zwei verschiedene Events mit derselben seq signiert
/// haben (N-14). Ihre Nachrichten koennen bei verschiedenen Mitgliedern verschieden aussehen.
pub fn equivocators(conn: &Connection, space: &SpaceId) -> rusqlite::Result<Vec<PublicKey>> {
    conn.prepare_cached(
        "SELECT DISTINCT author FROM equivocations WHERE space_id = ?1 ORDER BY author",
    )?
    .query_map([space], |r| r.get(0))?
    .collect()
}

/// Nach einer Entfernung: Events des Entfernten oberhalb von `last_seq`, die hier schon lagen
/// (bevor die Entfernung ankam) und nun nicht mehr gelten, ab dem ersten ungueltigen loeschen.
/// Die Folge bleibt luecklos; gibt `true` zurueck, wenn etwas geloescht wurde (Neuaufbau).
fn purge_after_remove(
    conn: &Connection,
    space: &SpaceId,
    user: &PublicKey,
    last_seq: u64,
) -> rusqlite::Result<bool> {
    let later: Vec<EventEnvelope> = conn
        .prepare_cached(&format!(
            "{SELECT_ENV} WHERE space_id = ?1 AND author = ?2 AND seq > ?3 ORDER BY seq"
        ))?
        .query_map(
            params![space, user, last_seq.min(i64::MAX as u64) as i64],
            row_to_envelope,
        )?
        .collect::<rusqlite::Result<_>>()?;
    for env in later {
        if membership(conn, &env)? == Membership::Member {
            continue;
        }
        let from = env.body.seq as i64;
        conn.execute(
            "DELETE FROM events WHERE space_id = ?1 AND author = ?2 AND seq >= ?3",
            params![space, user, from],
        )?;
        conn.execute(
            "UPDATE author_heads SET max_seq = ?3 WHERE space_id = ?1 AND author = ?2",
            params![space, user, from - 1],
        )?;
        return Ok(true);
    }
    Ok(false)
}

fn row_to_envelope(r: &rusqlite::Row<'_>) -> rusqlite::Result<EventEnvelope> {
    let payload: Vec<u8> = r.get(5)?;
    let sig: Vec<u8> = r.get(6)?;
    let bad = |m: &str| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Blob, m.into())
    };
    Ok(EventEnvelope {
        body: EventBody {
            space_id: r.get(0)?,
            author: r.get(1)?,
            seq: r.get::<_, i64>(2)? as u64,
            hlc: Hlc {
                wall_ms: r.get::<_, i64>(3)? as u64,
                counter: r.get::<_, i64>(4)? as u32,
            },
            kind: postcard::from_bytes(&payload).map_err(|_| bad("payload"))?,
        },
        signature: Signature(sig.try_into().map_err(|_| bad("signature"))?),
    })
}

const SELECT_ENV: &str =
    "SELECT space_id, author, seq, hlc_wall, hlc_counter, payload, signature FROM events";

/// Baut die Projektion eines Spaces aus allen Events neu auf.
pub fn rebuild_space(conn: &Connection, space: &SpaceId, now_s: i64) -> rusqlite::Result<()> {
    project::reset(conn, space)?;
    let mut st = conn.prepare(&format!(
        "{SELECT_ENV} WHERE space_id = ?1 ORDER BY hlc_wall, hlc_counter, event_id"
    ))?;
    let mut rows = st.query([space])?;
    while let Some(r) = rows.next()? {
        let env = row_to_envelope(r)?;
        project::apply(conn, &env.body, &env.id(), now_s)?;
    }
    Ok(())
}

/// Baut alle Spaces neu auf (Wiederherstellung, Tests).
pub fn rebuild_all(conn: &Connection, now_s: i64) -> rusqlite::Result<()> {
    let spaces: Vec<SpaceId> = conn
        .prepare("SELECT DISTINCT space_id FROM events")?
        .query_map([], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    for s in spaces {
        rebuild_space(conn, &s, now_s)?;
    }
    Ok(())
}

/// Wie fremde Events angenommen werden.
#[derive(Debug, Clone, Copy)]
pub struct IngestOpts {
    /// Eigener Nutzer: nur Spaces annehmen, in denen ich Mitglied bin oder eine Einladung habe,
    /// mit Quoten (M-3, `gate`). `None` = alles (Tests, Werkzeuge).
    pub me: Option<PublicKey>,
    /// Neuaufbau nicht sofort, sondern in `BatchOutcome::dirty` melden; der Aufrufer entprellt
    /// ihn (M-4). Bis dahin wird inkrementell angewendet.
    pub defer_rebuild: bool,
    /// Obergrenze fuer noch nicht angenommene Spaces.
    pub pending_cap: i64,
    /// Signaturen hat der Aufrufer schon mit [`verify`] geprueft (ausserhalb des DB-Threads).
    pub verified: bool,
}

impl Default for IngestOpts {
    fn default() -> Self {
        Self {
            me: None,
            defer_rebuild: false,
            pending_cap: gate::PENDING_SPACE_EVENTS,
            verified: false,
        }
    }
}

/// Ergebnis eines Stapels empfangener Events.
#[derive(Debug, Default)]
pub struct BatchOutcome {
    /// Spaces, deren Neuaufbau aufgeschoben wurde (`IngestOpts::defer_rebuild`).
    pub dirty: HashSet<SpaceId>,
    /// Neu gespeicherte Events (in Speicherreihenfolge).
    pub accepted: Vec<EventEnvelope>,
    pub duplicates: usize,
    /// Verworfene Events mit Grund (Luecken/Nicht-Mitglieder sind nur "noch nicht").
    pub rejected: Vec<Reject>,
    /// Es gab Luecken: der Absender sollte ein Sync anstossen.
    pub gaps: bool,
    pub spaces: HashSet<SpaceId>,
}

/// Nimmt fremde Events an: Pruefen, speichern, Projektion nachziehen (ohne Gate, sofortiger
/// Neuaufbau; fuer Tests und Werkzeuge).
pub fn ingest_batch(
    conn: &mut Connection,
    envs: Vec<EventEnvelope>,
    now_ms: u64,
) -> rusqlite::Result<BatchOutcome> {
    ingest_batch_with(conn, envs, now_ms, IngestOpts::default())
}

/// Schluessel der Anzeigereihenfolge des juengsten Events eines Spaces.
type OrderKey = (i64, i64, [u8; 32]);

fn newest_in(conn: &Connection, space: &SpaceId) -> rusqlite::Result<Option<OrderKey>> {
    conn.prepare_cached(
        "SELECT hlc_wall, hlc_counter, event_id FROM events WHERE space_id = ?1
         ORDER BY hlc_wall DESC, hlc_counter DESC, event_id DESC LIMIT 1",
    )?
    .query_row([space], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
    .optional()
}

/// Nimmt fremde Events an: Pruefen, speichern, Projektion nachziehen.
///
/// Verarbeitet wird je (Space, Autor) in `seq`-Reihenfolge, nicht nach HLC (M-4): jede Kette ist
/// linear, und Abhaengigkeiten zwischen Autoren (Einladung vor den Events des Eingeladenen)
/// brauchen nur wenige Durchlaeufe. Der Stapel ist auf `MAX_BATCH_EVENTS` begrenzt.
pub fn ingest_batch_with(
    conn: &mut Connection,
    mut envs: Vec<EventEnvelope>,
    now_ms: u64,
    opts: IngestOpts,
) -> rusqlite::Result<BatchOutcome> {
    let now_s = (now_ms / 1000) as i64;
    let mut out = BatchOutcome::default();
    if envs.len() > MAX_BATCH_EVENTS {
        envs.truncate(MAX_BATCH_EVENTS);
        out.gaps = true;
    }
    let mut pending: Vec<EventEnvelope> = Vec::new();
    for e in envs {
        if opts.verified {
            pending.push(e);
            continue;
        }
        match verify(&e, now_ms) {
            Ok(()) => pending.push(e),
            Err(r) => out.rejected.push(r),
        }
    }
    pending.sort_by(|a, b| {
        (a.body.space_id, a.body.author, a.body.seq).cmp(&(
            b.body.space_id,
            b.body.author,
            b.body.seq,
        ))
    });
    let tx = conn.transaction()?;
    let mut dirty: HashSet<SpaceId> = HashSet::new();
    let mut queued: BTreeMap<SpaceId, Vec<EventEnvelope>> = BTreeMap::new();
    // Juengstes Event je Space vor dem Stapel: liegt ein neues davor, muss neu aufgebaut werden.
    let mut newest: HashMap<SpaceId, Option<OrderKey>> = HashMap::new();
    let mut gate = gate::Gate::default();
    // Mehrere Durchlaeufe: ein Event kann erst nach einem anderen im selben Stapel gueltig sein.
    for pass in 1..=MAX_PASSES {
        let mut progress = false;
        let mut again = Vec::new();
        for e in pending {
            if let Some(me) = &opts.me {
                // Bekanntes zaehlt als Duplikat, nicht gegen Quoten.
                if tx
                    .prepare_cached("SELECT 1 FROM events WHERE event_id = ?1")?
                    .exists([e.id()])?
                {
                    out.duplicates += 1;
                    continue;
                }
                match gate.admit(&tx, me, opts.pending_cap, &e, now_s)? {
                    Ok(()) => {}
                    Err(r @ Reject::NotMember) => {
                        again.push((e, r));
                        continue;
                    }
                    Err(r) => {
                        out.rejected.push(r);
                        continue;
                    }
                }
            }
            let space = e.body.space_id;
            if let std::collections::hash_map::Entry::Vacant(v) = newest.entry(space) {
                v.insert(newest_in(&tx, &space)?);
            }
            match store(&tx, &e, now_s) {
                Ok(Stored::Duplicate) => out.duplicates += 1,
                Ok(Stored::New { purged }) => {
                    progress = true;
                    gate.stored(&e);
                    out.spaces.insert(space);
                    let key = (e.body.hlc.wall_ms as i64, e.body.hlc.counter as i64, e.id());
                    if purged || newest[&space].is_some_and(|n| key < n) {
                        dirty.insert(space);
                    }
                    queued.entry(space).or_default().push(e.clone());
                    out.accepted.push(e);
                }
                Err(Error::Reject(r @ (Reject::Gap { .. } | Reject::NotMember))) => {
                    again.push((e, r));
                }
                Err(Error::Reject(r @ Reject::Equivocation)) => {
                    // Warnung in der Oberflaeche auffrischen (N-14).
                    out.spaces.insert(space);
                    out.rejected.push(r);
                }
                Err(Error::Reject(r)) => out.rejected.push(r),
                Err(Error::Db(e)) => return Err(e),
            }
        }
        if again.is_empty() {
            break;
        }
        if !progress || pass == MAX_PASSES {
            out.gaps = true;
            out.rejected.extend(again.into_iter().map(|(_, r)| r));
            break;
        }
        pending = again.into_iter().map(|(e, _)| e).collect();
    }
    for (space, mut list) in queued {
        let rebuild = dirty.contains(&space) || list.len() > MAX_BATCH_EVENTS;
        if rebuild && !opts.defer_rebuild {
            rebuild_space(&tx, &space, now_s)?;
            continue;
        }
        if rebuild {
            out.dirty.insert(space);
        }
        list.sort_by(|a, b| {
            a.body
                .hlc
                .cmp(&b.body.hlc)
                .then_with(|| a.id().cmp(&b.id()))
        });
        for e in &list {
            project::apply(&tx, &e.body, &e.id(), now_s)?;
        }
    }
    tx.commit()?;
    Ok(out)
}

/// Erzeugt, signiert und speichert ein lokales Event; scheitert, wenn es nicht erlaubt waere.
pub fn create_event(
    conn: &mut Connection,
    clock: &HlcClock,
    author: PublicKey,
    sign: &dyn Fn(&[u8]) -> Signature,
    space_id: Option<SpaceId>,
    kind: EventKind,
    now_ms: u64,
) -> Result<EventEnvelope> {
    // Platzhalter entstehen nur beim Aufraeumen, nie als neues Event.
    if !sizes_ok(&kind) || kind.is_pruned() {
        return Err(Reject::Malformed.into());
    }
    let hlc = clock.send(now_ms);
    let space_id = match (&kind, space_id) {
        (EventKind::SpaceCreate { .. }, _) => derive_space_id(&author, &hlc),
        (_, Some(s)) => s,
        (_, None) => return Err(Reject::Malformed.into()),
    };
    let tx = conn.transaction()?;
    let seq = head(&tx, &space_id, &author)? + 1;
    // Obergrenze der Entfernung setzt immer der Ersteller: beim Austritt dieses Event, sonst der
    // hoechste hier bekannte Stand des Entfernten (spaeter eintreffende Events zaehlen nicht).
    let kind = match kind {
        EventKind::MemberRemove { user, .. } => EventKind::MemberRemove {
            user,
            last_seq: if user == author {
                seq
            } else {
                head(&tx, &space_id, &user)?
            },
        },
        k => k,
    };
    let body = EventBody {
        space_id,
        author,
        seq,
        hlc,
        kind,
    };
    let env = EventEnvelope {
        signature: sign(&body.signing_bytes()),
        body,
    };
    let now_s = (now_ms / 1000) as i64;
    match store(&tx, &env, now_s)? {
        Stored::New { .. } => {}
        Stored::Duplicate => return Err(Reject::Malformed.into()),
    }
    // Lokal liegt das Event immer am Ende (frischer HLC): inkrementell anwenden.
    if !project::apply(&tx, &env.body, &env.id(), now_s)? {
        tx.rollback()?;
        return Err(Reject::Unauthorized("Action not allowed").into());
    }
    tx.commit()?;
    Ok(env)
}

// ---------- Sync-Hilfen (rein datenbankbasiert) ----------

/// Merkt sich, bis zu welcher Folgenummer `peer` die Events der Autoren schon hat
/// (aus seinen Heads beim Abgleich). Der Wert wird nie kleiner.
/// Gibt die Spaces zurueck, in denen sich ein Zustellstand tatsaechlich erhoeht hat.
pub fn record_acks(
    conn: &Connection,
    peer: &PublicKey,
    heads: &[SpaceHeads],
) -> rusqlite::Result<Vec<SpaceId>> {
    // Nur Spaces, in denen `peer` Mitglied ist, und nur bis zu dem, was es bei uns gibt: sonst
    // koennte ein Freund "zugestellt" vortaeuschen oder die Tabelle mit Erfundenem fuellen.
    // Das WHERE am Update sorgt dafuer, dass nur echte Erhoehungen als Aenderung zaehlen.
    let mut st = conn.prepare_cached(
        "INSERT INTO peer_acks (space_id, peer_user, author, seq)
         SELECT h.space_id, ?2, h.author, min(?4, h.max_seq) FROM author_heads h
           JOIN members m ON m.space_id = h.space_id AND m.user_pubkey = ?2
          WHERE h.space_id = ?1 AND h.author = ?3
         ON CONFLICT (space_id, peer_user, author) DO UPDATE SET seq = excluded.seq
          WHERE excluded.seq > peer_acks.seq",
    )?;
    let mut changed = Vec::new();
    for h in heads {
        let mut any = false;
        for (author, seq, _) in &h.authors {
            any |= st.execute(params![h.space_id, peer, author, *seq as i64])? > 0;
        }
        if any {
            changed.push(h.space_id);
        }
    }
    Ok(changed)
}

/// Heads aller Spaces, in denen `peer` Mitglied ist.
pub fn heads_for(conn: &Connection, peer: &PublicKey) -> rusqlite::Result<Vec<SpaceHeads>> {
    // Auch Spaces, aus denen `peer` selbst ausgetreten ist: so erfaehrt er, dass sein Austritt
    // angekommen ist, und kann die Daten loeschen (M-3). Events bekommt er dort keine mehr.
    let mut st = conn.prepare(
        "SELECT h.space_id, h.author, h.max_seq, e.event_id FROM author_heads h
           JOIN events e ON e.space_id = h.space_id AND e.author = h.author AND e.seq = h.max_seq
          WHERE EXISTS (SELECT 1 FROM members m
                         WHERE m.space_id = h.space_id AND m.user_pubkey = ?1)
             OR EXISTS (SELECT 1 FROM events e
                         WHERE e.space_id = h.space_id AND e.kind = 'MemberRemove'
                           AND e.subject = ?1 AND e.author = ?1)
         ORDER BY h.space_id, h.author",
    )?;
    let mut map: BTreeMap<SpaceId, Vec<(PublicKey, u64, EventId)>> = BTreeMap::new();
    let mut rows = st.query([peer])?;
    while let Some(r) = rows.next()? {
        map.entry(r.get(0)?)
            .or_default()
            .push((r.get(1)?, r.get::<_, i64>(2)? as u64, r.get(3)?));
    }
    let mut member =
        conn.prepare_cached("SELECT 1 FROM members WHERE space_id = ?1 AND user_pubkey = ?2")?;
    let mut out = Vec::with_capacity(map.len());
    for (space_id, authors) in map {
        let invite = if member.exists(params![space_id, peer])? {
            gate::invite_for(conn, &space_id, peer)?
        } else {
            None
        };
        out.push(SpaceHeads {
            space_id,
            authors,
            invite,
        });
    }
    Ok(out)
}

/// Was uns laut den Heads des Gegenuebers fehlt. Hat das Gegenueber unter einer seq, die wir
/// schon kennen, ein anderes Event, wird genau dieses angefordert: beim Speichern faellt die
/// Equivocation dann auf und wird als Beweis festgehalten (N-14).
pub fn wants_from(conn: &Connection, remote: &[SpaceHeads]) -> rusqlite::Result<Vec<Want>> {
    let mut out = Vec::new();
    // Unser Event unter der seq, ausser die Abweichung ist schon belegt (nicht jedes Mal neu
    // anfordern).
    let mut at = conn.prepare_cached(
        "SELECT event_id FROM events e WHERE space_id = ?1 AND author = ?2 AND seq = ?3
            AND NOT EXISTS (SELECT 1 FROM equivocations q
                             WHERE q.space_id = ?1 AND q.author = ?2 AND q.seq = ?3)",
    )?;
    for sh in remote {
        for (author, max, tip) in &sh.authors {
            let mine = head(conn, &sh.space_id, author)?;
            let (from_seq, to_seq) = if *max > mine {
                (mine + 1, *max)
            } else if *max == 0 {
                continue;
            } else {
                let ours: Option<EventId> = at
                    .query_row(params![sh.space_id, author, *max as i64], |r| r.get(0))
                    .optional()?;
                if ours.is_none_or(|o| o == *tip) {
                    continue;
                }
                (*max, *max)
            };
            out.push(Want {
                space_id: sh.space_id,
                author: *author,
                from_seq,
                to_seq,
            });
        }
    }
    Ok(out)
}

/// Liest einen Teil von `want` fuer `peer` (nur aus Spaces, in denen er Mitglied ist).
/// Gibt die Events und die naechste noch offene Folgenummer zurueck (`None` = fertig).
pub fn read_want(
    conn: &Connection,
    peer: &PublicKey,
    want: &Want,
) -> rusqlite::Result<(Vec<EventEnvelope>, Option<u64>)> {
    let allowed = conn
        .prepare_cached("SELECT 1 FROM members WHERE space_id = ?1 AND user_pubkey = ?2")?
        .exists(params![want.space_id, peer])?;
    if !allowed || want.from_seq > want.to_seq {
        return Ok((Vec::new(), None));
    }
    let mut st = conn.prepare_cached(&format!(
        "{SELECT_ENV} WHERE space_id = ?1 AND author = ?2 AND seq BETWEEN ?3 AND ?4 ORDER BY seq"
    ))?;
    let mut rows = st.query(params![
        want.space_id,
        want.author,
        want.from_seq as i64,
        want.to_seq as i64
    ])?;
    let mut out = Vec::new();
    let mut bytes = 0usize;
    while let Some(r) = rows.next()? {
        let e = row_to_envelope(r)?;
        bytes += 120 + postcard::to_stdvec(&e.body.kind).map_or(0, |v| v.len());
        let seq = e.body.seq;
        out.push(e);
        if bytes >= BATCH_BYTES {
            let next = seq + 1;
            return Ok((out, (next <= want.to_seq).then_some(next)));
        }
    }
    Ok((out, None))
}

// ---------- Lesezugriff fuer UI und Tests ----------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpaceInfo {
    pub space_id: SpaceId,
    pub name: String,
    /// Gruender (einziger Admin).
    pub creator: Option<PublicKey>,
}

/// Alle Mitgliedschaften (Space, Nutzer) laut Projektion.
pub fn all_memberships(conn: &Connection) -> rusqlite::Result<HashSet<(SpaceId, PublicKey)>> {
    let mut st = conn.prepare_cached("SELECT space_id, user_pubkey FROM members")?;
    let rows = st.query_map([], |r| {
        Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, Vec<u8>>(1)?))
    })?;
    let mut set = HashSet::new();
    for row in rows {
        let (s, u) = row?;
        if let (Ok(s), Ok(u)) = (s.try_into(), u.try_into()) {
            set.insert((s, u));
        }
    }
    Ok(set)
}

/// Spaces, in denen `me` Mitglied ist. Events fremder Spaces koennen gespeichert sein (ein Freund
/// schickt sie, oder ich wurde entfernt); die erscheinen nicht.
pub fn list_spaces(conn: &Connection, me: &PublicKey) -> rusqlite::Result<Vec<SpaceInfo>> {
    conn.prepare(
        "SELECT s.space_id, s.name,
                (SELECT a.user_pubkey FROM members a
                  WHERE a.space_id = s.space_id AND a.role = 'admin' LIMIT 1)
           FROM spaces s
           JOIN members mine ON mine.space_id = s.space_id AND mine.user_pubkey = ?1
         ORDER BY s.joined_at, s.space_id",
    )?
    .query_map([me], |r| {
        Ok(SpaceInfo {
            space_id: r.get(0)?,
            name: r.get(1)?,
            creator: r.get(2)?,
        })
    })?
    .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelRow {
    pub id: [u8; 16],
    pub kind: app_proto::event::ChannelKind,
    pub name: String,
    pub position: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberRow {
    pub user: PublicKey,
    pub admin: bool,
}

/// Channels (nach Position) und Mitglieder eines Spaces fuer die Oberflaeche.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SpaceView {
    pub channels: Vec<ChannelRow>,
    pub members: Vec<MemberRow>,
    /// Autoren mit widerspruechlichem Verlauf (N-14), siehe [`equivocators`].
    pub equivocators: Vec<PublicKey>,
}

pub fn space_view(conn: &Connection, space: &SpaceId) -> rusqlite::Result<SpaceView> {
    let channels = conn
        .prepare(
            "SELECT channel_id, kind, name, position FROM channels WHERE space_id = ?1
             ORDER BY position, channel_id",
        )?
        .query_map([space], |r| {
            let kind: String = r.get(1)?;
            Ok(ChannelRow {
                id: r.get(0)?,
                kind: if kind == "voice" {
                    app_proto::event::ChannelKind::Voice
                } else {
                    app_proto::event::ChannelKind::Text
                },
                name: r.get(2)?,
                position: r.get(3)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    let members = conn
        .prepare(
            "SELECT user_pubkey, role FROM members WHERE space_id = ?1
             ORDER BY role = 'admin' DESC, user_pubkey",
        )?
        .query_map([space], |r| {
            Ok(MemberRow {
                user: r.get(0)?,
                admin: r.get::<_, String>(1)? == "admin",
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    Ok(SpaceView {
        channels,
        members,
        equivocators: equivocators(conn, space)?,
    })
}

/// Deterministischer Auszug der Projektion (ohne lokale Felder wie `joined_at`).
pub fn projection_dump(conn: &Connection) -> rusqlite::Result<Vec<String>> {
    let queries = [
        "SELECT 'space', hex(space_id), name FROM spaces ORDER BY 2",
        "SELECT 'member', hex(space_id), hex(user_pubkey) || role || hex(added_by) FROM members ORDER BY 2, 3",
        "SELECT 'channel', hex(space_id), hex(channel_id) || kind || name || position FROM channels ORDER BY 2, 3",
        "SELECT 'message', hex(space_id), hex(event_id) || hex(channel_id) || hex(author) || hlc_wall || ':' || hlc_counter || body || ifnull(hex(reply_to),'-') || ifnull(edited_hlc,'-') || deleted FROM messages ORDER BY 2, 3",
        "SELECT 'reaction', hex(message_event_id), hex(author) || emoji FROM reactions ORDER BY 2, 3",
        "SELECT 'attachment', hex(message_event_id), hex(blob_hash) || name || size || mime FROM attachments ORDER BY 2, 3",
    ];
    let mut out = Vec::new();
    for q in queries {
        let mut st = conn.prepare(q)?;
        let rows = st.query_map([], |r| {
            Ok(format!(
                "{}|{}|{}",
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?
            ))
        })?;
        for r in rows {
            out.push(r?);
        }
    }
    Ok(out)
}

/// Alle Events eines Spaces in Anzeigereihenfolge (Tests, Export).
pub fn all_events(conn: &Connection, space: &SpaceId) -> rusqlite::Result<Vec<EventEnvelope>> {
    conn.prepare(&format!(
        "{SELECT_ENV} WHERE space_id = ?1 ORDER BY hlc_wall, hlc_counter, event_id"
    ))?
    .query_map([space], row_to_envelope)?
    .collect()
}

/// Zaehlt gespeicherte Events (Tests, Messung).
pub fn event_count(conn: &Connection) -> rusqlite::Result<i64> {
    conn.query_row("SELECT count(*) FROM events", [], |r| r.get(0))
}
