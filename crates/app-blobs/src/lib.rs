//! Dateien teilen (M4-01): iroh-blobs mit eigenem Speicher, Zugriffspruefung und Downloads aus
//! mehreren Quellen.
//!
//! * Der Speicher (`FsStore`) liegt in einem Ordner (App: `%LOCALAPPDATA%\<app>\blobs`). Er ist
//!   inhaltsadressiert (BLAKE3); angefangene Downloads bleiben liegen und werden fortgesetzt.
//! * Ausgeliefert wird nur, was der [`Authorizer`] fuer das anfragende Geraet erlaubt. Push ist
//!   abgeschaltet, niemand kann bei uns etwas ablegen.
//! * Die eingehenden Verbindungen kommen ueber `app-net` (`NetEvent::Blobs`) und werden mit
//!   [`Blobs::accept`] uebergeben.

use app_proto::{Hash, PublicKey};
use futures_util::StreamExt;
use iroh::protocol::ProtocolHandler;
use iroh::{Endpoint, EndpointId};
use iroh_blobs::api::downloader::DownloadProgressItem;
use iroh_blobs::api::downloader::Downloader;
use iroh_blobs::provider::events::{
    AbortReason, ConnectMode, EventMask, EventSender, ObserveMode, ProviderMessage, RequestMode,
};
use iroh_blobs::store::fs::options::Options;
use iroh_blobs::store::fs::FsStore;
use iroh_blobs::store::{GcConfig, ProtectOutcome};
use iroh_blobs::BlobsProtocol;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Wie oft der Speicher aufraeumt (geraeumte Dateien werden erst dabei wirklich geloescht).
const GC_INTERVAL: Duration = Duration::from_secs(60);

/// Was beim Aufraeumen stehen bleiben muss. Alles andere im Speicher wird geloescht.
#[derive(Default)]
struct Keep {
    /// Vollstaendige Dateien, die wir behalten wollen (alles ausser dem Entfernten).
    known: HashSet<iroh_blobs::Hash>,
    /// Gerade laufende Downloads (angefangene Dateien).
    active: HashSet<iroh_blobs::Hash>,
}

#[derive(Debug, thiserror::Error)]
pub enum BlobError {
    #[error("Storage: {0}")]
    Store(String),
    #[error("Download failed: {0}")]
    Download(String),
    #[error("Invalid key")]
    Key,
    #[error(
        "{}",
        app_proto::i18n::tr(
            "The file is larger than announced",
            "Die Datei ist groesser als angegeben"
        )
    )]
    TooLarge,
}

/// Wer darf welche Datei abholen?
pub trait Authorizer: Send + Sync + 'static {
    /// `device`: das anfragende Geraet (bereits durch QUIC/TLS bewiesen).
    fn may_fetch(&self, device: &PublicKey, hash: &Hash) -> bool;

    /// Darf dieses Geraet ueberhaupt eine Datei-Verbindung oeffnen? Standard: ja. Die App
    /// erlaubt nur Geraete, mit denen schon eine authentifizierte Peer-Verbindung besteht
    /// (Freunde, Space-Mitglieder), damit Fremde keine Verbindungen und Abfragen erzeugen (M-6).
    fn may_connect(&self, device: &PublicKey) -> bool {
        let _ = device;
        true
    }
}

/// Hoechstens so viele gleichzeitige Datei-Verbindungen je Geraet und insgesamt.
const MAX_CONNS_PER_DEVICE: usize = 4;
const MAX_CONNS: usize = 64;

/// Erlaubt nichts.
pub struct DenyAll;

impl Authorizer for DenyAll {
    fn may_fetch(&self, _: &PublicKey, _: &Hash) -> bool {
        false
    }
}

fn to_id(d: &PublicKey) -> Result<EndpointId, BlobError> {
    EndpointId::from_bytes(d).map_err(|_| BlobError::Key)
}

/// Fragt `auth` fuer alle `hashes` ab. Die Pruefung darf blockieren (Datenbank), deshalb
/// laeuft sie auf einem Blocking-Thread statt auf dem Tokio-Worker.
async fn allowed(auth: &Arc<dyn Authorizer>, dev: Option<PublicKey>, hashes: Vec<Hash>) -> bool {
    let Some(d) = dev else {
        return false;
    };
    let auth = auth.clone();
    tokio::task::spawn_blocking(move || hashes.iter().all(|h| auth.may_fetch(&d, h)))
        .await
        .unwrap_or(false)
}

fn verdict(ok: bool) -> Result<(), AbortReason> {
    if ok {
        Ok(())
    } else {
        Err(AbortReason::Permission)
    }
}

pub struct Blobs {
    store: FsStore,
    proto: BlobsProtocol,
    auth: Arc<dyn Authorizer>,
    /// Offene eingehende Verbindungen je Geraet.
    open: Arc<Mutex<HashMap<PublicKey, usize>>>,
    downloader: Downloader,
    events: tokio::task::JoinHandle<()>,
    keep: Arc<Mutex<Keep>>,
    endpoint: Endpoint,
}

impl Blobs {
    /// Oeffnet (oder legt an) den Speicher in `dir`.
    pub async fn open(
        dir: &Path,
        endpoint: &Endpoint,
        auth: Arc<dyn Authorizer>,
    ) -> Result<Self, BlobError> {
        Self::open_with_gc(dir, endpoint, auth, GC_INTERVAL).await
    }

    /// Wie `open`, mit eigenem Aufraeum-Abstand (fuer Tests).
    pub async fn open_with_gc(
        dir: &Path,
        endpoint: &Endpoint,
        auth: Arc<dyn Authorizer>,
        gc_interval: Duration,
    ) -> Result<Self, BlobError> {
        let keep: Arc<Mutex<Keep>> = Arc::default();
        let protect = keep.clone();
        let mut options = Options::new(dir);
        options.gc = Some(GcConfig {
            interval: gc_interval,
            add_protected: Some(Arc::new(move |set: &mut HashSet<iroh_blobs::Hash>| {
                let keep = protect.clone();
                Box::pin(async move {
                    let k = keep
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    set.extend(k.known.iter().chain(k.active.iter()).copied());
                    ProtectOutcome::Continue
                })
            })),
        });
        let store = FsStore::load_with_opts(dir.join("blobs.db"), options)
            .await
            .map_err(|e| BlobError::Store(e.to_string()))?;
        // Alles, was schon im Speicher liegt, bleibt, bis es ausdruecklich entfernt wird.
        let existing = store
            .blobs()
            .list()
            .hashes()
            .await
            .map_err(|e| BlobError::Store(e.to_string()))?;
        keep.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .known
            .extend(existing);
        // Push bleibt aus (Standard der Maske): niemand darf bei uns Daten ablegen.
        let mask = EventMask {
            connected: ConnectMode::Intercept,
            get: RequestMode::Intercept,
            get_many: RequestMode::Intercept,
            observe: ObserveMode::Intercept,
            ..EventMask::DEFAULT
        };
        let (events, mut rx) = EventSender::channel(64, mask);
        let proto = BlobsProtocol::new(&store, Some(events));
        let (auth_conn, auth_keep) = (auth.clone(), auth.clone());
        let task = tokio::spawn(async move {
            // Verbindungs-ID -> Geraet, solange die Verbindung lebt.
            let mut conns: HashMap<u64, Option<PublicKey>> = HashMap::new();
            while let Some(msg) = rx.recv().await {
                match msg {
                    ProviderMessage::ClientConnected(m) => {
                        let dev = m.endpoint_id.map(|e| *e.as_bytes());
                        let ok = match dev {
                            Some(d) => {
                                let a = auth_conn.clone();
                                tokio::task::spawn_blocking(move || a.may_connect(&d))
                                    .await
                                    .unwrap_or(false)
                            }
                            None => false,
                        };
                        if ok {
                            conns.insert(m.connection_id, dev);
                        }
                        m.tx.send(verdict(ok)).await.ok();
                    }
                    ProviderMessage::ConnectionClosed(m) => {
                        conns.remove(&m.connection_id);
                    }
                    ProviderMessage::GetRequestReceived(m) => {
                        let dev = conns.get(&m.connection_id).copied().flatten();
                        let h = *m.request.hash.as_bytes();
                        let ok = allowed(&auth, dev, vec![h]).await;
                        m.tx.send(verdict(ok)).await.ok();
                    }
                    ProviderMessage::GetManyRequestReceived(m) => {
                        let dev = conns.get(&m.connection_id).copied().flatten();
                        let hashes = m.request.hashes.iter().map(|h| *h.as_bytes()).collect();
                        let ok = allowed(&auth, dev, hashes).await;
                        m.tx.send(verdict(ok)).await.ok();
                    }
                    ProviderMessage::ObserveRequestReceived(m) => {
                        let dev = conns.get(&m.connection_id).copied().flatten();
                        let h = *m.request.hash.as_bytes();
                        let ok = allowed(&auth, dev, vec![h]).await;
                        m.tx.send(verdict(ok)).await.ok();
                    }
                    // Push ist ueber die Maske abgeschaltet; der Rest sind Benachrichtigungen.
                    _ => {}
                }
            }
        });
        let downloader = store.downloader(endpoint);
        Ok(Self {
            store,
            proto,
            auth: auth_keep,
            open: Arc::default(),
            downloader,
            events: task,
            keep,
            endpoint: endpoint.clone(),
        })
    }

    /// Datei in den Speicher kopieren. Gibt Hash und Groesse zurueck.
    pub async fn import(&self, path: &Path) -> Result<(Hash, u64), BlobError> {
        let tag = self
            .store
            .add_path(path)
            .await
            .map_err(|e| BlobError::Store(e.to_string()))?;
        let size = std::fs::metadata(path)
            .map_err(|e| BlobError::Store(e.to_string()))?
            .len();
        self.keep
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .known
            .insert(tag.hash);
        Ok((*tag.hash.as_bytes(), size))
    }

    /// Kleine Daten (z. B. ein Vorschaubild) direkt in den Speicher legen.
    pub async fn import_bytes(&self, data: Vec<u8>) -> Result<Hash, BlobError> {
        let tag = self
            .store
            .add_bytes(data)
            .await
            .map_err(|e| BlobError::Store(e.to_string()))?;
        self.keep
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .known
            .insert(tag.hash);
        Ok(*tag.hash.as_bytes())
    }

    /// Eine vollstaendig vorhandene Datei in den Arbeitsspeicher lesen (nur fuer kleine Dateien).
    pub async fn read_bytes(&self, hash: &Hash) -> Result<Vec<u8>, BlobError> {
        let b = self
            .store
            .get_bytes(iroh_blobs::Hash::from_bytes(*hash))
            .await
            .map_err(|e| BlobError::Store(e.to_string()))?;
        Ok(b.to_vec())
    }

    /// Eingehende Datei-Verbindung bedienen (laeuft im Hintergrund).
    pub fn accept(&self, conn: iroh::endpoint::Connection) {
        let dev: PublicKey = *conn.remote_id().as_bytes();
        // Nur bekannte Geraete und hoechstens wenige Verbindungen je Geraet (M-6).
        if !self.auth.may_connect(&dev) {
            conn.close(1u32.into(), b"not allowed");
            return;
        }
        {
            let mut open = self
                .open
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let total: usize = open.values().sum();
            let n = open.entry(dev).or_default();
            if *n >= MAX_CONNS_PER_DEVICE || total >= MAX_CONNS {
                drop(open);
                conn.close(2u32.into(), b"busy");
                return;
            }
            *n += 1;
        }
        let (proto, open) = (self.proto.clone(), self.open.clone());
        tokio::spawn(async move {
            let _ = proto.accept(conn).await;
            let mut o = open
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(n) = o.get_mut(&dev) {
                *n -= 1;
                if *n == 0 {
                    o.remove(&dev);
                }
            }
        });
    }

    /// Liegt die Datei vollstaendig vor?
    pub async fn has(&self, hash: &Hash) -> bool {
        self.store
            .has(iroh_blobs::Hash::from_bytes(*hash))
            .await
            .unwrap_or(false)
    }

    /// Wie viele Bytes der Datei schon lokal liegen (fuer Fortschritt und Fortsetzen).
    pub async fn local_bytes(&self, hash: &Hash) -> u64 {
        match self
            .store
            .remote()
            .local(iroh_blobs::HashAndFormat::raw(
                iroh_blobs::Hash::from_bytes(*hash),
            ))
            .await
        {
            Ok(info) => info.local_bytes(),
            Err(_) => 0,
        }
    }

    /// Laedt die Datei von den angegebenen Geraeten (mehrere gleichzeitig nutzbar; fehlende Teile
    /// kommen von der naechsten Quelle). Schon vorhandene Teile werden nicht noch einmal geholt,
    /// ein abgebrochener Download setzt also fort. `progress` bekommt die Bytes, die insgesamt
    /// vorliegen (schon lokale eingerechnet).
    ///
    /// `max_bytes`: Obergrenze. Die Groesse im `FileShare` behauptet nur der Absender; der Hash
    /// kann auf eine viel groessere Datei zeigen. Wird die Grenze ueberschritten, bricht der
    /// Download ab ([`BlobError::TooLarge`]) und die angefangenen Teile raeumt das naechste
    /// Aufraeumen weg.
    pub async fn download(
        &self,
        hash: &Hash,
        providers: &[PublicKey],
        max_bytes: u64,
        progress: impl FnMut(u64) + Send,
    ) -> Result<(), BlobError> {
        let h = iroh_blobs::Hash::from_bytes(*hash);
        {
            let mut k = self
                .keep
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            k.active.insert(h);
            k.known.insert(h);
        }
        let res = self
            .download_inner(hash, providers, max_bytes, progress)
            .await;
        let mut k = self
            .keep
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        k.active.remove(&h);
        if res.is_err() {
            // Angefangene Teile gelten nicht als behalten, aber bleiben bis zum naechsten Lauf.
            k.known.remove(&h);
        }
        res
    }

    async fn download_inner(
        &self,
        hash: &Hash,
        providers: &[PublicKey],
        max_bytes: u64,
        mut progress: impl FnMut(u64) + Send,
    ) -> Result<(), BlobError> {
        if self.local_bytes(hash).await > max_bytes {
            return Err(BlobError::TooLarge);
        }
        let ids: Vec<EndpointId> = providers.iter().map(to_id).collect::<Result<_, _>>()?;
        let h = iroh_blobs::Hash::from_bytes(*hash);
        let mut s = self
            .downloader
            .download(h, ids)
            .stream()
            .await
            .map_err(|e| BlobError::Download(e.to_string()))?;
        while let Some(item) = s.next().await {
            match item {
                // Den Stream fallen zu lassen beendet auch den Download-Task (sein naechstes
                // Senden scheitert).
                DownloadProgressItem::Progress(n) if n > max_bytes => {
                    return Err(BlobError::TooLarge)
                }
                DownloadProgressItem::Progress(n) => progress(n),
                DownloadProgressItem::Error(e) => return Err(BlobError::Download(e.to_string())),
                DownloadProgressItem::DownloadError => {
                    return Err(BlobError::Download(
                        app_proto::i18n::tr(
                            "no source delivered the file",
                            "keine Quelle lieferte die Datei",
                        )
                        .into(),
                    ))
                }
                _ => {}
            }
        }
        if self.has(hash).await {
            if self.local_bytes(hash).await > max_bytes {
                return Err(BlobError::TooLarge);
            }
            Ok(())
        } else {
            Err(BlobError::Download("Datei unvollstaendig".into()))
        }
    }

    /// Datei aus dem Speicher an einen Zielort schreiben.
    pub async fn export(&self, hash: &Hash, dest: &Path) -> Result<(), BlobError> {
        self.store
            .export(iroh_blobs::Hash::from_bytes(*hash), dest)
            .await
            .map_err(|e| BlobError::Store(e.to_string()))?;
        Ok(())
    }

    /// Welche der Geraete haben die Datei vollstaendig und liefern sie an uns aus? Fragt alle
    /// gleichzeitig (je hoechstens `timeout`); Geraete, die nicht antworten oder ablehnen, fehlen.
    pub async fn who_has(
        &self,
        hash: &Hash,
        devices: &[PublicKey],
        timeout: Duration,
    ) -> Vec<PublicKey> {
        let h = iroh_blobs::Hash::from_bytes(*hash);
        let remote = self.store.remote();
        let probes = devices.iter().map(|dev| {
            let (endpoint, remote, dev) = (self.endpoint.clone(), remote.clone(), *dev);
            async move {
                let probe = async {
                    let id = to_id(&dev).ok()?;
                    let conn = endpoint.connect(id, iroh_blobs::ALPN).await.ok()?;
                    let mut s = Box::pin(
                        remote.observe(conn, iroh_blobs::protocol::ObserveRequest::new(h)),
                    );
                    let first = s.next().await?.ok()?;
                    first.is_complete().then_some(dev)
                };
                tokio::time::timeout(timeout, probe).await.ok().flatten()
            }
        });
        futures_util::future::join_all(probes)
            .await
            .into_iter()
            .flatten()
            .collect()
    }

    /// Alle Dateien, die der Speicher behaelt (mit Tag), auch solche, auf die keine Nachricht
    /// mehr verweist.
    pub async fn list(&self) -> Result<Vec<Hash>, BlobError> {
        let mut out = std::collections::HashSet::new();
        let mut s = Box::pin(
            self.store
                .tags()
                .list()
                .await
                .map_err(|e| BlobError::Store(e.to_string()))?,
        );
        while let Some(Ok(t)) = s.next().await {
            out.insert(*t.hash.as_bytes());
        }
        Ok(out.into_iter().collect())
    }

    /// Datei aus dem Speicher entfernen (wird beim naechsten Aufraeumen wirklich geloescht).
    /// Kommt sie spaeter wieder per Download oder Import, ist sie wieder normal da.
    pub async fn evict(&self, hash: &Hash) -> Result<(), BlobError> {
        let h = iroh_blobs::Hash::from_bytes(*hash);
        self.keep
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .known
            .remove(&h);
        let tags = self.store.tags();
        let mut names = Vec::new();
        {
            let mut s = Box::pin(
                tags.list()
                    .await
                    .map_err(|e| BlobError::Store(e.to_string()))?,
            );
            while let Some(Ok(t)) = s.next().await {
                if t.hash == h {
                    names.push(t.name);
                }
            }
        }
        for n in names {
            tags.delete(n)
                .await
                .map_err(|e| BlobError::Store(e.to_string()))?;
        }
        Ok(())
    }

    pub async fn shutdown(&self) {
        self.events.abort();
        let _ = self.store.shutdown().await;
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn alpn_matches_iroh_blobs() {
        assert_eq!(app_proto::alpn::BLOBS, iroh_blobs::ALPN);
    }
}
