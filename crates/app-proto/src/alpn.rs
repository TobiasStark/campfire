//! ALPN-Konstanten. Die Version steckt im ALPN.

/// Hauptverbindung zwischen Freunden bzw. Space-Mitgliedern.
pub const PEER: &[u8] = b"app/peer/1";
/// Freundschaftsanfragen von unbekannten Geraeten.
pub const HELLO: &[u8] = b"app/hello/1";

/// Datei-Transfer (iroh-blobs, M4-01). Muss mit `iroh_blobs::ALPN` uebereinstimmen (Test in app-blobs).
pub const BLOBS: &[u8] = b"/iroh-bytes/4";

/// Alle ALPNs, die der Endpoint annimmt.
pub const ALL: &[&[u8]] = &[PEER, HELLO, BLOBS];
