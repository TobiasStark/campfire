//! Schema-Migrationen. Nur anhaengen, nie aendern.

use rusqlite_migration::{Migrations, M};

/// Migration 1: lokale Tabellen. Migration 2: Event-Log und Projektionen. Migration 3: Freundschaftsanfragen (Notiz, Outbox).
pub fn migrations() -> Migrations<'static> {
    Migrations::new(vec![
        M::up(
            "CREATE TABLE identity (
                id INTEGER PRIMARY KEY CHECK (id = 1),
                user_secret BLOB NOT NULL,
                device_secret BLOB NOT NULL,
                device_cert BLOB NOT NULL,
                display_name TEXT NOT NULL,
                avatar_hash BLOB
            );
            CREATE TABLE friends (
                user_pubkey BLOB PRIMARY KEY,
                name TEXT NOT NULL,
                status TEXT NOT NULL CHECK (status IN ('pending_in','pending_out','accepted','blocked')),
                devices TEXT NOT NULL DEFAULT '[]',
                last_seen INTEGER,
                addr_hints TEXT
            );
            CREATE TABLE settings (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );
            CREATE TABLE read_state (
                channel_id BLOB PRIMARY KEY,
                last_read_hlc INTEGER NOT NULL
            );
            CREATE TABLE blobs_meta (
                hash BLOB PRIMARY KEY,
                size INTEGER NOT NULL,
                pinned INTEGER NOT NULL DEFAULT 0,
                last_access INTEGER NOT NULL,
                local INTEGER NOT NULL DEFAULT 0
            );",
        ),
        M::up(
            "CREATE TABLE spaces (
                space_id BLOB PRIMARY KEY,
                name TEXT NOT NULL,
                joined_at INTEGER NOT NULL,
                muted INTEGER NOT NULL DEFAULT 0
            );
            CREATE TABLE events (
                event_id BLOB PRIMARY KEY,
                space_id BLOB NOT NULL,
                author BLOB NOT NULL,
                seq INTEGER NOT NULL,
                hlc_wall INTEGER NOT NULL,
                hlc_counter INTEGER NOT NULL,
                kind TEXT NOT NULL,
                payload BLOB NOT NULL,
                signature BLOB NOT NULL,
                received_at INTEGER NOT NULL,
                UNIQUE (space_id, author, seq)
            );
            CREATE INDEX events_order ON events (space_id, hlc_wall, hlc_counter, event_id);
            CREATE TABLE author_heads (
                space_id BLOB NOT NULL,
                author BLOB NOT NULL,
                max_seq INTEGER NOT NULL,
                PRIMARY KEY (space_id, author)
            );
            CREATE TABLE members (
                space_id BLOB NOT NULL,
                user_pubkey BLOB NOT NULL,
                added_by BLOB NOT NULL,
                role TEXT NOT NULL DEFAULT 'member',
                PRIMARY KEY (space_id, user_pubkey)
            );
            CREATE TABLE channels (
                space_id BLOB NOT NULL,
                channel_id BLOB NOT NULL,
                kind TEXT NOT NULL CHECK (kind IN ('voice','text')),
                name TEXT NOT NULL,
                position INTEGER NOT NULL DEFAULT 0,
                settings TEXT,
                PRIMARY KEY (space_id, channel_id)
            );
            CREATE TABLE messages (
                space_id BLOB NOT NULL,
                channel_id BLOB NOT NULL,
                event_id BLOB PRIMARY KEY,
                author BLOB NOT NULL,
                hlc_wall INTEGER NOT NULL,
                hlc_counter INTEGER NOT NULL,
                body TEXT NOT NULL,
                reply_to BLOB,
                edited_hlc INTEGER,
                deleted INTEGER NOT NULL DEFAULT 0
            );
            CREATE INDEX messages_by_channel ON messages (channel_id, hlc_wall, hlc_counter, event_id);
            CREATE TABLE reactions (
                message_event_id BLOB NOT NULL,
                author BLOB NOT NULL,
                emoji TEXT NOT NULL,
                PRIMARY KEY (message_event_id, author, emoji)
            );
            CREATE TABLE attachments (
                message_event_id BLOB NOT NULL,
                blob_hash BLOB NOT NULL,
                name TEXT NOT NULL,
                size INTEGER NOT NULL,
                mime TEXT NOT NULL,
                thumb_hash BLOB,
                PRIMARY KEY (message_event_id, blob_hash)
            );",
        ),
        M::up(
            "ALTER TABLE friends ADD COLUMN note TEXT;
            CREATE TABLE hello_outbox (
                user_pubkey BLOB PRIMARY KEY,
                kind TEXT NOT NULL CHECK (kind IN ('request','accept','decline')),
                node_id BLOB NOT NULL,
                relay_url TEXT,
                attempts INTEGER NOT NULL DEFAULT 0,
                next_attempt INTEGER NOT NULL DEFAULT 0,
                created_at INTEGER NOT NULL
            );",
        ),
        M::up(
            "ALTER TABLE events ADD COLUMN subject BLOB;
            CREATE INDEX events_kind ON events (space_id, kind, subject);",
        ),
        // Welchen Stand andere Mitglieder von den Events eines Autors haben (Zustellstatus).
        M::up(
            "CREATE TABLE peer_acks (
                space_id BLOB NOT NULL,
                peer_user BLOB NOT NULL,
                author BLOB NOT NULL,
                seq INTEGER NOT NULL,
                PRIMARY KEY (space_id, peer_user, author)
            );",
        ),
        // Lokaler Spitzname (hat Vorrang vor dem selbst gewaehlten Namen) und ob der
        // Schluessel-Fingerprint ausserhalb der App verglichen wurde (M-8).
        M::up(
            "ALTER TABLE friends ADD COLUMN nickname TEXT;
            ALTER TABLE friends ADD COLUMN verified INTEGER NOT NULL DEFAULT 0;",
        ),
        // Ueber welchen Space eine Datei legitim auf dieses Geraet kam (eigenes Teilen oder ein
        // Download fuer einen Anhang in diesem Space). Ausgeliefert wird nur an Mitglieder dieser
        // Spaces (H-1). Bestand: der Space, in dem die Datei zuerst auftauchte.
        M::up(
            "CREATE TABLE blob_origin (
                blob_hash BLOB NOT NULL,
                space_id BLOB NOT NULL,
                PRIMARY KEY (blob_hash, space_id)
            );
            INSERT OR IGNORE INTO blob_origin (blob_hash, space_id)
            SELECT h, space_id FROM (
                SELECT a.blob_hash AS h, m.space_id,
                       ROW_NUMBER() OVER (PARTITION BY a.blob_hash
                                          ORDER BY m.hlc_wall, m.hlc_counter) AS rn
                  FROM attachments a JOIN messages m ON m.event_id = a.message_event_id
            ) WHERE rn = 1;
            INSERT OR IGNORE INTO blob_origin (blob_hash, space_id)
            SELECT h, space_id FROM (
                SELECT a.thumb_hash AS h, m.space_id,
                       ROW_NUMBER() OVER (PARTITION BY a.thumb_hash
                                          ORDER BY m.hlc_wall, m.hlc_counter) AS rn
                  FROM attachments a JOIN messages m ON m.event_id = a.message_event_id
                 WHERE a.thumb_hash IS NOT NULL
            ) WHERE rn = 1;",
        ),
        // `MemberRemove.last_seq` (M-1), damit `membership` per SQL danach filtern kann.
        M::up("ALTER TABLE events ADD COLUMN ref_seq INTEGER;"),
        // Einladungen erst nach Zustimmung (M-2). Bestehende Spaces gelten als angenommen.
        M::up(
            "CREATE TABLE accepted_spaces (
                space_id BLOB PRIMARY KEY,
                via_dm INTEGER NOT NULL DEFAULT 0
            );
            INSERT INTO accepted_spaces (space_id) SELECT space_id FROM spaces;
            CREATE VIEW shown_spaces AS
              SELECT a.space_id FROM accepted_spaces a
               WHERE NOT EXISTS (
                 SELECT 1 FROM members c JOIN friends f ON f.user_pubkey = c.user_pubkey
                  WHERE c.space_id = a.space_id AND c.role = 'admin' AND f.status = 'blocked');",
        ),
        // Einladungen aus den Heads (M-3): berechtigen zur Annahme, bis das `MemberAdd` im Log ist.
        M::up(
            "CREATE TABLE space_tickets (
                space_id BLOB PRIMARY KEY,
                add_event BLOB NOT NULL,
                add_wall INTEGER NOT NULL,
                add_counter INTEGER NOT NULL,
                creator BLOB NOT NULL
            );",
        ),
        // Einladungsgeheimnis aus dem Code des Angefragten (N-1).
        M::up("ALTER TABLE friends ADD COLUMN invite_secret BLOB;"),
        // Beweise fuer Equivocation (N-14): zwei vom selben Autor signierte Events mit derselben
        // seq. `kept_event` liegt im Log, `other` ist der vollstaendige Umschlag des anderen.
        M::up(
            "CREATE TABLE equivocations (
                space_id BLOB NOT NULL,
                author BLOB NOT NULL,
                seq INTEGER NOT NULL,
                kept_event BLOB NOT NULL,
                other BLOB NOT NULL,
                detected_at INTEGER NOT NULL,
                PRIMARY KEY (space_id, author, seq)
            );",
        ),
        // Hash des Wiederherstellungsschluessels eines Freundes (M-7), einmal festgehalten.
        // Outbox kennt zusaetzlich 'rotate' (eigener Schluesselwechsel).
        M::up(
            "ALTER TABLE friends ADD COLUMN recovery BLOB;
            CREATE TABLE hello_outbox_new (
                user_pubkey BLOB PRIMARY KEY,
                kind TEXT NOT NULL CHECK (kind IN ('request','accept','decline','rotate')),
                node_id BLOB NOT NULL,
                relay_url TEXT,
                attempts INTEGER NOT NULL DEFAULT 0,
                next_attempt INTEGER NOT NULL DEFAULT 0,
                created_at INTEGER NOT NULL
            );
            INSERT INTO hello_outbox_new SELECT user_pubkey, kind, node_id, relay_url, attempts,
                next_attempt, created_at FROM hello_outbox;
            DROP TABLE hello_outbox;
            ALTER TABLE hello_outbox_new RENAME TO hello_outbox;",
        ),
        // Speichergrenze fuer Nachrichten: Events signieren seitdem nur einen Inhalts-Hash, damit
        // aufgeraeumte Events als Platzhalter pruefbar bleiben. Events aus 0.6 sind anders
        // signiert und nicht mehr gueltig: Spaces und Nachrichten werden einmalig entfernt
        // (Freunde und Identitaet bleiben). `target`: worauf sich ein Event bezieht.
        M::up(
            "DELETE FROM events;
            DELETE FROM author_heads;
            DELETE FROM members;
            DELETE FROM channels;
            DELETE FROM messages;
            DELETE FROM reactions;
            DELETE FROM attachments;
            DELETE FROM spaces;
            DELETE FROM read_state;
            DELETE FROM peer_acks;
            DELETE FROM blob_origin;
            DELETE FROM accepted_spaces;
            DELETE FROM space_tickets;
            DELETE FROM equivocations;
            ALTER TABLE events ADD COLUMN target BLOB;
            ALTER TABLE events ADD COLUMN pruned INTEGER NOT NULL DEFAULT 0;
            CREATE INDEX events_target ON events (target) WHERE target IS NOT NULL;",
        ),
    ])
}
