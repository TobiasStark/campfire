//! app-testkit: Test-Hilfen, u. a. Knoten im selben Prozess (nur Loopback, ohne Relay).

use app_net::{Access, Net, NetConfig, NetEvent, NetMode, PeerDirectory};
use app_proto::control::{DeviceCert, Handshake};
use app_proto::{PublicKey, Signature};
use ed25519_dalek::{Signer, SigningKey};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;

pub fn random_key() -> SigningKey {
    let mut seed = [0u8; 32];
    getrandom::fill(&mut seed).expect("Zufall");
    SigningKey::from_bytes(&seed)
}

/// Veraenderbare Allowlist.
#[derive(Default)]
pub struct StaticDirectory(Mutex<HashMap<PublicKey, Access>>);

impl StaticDirectory {
    pub fn set(&self, user: PublicKey, access: Access) {
        self.0.lock().unwrap().insert(user, access);
    }
}

impl PeerDirectory for StaticDirectory {
    fn check(&self, user: &PublicKey) -> Access {
        self.0
            .lock()
            .unwrap()
            .get(user)
            .copied()
            .unwrap_or(Access::Unknown)
    }
}

pub fn make_cert(user: &SigningKey, device: &SigningKey) -> DeviceCert {
    let (u, d) = (
        user.verifying_key().to_bytes(),
        device.verifying_key().to_bytes(),
    );
    let sig = user.sign(&DeviceCert::signing_bytes(&u, &d, 1));
    DeviceCert {
        user: u,
        device: d,
        created_at: 1,
        signature: Signature(sig.to_bytes()),
    }
}

pub struct TestNode {
    pub net: Net,
    pub rx: mpsc::Receiver<NetEvent>,
    pub user_key: SigningKey,
    pub device_key: SigningKey,
    pub user: PublicKey,
    pub device: PublicKey,
    pub dir: Arc<StaticDirectory>,
}

impl TestNode {
    pub async fn spawn(name: &str) -> Self {
        Self::spawn_with(name, random_key(), random_key(), None, None).await
    }

    /// `cert_device`: Zertifikat fuer einen anderen Geraeteschluessel (Faelschungstest).
    pub async fn spawn_with(
        name: &str,
        user_key: SigningKey,
        device_key: SigningKey,
        bind: Option<SocketAddr>,
        cert_device: Option<&SigningKey>,
    ) -> Self {
        let dir = Arc::new(StaticDirectory::default());
        let handshake = Handshake {
            versions: vec![1],
            cert: make_cert(&user_key, cert_device.unwrap_or(&device_key)),
            display_name: name.to_owned(),
            avatar_hash: None,
        };
        let mut cfg = NetConfig::new(device_key.to_bytes(), handshake, dir.clone());
        cfg.mode = NetMode::Loopback;
        cfg.bind_addr = Some(bind.unwrap_or_else(|| "127.0.0.1:0".parse().unwrap()));
        cfg.initial_backoff = Duration::from_millis(100);
        cfg.max_backoff = Duration::from_secs(2);
        // Ein fester Port wird nach einem Neustart auf langsamen Rechnern (CI) erst
        // verzoegert frei; daher kurz wiederholen.
        let mut tries = 0;
        let (net, rx) = loop {
            match Net::start(cfg.clone()).await {
                Ok(v) => break v,
                Err(e) if bind.is_some() && tries < 50 => {
                    tries += 1;
                    let _ = e;
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                Err(e) => panic!("Net::start: {e:?}"),
            }
        };
        Self {
            net,
            rx,
            user: user_key.verifying_key().to_bytes(),
            device: device_key.verifying_key().to_bytes(),
            user_key,
            device_key,
            dir,
        }
    }

    pub fn allow(&self, other: &TestNode) {
        self.dir.set(other.user, Access::Allowed);
    }

    /// Wartet auf ein Ereignis, das `f` annimmt; andere werden uebersprungen.
    pub async fn wait_for<T>(
        &mut self,
        timeout: Duration,
        mut f: impl FnMut(&NetEvent) -> Option<T>,
    ) -> Option<T> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            match tokio::time::timeout_at(deadline, self.rx.recv()).await {
                Ok(Some(ev)) => {
                    if let Some(v) = f(&ev) {
                        return Some(v);
                    }
                }
                _ => return None,
            }
        }
    }
}

/// Freier UDP-Port auf Loopback (kurz belegt, dann freigegeben).
pub fn free_udp_port() -> u16 {
    std::net::UdpSocket::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}
