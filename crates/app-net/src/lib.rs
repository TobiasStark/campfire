//! app-net: iroh-Endpoint, Verbindungsverwaltung, Allowlist, Protokoll-Router.
//!
//! Auf `app/peer/1` gilt (siehe docs/protocol.md):
//! * erster bidirektionaler Stream (vom Anrufer geoeffnet) = Control, beginnt mit dem Handshake
//! * weitere bidirektionale Streams beginnen mit einem Kanal-Byte ([`CHANNEL_SYNC`])
//! * unidirektionale Streams = Video, Datagramme = Voice

mod cert;
mod framing;
mod node;
pub mod rate;
pub mod video;

pub use cert::verify_cert;
pub use framing::{read_frame, read_frame_max, write_frame, FrameError};
pub use node::{
    Access, Net, NetConfig, NetError, NetEvent, NetMode, PathKind, PeerDirectory, PeerInfo,
    RelayConfig,
};

/// Kanal-Byte fuer Sync-Streams.
pub const CHANNEL_SYNC: u8 = 1;
/// Vom Client unterstuetzte Protokollversionen.
pub const SUPPORTED_VERSIONS: &[u16] = &[1];
