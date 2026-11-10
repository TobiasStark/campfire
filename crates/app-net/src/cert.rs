//! Pruefung von Device-Zertifikaten.

use app_proto::control::DeviceCert;
use app_proto::{PublicKey, Signature};
use ed25519_dalek::{Signer, SigningKey, VerifyingKey};

/// Bindung des Rendezvous-Endpoints an das Geraet (M-5), signiert mit dem Geraeteschluessel.
pub fn sign_binding(device_secret: &[u8; 32], rendezvous: &PublicKey) -> Signature {
    let key = SigningKey::from_bytes(device_secret);
    let msg = app_proto::hello::binding_bytes(&key.verifying_key().to_bytes(), rendezvous);
    Signature(key.sign(&msg).to_bytes())
}

/// Prueft die Unterschrift des Wiederherstellungsschluessels unter einem Schluesselwechsel.
/// Ob der Schluessel der richtige ist, entscheidet der Aufrufer ueber `commitment`.
pub fn verify_rotation(r: &app_proto::recovery::Rotation) -> bool {
    let Ok(vk) = VerifyingKey::from_bytes(&r.recovery) else {
        return false;
    };
    let msg = app_proto::recovery::Rotation::signing_bytes(&r.old, &r.new, &r.next);
    r.old != r.new
        && vk
            .verify_strict(&msg, &ed25519_dalek::Signature::from_bytes(&r.signature.0))
            .is_ok()
}

/// Prueft, dass `rendezvous` (die Gegenstelle einer Hello-Verbindung) zu `device` gehoert.
pub fn verify_binding(device: &PublicKey, rendezvous: &PublicKey, sig: &Signature) -> bool {
    let Ok(vk) = VerifyingKey::from_bytes(device) else {
        return false;
    };
    let msg = app_proto::hello::binding_bytes(device, rendezvous);
    vk.verify_strict(&msg, &ed25519_dalek::Signature::from_bytes(&sig.0))
        .is_ok()
}

/// Spielraum fuer abweichende Uhren beim Ausstellungszeitpunkt.
const CERT_FUTURE_SECS: u64 = 24 * 3600;

/// Prueft die User-Signatur ueber (user, device, created_at). Zertifikate aus der Zukunft
/// (mehr als einen Tag) gelten nicht (M-7).
pub fn verify_cert(cert: &DeviceCert) -> bool {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    if cert.created_at > now.saturating_add(CERT_FUTURE_SECS) {
        return false;
    }
    let Ok(vk) = VerifyingKey::from_bytes(&cert.user) else {
        return false;
    };
    let msg = DeviceCert::signing_bytes(&cert.user, &cert.device, cert.created_at);
    let sig = ed25519_dalek::Signature::from_bytes(&cert.signature.0);
    vk.verify_strict(&msg, &sig).is_ok()
}
