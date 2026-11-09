//! Pruefung von Device-Zertifikaten.

use app_proto::control::DeviceCert;
use ed25519_dalek::VerifyingKey;

/// Prueft die User-Signatur ueber (user, device, created_at).
pub fn verify_cert(cert: &DeviceCert) -> bool {
    let Ok(vk) = VerifyingKey::from_bytes(&cert.user) else {
        return false;
    };
    let msg = DeviceCert::signing_bytes(&cert.user, &cert.device, cert.created_at);
    let sig = ed25519_dalek::Signature::from_bytes(&cert.signature.0);
    vk.verify_strict(&msg, &sig).is_ok()
}
