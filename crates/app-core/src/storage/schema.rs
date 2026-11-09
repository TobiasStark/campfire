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
    ])
}
