//! Dateien teilen und holen (M4-02): Dienst zwischen Oberflaeche, Blob-Speicher und Event-Log.

use crate::signals::{send, Signal};
use app_blobs::{Authorizer, Blobs};
use app_core::eventlog::chat::blob_allowed;
use app_core::eventlog::sync::SyncService;
use app_core::files::{guess_mime, sanitize_file_name};
use app_core::settings::AutoDownload;
use app_core::storage::Db;
use app_net::Net;
use app_proto::event::{ChannelId, EventKind, SpaceId};
use app_proto::lock::Locked;
use app_proto::{Hash, PublicKey};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// Groesste Datei, die geteilt werden darf (Schutz vor versehentlichen Riesen-Importen).
pub use app_proto::event::MAX_FILE_BYTES;

/// Pruefung fuer den Blob-Speicher: Gerät -> Nutzer (aus dem Handshake) -> Mitglied eines Spaces,
/// in dem die Datei angehaengt ist.
pub struct SpaceAuth {
    pub db: Db,
    pub net: Net,
    /// Eigener Nutzer: ausgeliefert wird nur aus Spaces, in denen auch wir Mitglied sind.
    pub me: PublicKey,
}

impl Authorizer for SpaceAuth {
    /// Nur Geraete mit bestehender, authentifizierter Peer-Verbindung (Allowlist).
    fn may_connect(&self, device: &PublicKey) -> bool {
        self.net.peers().iter().any(|p| p.device == *device)
    }

    fn may_fetch(&self, device: &PublicKey, hash: &Hash) -> bool {
        let Some(user) = self
            .net
            .peers()
            .into_iter()
            .find(|p| p.device == *device)
            .map(|p| p.user)
        else {
            return false;
        };
        let (h, me) = (*hash, self.me);
        self.db
            .call(move |c| blob_allowed(c, &me, &user, &h))
            .unwrap_or(false)
    }
}

/// Zustand einer Datei aus Sicht der Oberflaeche.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileState {
    /// Liegt lokal vor.
    Local,
    /// Noch nicht geladen.
    Remote,
    Downloading {
        done: u64,
        total: u64,
    },
    Failed(String),
}

#[derive(Default)]
struct State {
    local: HashSet<Hash>,
    downloading: HashMap<Hash, (u64, u64)>,
    failed: HashMap<Hash, String>,
    /// Fortschritt beim Teilen, z. B. "Datei 2 von 3: bild.png".
    sharing: Option<String>,
    share_error: Option<String>,
    /// Dateien, fuer die schon ein automatischer Download lief (nicht endlos wiederholen).
    auto_tried: HashSet<Hash>,
    /// Fertig geladene Vorschaubilder.
    thumbs: HashMap<Hash, iced::widget::image::Handle>,
    /// Reihenfolge der geladenen Vorschauen (aelteste vorn) fuer die Obergrenze `MAX_THUMBS`.
    thumb_order: std::collections::VecDeque<Hash>,
    /// Vorschauen, die schon angefragt wurden.
    thumb_tried: HashSet<Hash>,
    /// Angepinnte und selbst geteilte Dateien: werden nie entfernt.
    pinned: HashSet<Hash>,
    /// Wer die Datei gerade hat: (Zeitpunkt der Pruefung, Anzahl).
    avail: HashMap<Hash, (std::time::Instant, usize)>,
    /// Gescheiterte Downloads (Hash -> Groesse, Space des Anhangs): starten von selbst, sobald
    /// eine Quelle online ist.
    wanted: HashMap<Hash, (u64, SpaceId)>,
    /// Vom Datei-Speicher belegte Bytes (Stand des letzten Aufraeumens).
    used: u64,
}

#[derive(Clone)]
pub struct FileService {
    blobs: Option<Arc<Blobs>>,
    sync: SyncService,
    net: Net,
    db: Db,
    me: PublicKey,
    auto: Arc<Mutex<AutoDownload>>,
    cache_mb: Arc<Mutex<u64>>,
    state: Arc<Mutex<State>>,
}

impl FileService {
    pub fn new(
        blobs: Option<Arc<Blobs>>,
        sync: SyncService,
        net: Net,
        db: Db,
        me: PublicKey,
    ) -> Self {
        let auto: AutoDownload = db
            .call(|c| app_core::settings::get(c, app_core::settings::FILES_AUTO))
            .ok()
            .flatten()
            .unwrap_or_default();
        let cache = db
            .call(|c| app_core::settings::get::<u64>(c, app_core::settings::FILES_CACHE_MB))
            .ok()
            .flatten()
            .unwrap_or(app_core::settings::FILES_CACHE_MB_DEFAULT);
        let pinned: HashSet<Hash> = db
            .call(|c| app_core::settings::get::<Vec<Hash>>(c, app_core::settings::FILES_PINNED))
            .ok()
            .flatten()
            .unwrap_or_default()
            .into_iter()
            .collect();
        Self {
            blobs,
            sync,
            net,
            db,
            me,
            auto: Arc::new(Mutex::new(auto)),
            cache_mb: Arc::new(Mutex::new(cache)),
            state: Arc::new(Mutex::new(State {
                pinned,
                ..State::default()
            })),
        }
    }

    /// Groesse des Datei-Speichers in MB; 0 = unbegrenzt.
    pub fn cache_mb(&self) -> u64 {
        *self.cache_mb.locked()
    }

    pub fn set_cache_mb(&self, mb: u64) {
        *self.cache_mb.locked() = mb;
        let _ = self.db.spawn(
            move |c| app_core::settings::set(c, app_core::settings::FILES_CACHE_MB, &mb),
            |_| {},
        );
        let svc = self.clone();
        tokio::spawn(async move { svc.enforce_cache().await });
    }

    /// Vom Datei-Speicher belegte Bytes (Stand des letzten Aufraeumens).
    pub fn used(&self) -> u64 {
        self.state.locked().used
    }

    /// Bei wie vielen gerade online erreichbaren Personen die Datei liegt (`None`: noch nicht
    /// geprueft).
    pub fn availability(&self, hash: &Hash) -> Option<usize> {
        self.state.locked().avail.get(hash).map(|a| a.1)
    }

    /// Prueft im Hintergrund, wer die (nicht lokalen) Dateien anbietet. Pro Datei hoechstens alle
    /// 30 s (`force`: sofort). Haengt eine Datei in der Wunschliste und ist jemand da, startet der
    /// Download.
    pub fn check_availability(&self, hashes: Vec<(Hash, SpaceId)>, force: bool) {
        let Some(blobs) = self.blobs.clone() else {
            return;
        };
        let todo: Vec<(Hash, SpaceId)> = {
            let st = self.state.locked();
            hashes
                .into_iter()
                .filter(|(h, _)| {
                    !st.local.contains(h)
                        && !st.downloading.contains_key(h)
                        && (force
                            || st
                                .avail
                                .get(h)
                                .is_none_or(|(t, _)| t.elapsed().as_secs() >= 30))
                })
                .collect()
        };
        if todo.is_empty() {
            return;
        }
        let svc = self.clone();
        tokio::spawn(async move {
            for (h, space) in todo {
                // Nur Mitglieder des Spaces fragen, in dem der Anhang liegt (H-1): sonst
                // erfaehrt jeder verbundene Freund die Hashes aller Dateien.
                let devices = svc.member_devices(&space);
                let n = if devices.is_empty() {
                    0
                } else {
                    blobs
                        .who_has(&h, &devices, std::time::Duration::from_secs(4))
                        .await
                        .len()
                };
                let want = {
                    let mut st = svc.state.locked();
                    st.avail.insert(h, (std::time::Instant::now(), n));
                    st.wanted.get(&h).copied()
                };
                if let (Some((size, space)), true) = (want, n > 0) {
                    svc.download(h, size, space);
                }
            }
            send(Signal::FilesChanged);
        });
    }

    /// Jemand ist online gekommen oder die Minute ist um: Wunschliste neu pruefen.
    pub fn retry_wanted(&self) {
        let hashes: Vec<(Hash, SpaceId)> = self
            .state
            .locked()
            .wanted
            .iter()
            .map(|(h, (_, sp))| (*h, *sp))
            .collect();
        if !hashes.is_empty() {
            self.check_availability(hashes, true);
        }
    }

    pub fn is_pinned(&self, hash: &Hash) -> bool {
        self.state.locked().pinned.contains(hash)
    }

    pub fn toggle_pin(&self, hash: Hash) {
        {
            let mut st = self.state.locked();
            if !st.pinned.remove(&hash) {
                st.pinned.insert(hash);
            }
        }
        persist_pins(&self.db, &self.state);
        send(Signal::FilesChanged);
    }

    /// Merkt die Nutzung einer Datei: aufgeraeumt wird, was am laengsten ungenutzt ist.
    fn touch(&self, hash: Hash) {
        let now = app_core::eventlog::sync::now_ms() as i64 / 1000;
        let _ = self.db.spawn(
            move |c| app_core::eventlog::chat::touch_blob(c, &hash, now),
            |_| {},
        );
    }

    /// Zaehlt, was der Speicher belegt, und entfernt die am laengsten ungenutzten, nicht
    /// angepinnten Dateien, bis er in die eingestellte Groesse passt. Dateien, auf die keine
    /// Nachricht mehr verweist (aufgeraeumt, Space verlassen), kommen ganz weg.
    pub async fn enforce_cache(&self) {
        let Some(blobs) = self.blobs.clone() else {
            return;
        };
        // Waehrend des Teilens ist die neue Datei kurz ohne Nachricht: dann nicht aufraeumen.
        if self.sharing().is_none() {
            let refs = self
                .db
                .call(|c| app_core::eventlog::chat::referenced_blobs(c));
            if let (Ok(all), Ok(refs)) = (blobs.list().await, refs) {
                let pinned = self.state.locked().pinned.clone();
                for h in all {
                    if !refs.contains(&h) && !pinned.contains(&h) && blobs.evict(&h).await.is_ok() {
                        self.state.locked().local.remove(&h);
                    }
                }
            }
        }
        let rows = self
            .db
            .call(|c| app_core::eventlog::chat::attachments_oldest_first(c))
            .unwrap_or_default();
        let mut files = Vec::new();
        for (h, _) in rows {
            if self.state.locked().local.contains(&h) {
                let n = blobs.local_bytes(&h).await;
                files.push((h, n));
            }
        }
        let pinned = self.state.locked().pinned.clone();
        let max = self.cache_mb().saturating_mul(1024 * 1024);
        let drop_list = app_core::files::plan_eviction(&files, &pinned, max);
        let mut used: u64 = files.iter().map(|f| f.1).sum();
        for h in drop_list {
            if blobs.evict(&h).await.is_ok() {
                used -= files.iter().find(|f| f.0 == h).map_or(0, |f| f.1);
                self.state.locked().local.remove(&h);
            }
        }
        self.state.locked().used = used;
        send(Signal::FilesChanged);
    }

    /// Datei an einen vom Nutzer gewaehlten Ort schreiben ("Speichern unter").
    pub async fn export_path(&self, hash: &Hash, dest: &Path) -> Result<PathBuf, String> {
        let blobs = self.blobs.clone().ok_or(app_core::i18n::tr(
            "File storage unavailable",
            "Dateispeicher nicht verfügbar",
        ))?;
        blobs.export(hash, dest).await.map_err(|e| e.to_string())?;
        self.touch(*hash);
        let name = dest
            .file_name()
            .map(|n| n.to_string_lossy())
            .unwrap_or_default();
        if !mark_of_the_web(dest) && needs_mark(&name) {
            let _ = std::fs::remove_file(dest);
            return Err(no_mark_error());
        }
        Ok(dest.to_path_buf())
    }

    /// Was die App von selbst laedt (Standard: nichts).
    pub fn auto(&self) -> AutoDownload {
        *self.auto.locked()
    }

    pub fn set_auto(&self, a: AutoDownload) {
        *self.auto.locked() = a;
        let _ = self.db.spawn(
            move |c| app_core::settings::set(c, app_core::settings::FILES_AUTO, &a),
            |_| {},
        );
        self.auto_fetch(false);
    }

    /// Laedt neue Anhaenge automatisch, soweit der Nutzer ihre Art freigegeben hat
    /// ([`AutoDownload`]). `retry`: auch Dateien, die schon einmal gescheitert sind (z. B.
    /// weil niemand online war).
    pub fn auto_fetch(&self, retry: bool) {
        let Some(blobs) = self.blobs.clone() else {
            return;
        };
        let auto = self.auto();
        if !auto.any_files() && !retry {
            return;
        }
        let svc = self.clone();
        tokio::spawn(async move {
            if retry {
                svc.enforce_cache().await;
                let s = svc.clone();
                let _ = tokio::task::spawn_blocking(move || s.clean_opened_orphans()).await;
            }
            if !auto.any_files() {
                return;
            }
            let max = svc.cache_mb().saturating_mul(1024 * 1024);
            let mut used = svc.used();
            let me = svc.me;
            let rows = svc
                .db
                .call(move |c| app_core::eventlog::chat::recent_attachments(c, &me, 100))
                .unwrap_or_default();
            if retry {
                {
                    let mut st = svc.state.locked();
                    st.auto_tried.clear();
                    st.thumb_tried.clear();
                }
            }
            for (h, size, name, space) in rows {
                if !auto.allows(&name, size) || (max > 0 && used.saturating_add(size) > max) {
                    continue;
                }
                {
                    let st = svc.state.locked();
                    if st.local.contains(&h)
                        || st.downloading.contains_key(&h)
                        || st.auto_tried.contains(&h)
                    {
                        continue;
                    }
                }
                if blobs.has(&h).await {
                    svc.state.locked().local.insert(h);
                    send(Signal::FilesChanged);
                    continue;
                }
                svc.state.locked().auto_tried.insert(h);
                used = used.saturating_add(size);
                svc.download(h, size, space);
            }
        });
    }

    /// Vorschaubild, falls schon geladen.
    pub fn thumb(&self, hash: &Hash) -> Option<iced::widget::image::Handle> {
        self.state.locked().thumbs.get(hash).cloned()
    }

    /// Holt Vorschaubilder immer automatisch (sie sind klein, hoechstens 100 KB). Dass der Hash
    /// wirklich auf ein kleines Bild zeigt, behauptet nur der Absender: geladen wird hoechstens
    /// `MAX_THUMB_BYTES`, dekodiert mit festen Grenzen (`decode_limited`).
    pub fn want_thumbs(&self, hashes: Vec<Hash>, space: SpaceId) {
        // Vorschaubilder nur, wenn der Nutzer das erlaubt hat; sonst per Klick (`load_thumb`).
        if self.auto().previews {
            self.fetch_thumbs(hashes, space, false);
        }
    }

    /// Vorschaubild auf ausdruecklichen Wunsch laden (Klick auf "Vorschau laden").
    pub fn load_thumb(&self, hash: Hash, space: SpaceId) {
        self.fetch_thumbs(vec![hash], space, true);
    }

    fn fetch_thumbs(&self, hashes: Vec<Hash>, space: SpaceId, force: bool) {
        let Some(blobs) = self.blobs.clone() else {
            return;
        };
        let todo: Vec<Hash> = {
            let mut st = self.state.locked();
            hashes
                .into_iter()
                .filter(|h| !st.thumbs.contains_key(h) && (st.thumb_tried.insert(*h) || force))
                .collect()
        };
        if todo.is_empty() {
            return;
        }
        let (st, svc) = (self.state.clone(), self.clone());
        tokio::spawn(async move {
            for h in todo {
                if !blobs.has(&h).await {
                    let providers = svc.member_devices(&space);
                    match blobs
                        .download(&h, &providers, MAX_THUMB_BYTES, |_| {})
                        .await
                    {
                        Ok(()) => svc.add_origin(h, space),
                        // Kein Vorschaubild: nicht wiederholen.
                        Err(app_blobs::BlobError::TooLarge) => continue,
                        Err(_) => {
                            // Spaeter nochmal versuchen (z. B. wenn der Absender online kommt).
                            st.locked().thumb_tried.remove(&h);
                            continue;
                        }
                    }
                }
                // Liegt die Datei schon lokal (z. B. weil der Hash auf eine grosse Datei zeigt),
                // nicht komplett in den Speicher lesen.
                if blobs.local_bytes(&h).await > MAX_THUMB_BYTES {
                    continue;
                }
                let Ok(bytes) = blobs.read_bytes(&h).await else {
                    continue;
                };
                let decoded = tokio::task::spawn_blocking(move || {
                    decode_limited(&bytes, THUMB_MAX_EDGE, 16 << 20, THUMB_MAX_EDGE)
                })
                .await
                .ok()
                .flatten();
                if let Some(handle) = decoded {
                    let mut s = st.locked();
                    s.thumbs.insert(h, handle);
                    s.thumb_order.push_back(h);
                    while s.thumb_order.len() > MAX_THUMBS {
                        if let Some(old) = s.thumb_order.pop_front() {
                            s.thumbs.remove(&old);
                            // Darf bei Bedarf wieder geladen werden (liegt dann lokal).
                            s.thumb_tried.remove(&old);
                        }
                    }
                    drop(s);
                    send(Signal::FilesChanged);
                }
            }
        });
    }

    /// Das ganze Bild fuer die Grossansicht (nur wenn die Datei lokal liegt und nicht riesig ist).
    pub async fn load_image(&self, hash: &Hash) -> Option<iced::widget::image::Handle> {
        let blobs = self.blobs.clone()?;
        if blobs.local_bytes(hash).await > MAX_VIEW_BYTES {
            return None;
        }
        let bytes = blobs.read_bytes(hash).await.ok()?;
        self.touch(*hash);
        tokio::task::spawn_blocking(move || {
            decode_limited(&bytes, VIEW_MAX_EDGE, 512 << 20, VIEW_FIT_EDGE)
        })
        .await
        .ok()
        .flatten()
    }

    pub fn sharing(&self) -> Option<String> {
        self.state.locked().sharing.clone()
    }

    pub fn share_error(&self) -> Option<String> {
        self.state.locked().share_error.clone()
    }

    pub fn state(&self, hash: &Hash) -> FileState {
        let st = self.state.locked();
        if st.local.contains(hash) {
            FileState::Local
        } else if let Some((done, total)) = st.downloading.get(hash) {
            FileState::Downloading {
                done: *done,
                total: *total,
            }
        } else if let Some(e) = st.failed.get(hash) {
            FileState::Failed(e.clone())
        } else {
            FileState::Remote
        }
    }

    /// Prueft im Hintergrund, welche der Dateien schon lokal liegen.
    pub fn probe(&self, hashes: Vec<Hash>) {
        let Some(blobs) = self.blobs.clone() else {
            return;
        };
        let st = self.state.clone();
        tokio::spawn(async move {
            let mut changed = false;
            for h in hashes {
                if st.locked().local.contains(&h) {
                    continue;
                }
                if blobs.has(&h).await {
                    st.locked().local.insert(h);
                    changed = true;
                }
            }
            if changed {
                send(Signal::FilesChanged);
            }
        });
    }

    /// Dateien in den Speicher nehmen und je ein `FileShare`-Event im Channel erzeugen.
    /// Mit `text` geht vorher eine normale Nachricht raus.
    pub fn share(
        &self,
        space: SpaceId,
        channel: ChannelId,
        text: Option<String>,
        paths: Vec<PathBuf>,
    ) {
        let Some(blobs) = self.blobs.clone() else {
            self.state.locked().share_error = Some(
                app_core::i18n::tr(
                    "File storage is unavailable.",
                    "Der Dateispeicher ist nicht verfügbar.",
                )
                .to_owned(),
            );
            send(Signal::FilesChanged);
            return;
        };
        let (sync, st, db) = (self.sync.clone(), self.state.clone(), self.db.clone());
        tokio::spawn(async move {
            st.locked().share_error = None;
            if let Some(body) = text.filter(|t| !t.trim().is_empty()) {
                let kind = EventKind::Message {
                    channel,
                    body,
                    reply_to: None,
                };
                if let Err(e) = sync.publish(Some(space), kind).await {
                    st.locked().share_error = Some(e.to_string());
                }
            }
            let total = paths.len();
            for (i, path) in paths.iter().enumerate() {
                let name = sanitize_file_name(&path.to_string_lossy());
                st.locked().sharing = Some(app_core::tr!(
                    "File {} of {total}: {name}",
                    "Datei {} von {total}: {name}",
                    i + 1
                ));
                send(Signal::FilesChanged);
                match share_one(&blobs, &db, &sync, &st, space, channel, path, &name).await {
                    // Selbst geteilte Dateien sind oft die einzige Kopie: nie automatisch entfernen.
                    Ok(h) => {
                        st.locked().pinned.insert(h);
                        persist_pins(&db, &st);
                    }
                    Err(e) => st.locked().share_error = Some(format!("{name}: {e}")),
                }
            }
            st.locked().sharing = None;
            send(Signal::FilesChanged);
        });
    }

    /// Geraete der verbundenen Mitglieder eines Spaces.
    fn member_devices(&self, space: &SpaceId) -> Vec<PublicKey> {
        let sp = *space;
        let members = self
            .db
            .call(move |c| app_core::eventlog::chat::space_member_users(c, &sp))
            .unwrap_or_default();
        self.net
            .peers()
            .into_iter()
            .filter(|p| members.contains(&p.user))
            .map(|p| p.device)
            .collect()
    }

    /// Merkt, dass die Datei ueber `space` hierher kam (H-1).
    fn add_origin(&self, hash: Hash, space: SpaceId) {
        let _ = self.db.spawn(
            move |c| app_core::eventlog::chat::add_blob_origin(c, &hash, &space),
            |_| {},
        );
    }

    /// Datei fuer einen Anhang in `space` holen, nur von verbundenen Mitgliedern dieses Spaces
    /// (H-1). Wer sie nicht ausliefern darf, lehnt ab; der Downloader nimmt die naechste Quelle.
    pub fn download(&self, hash: Hash, size: u64, space: SpaceId) {
        let Some(blobs) = self.blobs.clone() else {
            return;
        };
        {
            let mut st = self.state.locked();
            if st.downloading.contains_key(&hash) || st.local.contains(&hash) {
                return;
            }
            st.failed.remove(&hash);
            st.downloading.insert(hash, (0, size));
        }
        let providers = self.member_devices(&space);
        let (st, svc) = (self.state.clone(), self.clone());
        tokio::spawn(async move {
            let st2 = st.clone();
            // Die Groesse aus dem FileShare ist zugleich die Obergrenze: zeigt der Hash auf eine
            // groessere Datei, bricht der Download ab.
            let res = blobs
                .download(&hash, &providers, size, move |n| {
                    if let Some(e) = st2.locked().downloading.get_mut(&hash) {
                        e.0 = n;
                    }
                    send(Signal::FilesChanged);
                })
                .await;
            let mut s = st.locked();
            s.downloading.remove(&hash);
            match res {
                Ok(()) => {
                    s.local.insert(hash);
                    s.wanted.remove(&hash);
                    svc.add_origin(hash, space);
                    svc.touch(hash);
                }
                Err(app_blobs::BlobError::TooLarge) => {
                    s.failed.insert(
                        hash,
                        app_core::i18n::tr(
                            "The file is larger than announced. Download aborted.",
                            "Die Datei ist größer als angegeben. Download abgebrochen.",
                        )
                        .to_owned(),
                    );
                }
                Err(e) => {
                    s.wanted.insert(hash, (size, space));
                    let msg = if providers.is_empty() {
                        app_core::i18n::tr("Nobody with this file is online right now. Starts automatically when someone comes online.", "Niemand mit dieser Datei ist gerade online. Startet von selbst, sobald jemand kommt.").to_owned()
                    } else {
                        app_core::tr!("Download failed ({e}). Starts automatically when someone is online.", "Download fehlgeschlagen ({e}). Startet von selbst, sobald jemand online ist.")
                    };
                    s.failed.insert(hash, msg);
                }
            }
            drop(s);
            send(Signal::FilesChanged);
        });
    }

    /// Schreibt die Datei nach `dir` (ohne etwas zu ueberschreiben) und gibt den Pfad zurueck.
    pub async fn export_to(&self, hash: &Hash, name: &str, dir: &Path) -> Result<PathBuf, String> {
        let blobs = self.blobs.clone().ok_or(app_core::i18n::tr(
            "File storage unavailable",
            "Dateispeicher nicht verfügbar",
        ))?;
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        let dest = reserve_unique(dir, &sanitize_file_name(name)).map_err(|e| e.to_string())?;
        if let Err(e) = blobs.export(hash, &dest).await {
            let _ = std::fs::remove_file(&dest);
            return Err(e.to_string());
        }
        if !mark_of_the_web(&dest) && needs_mark(name) {
            let _ = std::fs::remove_file(&dest);
            return Err(no_mark_error());
        }
        self.touch(*hash);
        Ok(dest)
    }

    /// Entfernt geoeffnete Kopien von Anhaengen, deren Nachricht geloescht wurde (I-5).
    pub fn clean_opened_orphans(&self) {
        let Ok(live) = self
            .db
            .call(|c| app_core::eventlog::chat::live_attachment_hashes(c))
        else {
            return;
        };
        let keep: HashSet<String> = live.iter().map(hex).collect();
        let Ok(entries) = std::fs::read_dir(opened_dir()) else {
            return;
        };
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if e.file_type().is_ok_and(|t| t.is_dir()) && !keep.contains(&name) {
                let _ = std::fs::remove_dir_all(e.path());
            }
        }
    }
}

fn hex(h: &Hash) -> String {
    h.iter().map(|b| format!("{b:02x}")).collect()
}

/// Ordner fuer die geoeffnete Kopie eines Anhangs: je Datei ein eigener Unterordner, damit
/// er beim Loeschen der Nachricht gezielt entfernt werden kann (I-5).
pub fn opened_dir_for(hash: &Hash) -> PathBuf {
    opened_dir().join(hex(hash))
}

/// Dateien, die ohne Mark-of-the-Web nicht herausgegeben werden: alles ausser Bildern,
/// Videos und Audio (Programme, Archive, Office/PDF, Unbekanntes) (N-10).
fn needs_mark(name: &str) -> bool {
    use app_core::files::FileKind;
    !matches!(
        app_core::files::file_kind(name),
        FileKind::Image | FileKind::Video | FileKind::Audio
    )
}

fn no_mark_error() -> String {
    app_core::i18n::tr(
        "This drive cannot mark the file as downloaded from the internet (e.g. a USB stick with FAT32), so Windows would not protect you when opening it. Save it to a folder on your computer instead.",
        "Dieses Laufwerk kann die Datei nicht als „aus dem Internet“ markieren (z. B. USB-Stick mit FAT32), Windows würde dich beim Öffnen also nicht schützen. Speichere sie stattdessen in einen Ordner auf deinem Computer.",
    )
    .to_owned()
}

#[allow(clippy::too_many_arguments)]
async fn share_one(
    blobs: &Blobs,
    db: &Db,
    sync: &SyncService,
    st: &Arc<Mutex<State>>,
    space: SpaceId,
    channel: ChannelId,
    path: &Path,
    name: &str,
) -> Result<Hash, String> {
    let meta = std::fs::metadata(path)
        .map_err(|e| app_core::tr!("not readable ({e})", "nicht lesbar ({e})"))?;
    if !meta.is_file() {
        return Err(app_core::i18n::tr("this is not a file", "das ist keine Datei").to_owned());
    }
    if meta.len() == 0 {
        return Err(app_core::i18n::tr("the file is empty", "die Datei ist leer").to_owned());
    }
    if meta.len() > MAX_FILE_BYTES {
        return Err(app_core::i18n::tr(
            "the file is larger than 4 GB",
            "die Datei ist größer als 4 GB",
        )
        .to_owned());
    }
    let (hash, size) = blobs.import(path).await.map_err(|e| e.to_string())?;
    st.locked().local.insert(hash);
    let mime = guess_mime(name);
    let thumb_hash = match make_thumbnail(path, mime, meta.len()).await {
        Some(bytes) => blobs.import_bytes(bytes).await.ok(),
        None => None,
    };
    // Selbst geteilt: darf an die Mitglieder dieses Spaces gehen (H-1).
    for h in std::iter::once(hash).chain(thumb_hash) {
        let _ = db.spawn(
            move |c| app_core::eventlog::chat::add_blob_origin(c, &h, &space),
            |_| {},
        );
    }
    let kind = EventKind::FileShare {
        channel,
        blob_hash: hash,
        name: name.to_owned(),
        size,
        mime: mime.to_owned(),
        thumb_hash,
    };
    sync.publish(Some(space), kind)
        .await
        .map(|_| hash)
        .map_err(|e| e.to_string())
}

fn persist_pins(db: &Db, st: &Arc<Mutex<State>>) {
    let pins: Vec<Hash> = st.locked().pinned.iter().copied().collect();
    let _ = db.spawn(
        move |c| app_core::settings::set(c, app_core::settings::FILES_PINNED, &pins),
        |_| {},
    );
}

/// Bilder, die wir fuer die Vorschau einlesen, hoechstens so gross (Arbeitsspeicher).
const MAX_THUMB_SOURCE: u64 = 100 * 1024 * 1024;
/// Groesstes Bild fuer die Grossansicht in der App.
const MAX_VIEW_BYTES: u64 = 50 * 1024 * 1024;
/// Vorschaubilder sind hoechstens 100 KB (Absender); mehr wird nie geladen.
const MAX_THUMB_BYTES: u64 = 256 * 1024;
/// Vorschaubilder sind 320 px gross; groessere werden nicht angezeigt.
const THUMB_MAX_EDGE: u32 = 512;
/// So viele Vorschaubilder bleiben dekodiert im Speicher (je hoechstens 1 MB).
const MAX_THUMBS: usize = 300;
/// Grossansicht: groesste Kante beim Dekodieren und beim Anzeigen (Texturgrenze der GPU).
const VIEW_MAX_EDGE: u32 = 16_384;
const VIEW_FIT_EDGE: u32 = 4096;

/// Bild aus fremden Bytes dekodieren (mit Grenzen gegen Dekompressionsbomben) und als fertige
/// Pixel uebergeben; iced dekodiert dann nichts mehr selbst.
fn decode_limited(
    bytes: &[u8],
    max_edge: u32,
    max_alloc: u64,
    fit_edge: u32,
) -> Option<iced::widget::image::Handle> {
    let (w, h, px) = app_video::thumb::decode_rgba_limited(bytes, max_edge, max_alloc, fit_edge)?;
    Some(iced::widget::image::Handle::from_rgba(w, h, px))
}

/// Erzeugt das Vorschaubild (JPEG, hoechstens 100 KB) fuer Bilder und Videos.
async fn make_thumbnail(path: &Path, mime: &str, len: u64) -> Option<Vec<u8>> {
    let path = path.to_path_buf();
    if mime.starts_with("image/") && mime != "image/svg+xml" && len <= MAX_THUMB_SOURCE {
        return tokio::task::spawn_blocking(move || app_video::thumb::image_thumbnail(&path))
            .await
            .ok()
            .flatten();
    }
    #[cfg(feature = "video")]
    if mime.starts_with("video/") {
        return tokio::task::spawn_blocking(move || app_video::thumb::video_thumbnail(&path))
            .await
            .ok()
            .flatten();
    }
    None
}

/// Legt `dir/name` (bei Kollision `dir/name (2).ext` usw.) leer an und gibt den Pfad zurueck.
/// `create_new` belegt den Namen atomar: zwei gleichzeitige Exporte koennen sich nicht
/// gegenseitig (oder eine fremde Datei) ueberschreiben.
fn reserve_unique(dir: &Path, name: &str) -> std::io::Result<PathBuf> {
    let (stem, ext) = match name.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() => (s.to_owned(), format!(".{e}")),
        _ => (name.to_owned(), String::new()),
    };
    for n in 1..10_000u32 {
        let p = if n == 1 {
            dir.join(name)
        } else {
            dir.join(format!("{stem} ({n}){ext}"))
        };
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&p)
        {
            Ok(_) => return Ok(p),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        app_core::i18n::tr("no free file name", "kein freier Dateiname"),
    ))
}

/// Markiert eine exportierte Datei als "aus dem Internet" (Mark-of-the-Web, Zone 3) wie ein
/// Browser-Download: SmartScreen prueft Programme, Office oeffnet in der geschuetzten Ansicht,
/// und beim Entpacken erben die Dateien die Markierung. `false` auf Laufwerken ohne
/// NTFS-Datenstroeme (z. B. FAT32-Sticks); dann verweigert der Aufrufer riskante Dateien.
fn mark_of_the_web(path: &Path) -> bool {
    let mut ads = path.as_os_str().to_owned();
    ads.push(":Zone.Identifier");
    match std::fs::write(&ads, b"[ZoneTransfer]\r\nZoneId=3\r\n") {
        Ok(()) => true,
        Err(e) => {
            tracing::warn!("Mark-of-the-Web nicht gesetzt ({}): {e}", path.display());
            false
        }
    }
}

/// Ordner fuer Dateien, die per "Oeffnen" an ein anderes Programm gehen.
pub fn opened_dir() -> PathBuf {
    std::env::temp_dir().join("Campfire").join("geoeffnet")
}

/// Entfernt geoeffnete Kopien, die aelter als `max_age` sind (auch in den Unterordnern je
/// Anhang; leere Unterordner verschwinden mit). Sonst bleiben Anhaenge aus allen Spaces im
/// Temp-Ordner liegen. Dateien, die gerade offen sind, laesst Windows nicht loeschen; sie
/// kommen beim naechsten Aufraeumen dran.
pub fn clean_opened(dir: &Path, max_age: std::time::Duration) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut removed = 0;
    for e in entries.flatten() {
        let Ok(t) = e.file_type() else {
            continue;
        };
        if t.is_dir() {
            removed += clean_opened(&e.path(), max_age);
            let _ = std::fs::remove_dir(e.path()); // nur wenn leer
            continue;
        }
        let old = e
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age >= max_age);
        if old && t.is_file() && std::fs::remove_file(e.path()).is_ok() {
            removed += 1;
        }
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colliding_names_get_a_counter_before_the_extension() {
        let d = std::env::temp_dir().join(format!("campfire-unique-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        assert_eq!(reserve_unique(&d, "a.txt").unwrap(), d.join("a.txt"));
        assert_eq!(reserve_unique(&d, "a.txt").unwrap(), d.join("a (2).txt"));
        assert_eq!(reserve_unique(&d, "a.txt").unwrap(), d.join("a (3).txt"));
        std::fs::write(d.join("LICENSE"), b"x").unwrap();
        assert_eq!(
            reserve_unique(&d, "LICENSE").unwrap(),
            d.join("LICENSE (2)")
        );
        // Die vorhandene Datei bleibt unangetastet.
        assert_eq!(std::fs::read(d.join("LICENSE")).unwrap(), b"x");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn exported_files_carry_the_mark_of_the_web() {
        let d = std::env::temp_dir().join(format!("campfire-motw-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let f = d.join("rechnung.pdf");
        std::fs::write(&f, b"%PDF").unwrap();
        let marked = mark_of_the_web(&f);
        let mut ads = f.as_os_str().to_owned();
        ads.push(":Zone.Identifier");
        match std::fs::read_to_string(&ads) {
            Ok(z) => assert!(marked && z.contains("ZoneId=3"), "{z}"),
            // Temp-Ordner ohne NTFS (selten): dann gibt es nichts zu pruefen.
            Err(e) => eprintln!("kein NTFS-Datenstrom: {e}"),
        }
        assert_eq!(std::fs::read(&f).unwrap(), b"%PDF");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn old_opened_copies_are_cleaned_up() {
        let d = std::env::temp_dir().join(format!("campfire-opened-{}", std::process::id()));
        std::fs::create_dir_all(d.join("sub")).unwrap();
        std::fs::write(d.join("a.pdf"), b"x").unwrap();
        std::fs::write(d.join("sub").join("b.pdf"), b"x").unwrap();
        assert_eq!(clean_opened(&d, std::time::Duration::from_secs(3600)), 0);
        assert_eq!(clean_opened(&d, std::time::Duration::ZERO), 2);
        assert!(!d.join("a.pdf").exists());
        assert!(!d.join("sub").exists(), "leere Unterordner gehen mit");
        assert_eq!(clean_opened(&d.join("fehlt"), std::time::Duration::ZERO), 0);
        let _ = std::fs::remove_dir_all(&d);
    }
}
