//! Verdrahtung der Kernbausteine (Speicher, Identitaet, Netz, Freunde, Presence).

use crate::hotkeys::Hotkeys;
use crate::overlay::OverlaySettings;
use crate::signals::{send, Signal};
use crate::sounds::Sounds;
use app_audio::io::{AudioIo, IoConfig};
use app_audio::process::InputSettings;
use app_audio::service::{FriendCheck, MemberCheck, VoiceService};
use app_audio::voice::{VoiceCore, DEFAULT_KBPS};
use app_core::eventlog::sync::{SyncConfig, SyncService};
use app_core::friends::store::FriendStatus;
use app_core::friends::{FriendEvent, FriendsDirectory, FriendsService, ServiceConfig};
use app_core::identity::{Dpapi, Identity};
use app_core::presence::{idle_time, LocalState, PresenceService, DEFAULT_AWAY_AFTER};
use app_core::storage::{Db, Paths};
use app_net::{Net, NetConfig, NetEvent, NetMode, RelayConfig};
use app_proto::control::{Handshake, Presence, PresenceStatus};
use app_proto::lock::Locked;
use app_proto::PublicKey;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Relay-Auswahl: 0 = Standard, 1 = eigene Adresse, 2 = nur Relay, 3 = nur eigenes Relay.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RelaySetting {
    pub mode: u8,
    pub url: String,
}

/// Persoenliche Einstellung fuer einen anderen Nutzer (nur lokal wirksam).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct PeerPref {
    /// 0.0 bis 2.0 (100 % = 1.0)
    pub volume: f32,
    pub muted: bool,
}

impl Default for PeerPref {
    fn default() -> Self {
        Self {
            volume: 1.0,
            muted: false,
        }
    }
}

fn pref_key(user: &PublicKey) -> String {
    data_encoding::HEXLOWER.encode(user)
}

/// Alle Mitgliedschaften im Speicher. Voice und Presence fragen sie im Netz-Verteiler ab;
/// ein blockierender DB-Aufruf dort wuerde hinter einem grossen Sync-Batch haengen und
/// Control-Nachrichten und Voice-Joins aller Freunde aufhalten.
#[derive(Clone, Default)]
struct Members(Arc<std::sync::RwLock<HashSet<(app_proto::event::SpaceId, PublicKey)>>>);

impl Members {
    /// Setzt den neuen Stand; `true`, wenn er sich geaendert hat.
    fn set(&self, set: HashSet<(app_proto::event::SpaceId, PublicKey)>) -> bool {
        let mut cur = self
            .0
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let changed = *cur != set;
        *cur = set;
        changed
    }

    /// Einmal beim Start (blockierend) laden.
    fn load(&self, db: &Db) {
        if let Ok(set) = db.call(|c| app_core::eventlog::all_memberships(c)) {
            self.set(set);
        }
    }

    /// Nach Aenderungen im Log neu laden (ohne einen Tokio-Worker zu blockieren);
    /// `true`, wenn sich etwas geaendert hat.
    async fn reload(&self, db: &Db) -> bool {
        let db = db.clone();
        let r = tokio::task::spawn_blocking(move || {
            db.call(|c| app_core::eventlog::all_memberships(c))
        })
        .await;
        match r {
            Ok(Ok(set)) => self.set(set),
            _ => false,
        }
    }

    fn contains(&self, space: &app_proto::event::SpaceId, user: &PublicKey) -> bool {
        self.0
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains(&(*space, *user))
    }
}

#[derive(Clone)]
pub struct Core {
    pub db: Db,
    pub net: Net,
    pub friends: FriendsService,
    pub presence: PresenceService,
    pub sync: SyncService,
    pub voice: VoiceService,
    pub stream: crate::stream::StreamService,
    /// Dateispeicher (M4). Fehlt, wenn er sich nicht oeffnen liess.
    pub files: crate::files::FileService,
    pub hotkeys: Hotkeys,
    pub sounds: Sounds,
    prefs: Arc<Mutex<HashMap<String, PeerPref>>>,
    ringing: Arc<std::sync::atomic::AtomicBool>,
    overlay: Arc<Mutex<OverlaySettings>>,
    notify: Arc<Mutex<app_core::notify::NotifySettings>>,
    video: Arc<Mutex<app_video::select::StreamOptions>>,
    io_cfg: Arc<Mutex<IoConfig>>,
    bitrate: Arc<Mutex<u32>>,
    relay: Arc<Mutex<RelaySetting>>,
    audio: Arc<Mutex<Option<AudioIo>>>,
    input: Arc<Mutex<InputSettings>>,
    monitor: Arc<std::sync::atomic::AtomicBool>,
    pub local: Arc<LocalState>,
    pub name: String,
    pub user: PublicKey,
    pub my_code: String,
    targets: Arc<Mutex<HashSet<PublicKey>>>,
}

impl std::fmt::Debug for Core {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Core")
    }
}

#[derive(Clone)]
pub enum Boot {
    NeedsSetup(Db),
    Ready(Box<Core>),
}

impl std::fmt::Debug for Boot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Boot")
    }
}

fn err<E: std::fmt::Display>(e: E) -> String {
    e.to_string()
}

pub async fn boot() -> Result<Boot, String> {
    let db = tokio::task::spawn_blocking(|| {
        let paths = Paths::from_env().map_err(err)?;
        Db::open(&paths).map_err(err)
    })
    .await
    .map_err(err)??;
    let d = db.clone();
    let id = tokio::task::spawn_blocking(move || d.call(|c| Ok(Identity::load(c, &Dpapi))))
        .await
        .map_err(err)?
        .map_err(err)?
        .map_err(err)?;
    match id {
        None => Ok(Boot::NeedsSetup(db)),
        Some(id) => Ok(Boot::Ready(Box::new(start(db, id).await?))),
    }
}

/// Speichert eine neue Identitaet und startet den Kern.
pub async fn finish_setup(db: Db, id: Identity) -> Result<Core, String> {
    let d = db.clone();
    let id =
        tokio::task::spawn_blocking(move || d.call(move |c| Ok(id.save(c, &Dpapi).map(|()| id))))
            .await
            .map_err(err)?
            .map_err(err)?
            .map_err(err)?;
    start(db, id).await
}

async fn start(db: Db, id: Identity) -> Result<Core, String> {
    let id = Arc::new(id);
    let dir = Arc::new(FriendsDirectory::default());
    let recovery = db
        .call(|c| Ok(app_core::identity::recovery_commitment(c)))
        .map_err(err)?
        .map_err(err)?;
    let hs = Handshake {
        versions: vec![1],
        cert: id.cert().clone(),
        display_name: id.display_name.clone(),
        avatar_hash: id.avatar_hash,
        recovery,
    };
    let relay: RelaySetting = db
        .call(|c| app_core::settings::get(c, app_core::settings::RELAY_MODE))
        .ok()
        .flatten()
        .unwrap_or_default();
    let mut cfg = NetConfig::new(*id.device_secret_bytes(), hs, dir.clone());
    // Freundschaftsanfragen nur ueber einen eigenen, relay-only Endpoint (M-5).
    cfg.rendezvous = true;
    cfg.mode = NetMode::Internet(match relay.mode {
        m @ (1 | 3) if app_proto::urls::is_public_https(&relay.url) => RelayConfig::Custom {
            urls: vec![relay.url.trim().to_owned()],
            relay_only: m == 3,
        },
        // Eigenes Relay ungueltig: die IP trotzdem nicht preisgeben.
        2 | 3 => RelayConfig::RelayOnly,
        _ => RelayConfig::Default,
    });
    let (net, mut net_rx) = Net::start(cfg).await.map_err(err)?;
    // Ausgeliefert wird nur an Mitglieder des Spaces, in dem die Datei angehaengt ist.
    let blobs = match Paths::from_env() {
        Ok(p) => match app_blobs::Blobs::open(
            &p.blobs_dir,
            &net.endpoint(),
            Arc::new(crate::files::SpaceAuth {
                db: db.clone(),
                net: net.clone(),
                me: id.user_pubkey(),
            }),
        )
        .await
        {
            Ok(b) => Some(Arc::new(b)),
            Err(e) => {
                tracing::warn!("Dateispeicher nicht verfuegbar: {e}");
                None
            }
        },
        Err(_) => None,
    };

    let (friends, mut friend_events) = FriendsService::start(
        db.clone(),
        net.clone(),
        dir.clone(),
        id.cert().clone(),
        id.display_name.clone(),
        id.invite_key(),
        ServiceConfig::default(),
    )
    .await
    .map_err(err)?;
    let initial = Presence {
        status: PresenceStatus::Online,
        voice: None,
        streaming: false,
    };
    let members = Members::default();
    members.load(&db);
    let is_member: MemberCheck = {
        let members = members.clone();
        Arc::new(move |space, user| members.contains(space, user))
    };
    let (presence, mut presence_events) =
        PresenceService::new(net.clone(), initial.clone(), is_member.clone());
    let local = Arc::new(LocalState::new(
        presence.clone(),
        initial,
        DEFAULT_AWAY_AFTER,
    ));
    local.spawn_auto_away(Duration::from_secs(30), idle_time);

    let signer_id = id.clone();
    let (sync, mut log_events) = SyncService::start(
        db.clone(),
        net.clone(),
        id.user_pubkey(),
        Arc::new(move |m| signer_id.sign_user(m)),
        SyncConfig::default(),
    )
    .await
    .map_err(err)?;

    let input: InputSettings = db
        .call(|c| app_core::settings::get(c, app_core::settings::AUDIO_INPUT))
        .ok()
        .flatten()
        .unwrap_or_default();
    let prefs: HashMap<String, PeerPref> = db
        .call(|c| app_core::settings::get(c, app_core::settings::PEER_PREFS))
        .ok()
        .flatten()
        .unwrap_or_default();
    let video: app_video::select::StreamOptions = db
        .call(|c| app_core::settings::get(c, app_core::settings::VIDEO))
        .ok()
        .flatten()
        .unwrap_or_default();
    let notify = db
        .call(|c| Ok(app_core::notify::load(c)))
        .unwrap_or_default();
    let overlay: OverlaySettings = db
        .call(|c| app_core::settings::get(c, app_core::settings::OVERLAY))
        .ok()
        .flatten()
        .unwrap_or_default();
    let io_cfg: IoConfig = db
        .call(|c| app_core::settings::get(c, app_core::settings::AUDIO_DEVICES))
        .ok()
        .flatten()
        .unwrap_or_default();
    let bitrate: u32 = db
        .call(|c| app_core::settings::get(c, app_core::settings::AUDIO_BITRATE))
        .ok()
        .flatten()
        .unwrap_or(DEFAULT_KBPS);
    let voice_core = Arc::new(Mutex::new(VoiceCore::new(bitrate).map_err(err)?));
    let sounds = Sounds::load(&db, voice_core.clone());
    let hotkeys = Hotkeys::start(&db, voice_core.clone(), sounds.clone());
    // Anrufen duerfen nur angenommene Freunde (Freundes-Directory).
    let is_friend: FriendCheck = {
        let dir = dir.clone();
        Arc::new(move |u| {
            matches!(
                app_net::PeerDirectory::check(&*dir, u),
                app_net::Access::Allowed
            )
        })
    };
    let (voice, mut voice_events) =
        VoiceService::new(net.clone(), voice_core, is_member, is_friend);
    {
        let v = voice.clone();
        hotkeys.watch_voice(Arc::new(move || {
            v.current().is_some() || v.call_state() != app_audio::service::CallState::Idle
        }));
    }

    let stream = crate::stream::StreamService::new(
        net.clone(),
        voice.clone(),
        Arc::new(|| send(Signal::StreamChanged)),
    );
    tokio::task::spawn_blocking(|| {
        // Kopien aus der letzten Sitzung braucht die App nicht mehr (I-5).
        let n = crate::files::clean_opened(&crate::files::opened_dir(), Duration::ZERO);
        if n > 0 {
            tracing::info!("{n} alte geoeffnete Dateien aus dem Temp-Ordner entfernt");
        }
    });
    let files = crate::files::FileService::new(
        blobs.clone(),
        sync.clone(),
        net.clone(),
        db.clone(),
        id.user_pubkey(),
    );
    let core = Core {
        db,
        net,
        friends: friends.clone(),
        presence: presence.clone(),
        sync: sync.clone(),
        voice: voice.clone(),
        stream: stream.clone(),
        files,
        hotkeys,
        sounds: sounds.clone(),
        prefs: Arc::new(Mutex::new(prefs)),
        ringing: Arc::default(),
        overlay: Arc::new(Mutex::new(overlay)),
        notify: Arc::new(Mutex::new(notify)),
        video: Arc::new(Mutex::new(video.sanitized())),
        io_cfg: Arc::new(Mutex::new(io_cfg)),
        bitrate: Arc::new(Mutex::new(bitrate)),
        relay: Arc::new(Mutex::new(relay)),
        audio: Arc::default(),
        input: Arc::new(Mutex::new(input)),
        monitor: Arc::default(),
        local,
        name: id.display_name.clone(),
        user: id.user_pubkey(),
        my_code: friends.my_code(None).await.map_err(err)?,
        targets: Arc::default(),
    };

    // Netzwerk-Ereignisse verteilen.
    {
        let friends = friends.clone();
        let presence = presence.clone();
        let sync = sync.clone();
        let voice = voice.clone();
        let blobs = blobs.clone();
        let files = core.files.clone();
        tokio::spawn(async move {
            while let Some(ev) = net_rx.recv().await {
                voice.on_net_event(&ev);
                stream.on_net_event(&ev);
                match ev {
                    NetEvent::VideoStream { from, recv } => stream.on_video_stream(from, recv),
                    NetEvent::Hello(conn) => friends.spawn_hello_handler(conn),
                    NetEvent::Blobs(conn) => {
                        if let Some(b) = &blobs {
                            b.accept(conn);
                        }
                    }
                    NetEvent::SyncStream { from, send, recv } => {
                        sync.on_sync_stream(from, send, recv);
                    }
                    NetEvent::PeerUp(ref p) => {
                        if let Some(c) = p.recovery {
                            let (friends, user) = (friends.clone(), p.user);
                            tokio::spawn(async move {
                                let _ = friends.note_recovery(user, c).await;
                            });
                        }
                        sync.on_peer_up(p.clone());
                        files.retry_wanted();
                        friends.kick();
                        presence.on_net_event(&ev);
                    }
                    other => {
                        presence.on_net_event(&other);
                    }
                }
            }
        });
    }
    // Freundes- und Presence-Ereignisse wecken die UI.
    {
        let core = core.clone();
        tokio::spawn(async move {
            while let Some(ev) = friend_events.recv().await {
                if matches!(
                    ev,
                    FriendEvent::Accepted { .. } | FriendEvent::Rotated { .. }
                ) {
                    core.sync_targets().await;
                }
                send(Signal::FriendsChanged(matches!(
                    ev,
                    FriendEvent::RequestReceived { .. }
                )));
            }
        });
    }
    let files = core.files.clone();
    // Vom Netz ausgeloest: hinter den eigenen DB-Jobs.
    let member_db = core.db.background();
    let member_presence = presence.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(60));
        loop {
            tokio::select! {
                ev = log_events.recv() => {
                    let Some(ev) = ev else { break };
                    // Nur der Zustellstatus hat sich geaendert: Ansicht auffrischen, sonst nichts.
                    if matches!(ev, app_core::eventlog::sync::LogEvent::DeliveryChanged(_)) {
                        send(Signal::SpacesChanged);
                        continue;
                    }
                    // Neue Mitglieder erfahren den eigenen Voice-Channel sofort,
                    // entfernte nicht mehr.
                    if members.reload(&member_db).await
                        && member_presence.local().voice.is_some()
                    {
                        member_presence.resend();
                    }
                    send(Signal::SpacesChanged);
                    files.auto_fetch(false);
                }
                // Alle Minute noch einmal versuchen, falls inzwischen jemand online ist.
                _ = tick.tick() => {
                    files.auto_fetch(true);
                    files.retry_wanted();
                }
            }
        }
    });
    let pref_core = core.clone();
    tokio::spawn(async move {
        let (mut was_in, mut was_n) = (false, 0usize);
        while voice_events.recv().await.is_some() {
            pref_core.apply_prefs();
            pref_core.sync_audio();
            pref_core.ring_if_needed();
            // Join-/Leave-Toene: eigener Beitritt/Austritt und Kommen/Gehen anderer.
            let now_in = pref_core.voice.current().is_some();
            let n = pref_core.voice.participants().len();
            if now_in && (!was_in || n > was_n) {
                sounds.play(app_audio::sfx::Sound::Join);
            } else if (!now_in && was_in) || (now_in && n < was_n) {
                sounds.play(app_audio::sfx::Sound::Leave);
            }
            (was_in, was_n) = (now_in, n);
            send(Signal::VoiceChanged);
        }
    });
    tokio::spawn(async move {
        while presence_events.recv().await.is_some() {
            send(Signal::PresenceChanged);
        }
    });
    core.sync_targets().await;
    Ok(core)
}

impl Core {
    /// Tritt einem Voice-Channel bei und startet Mikrofon und Lautsprecher.
    pub fn join_voice(
        &self,
        space: app_proto::event::SpaceId,
        channel: app_proto::event::ChannelId,
    ) {
        self.voice.join(space, channel);
        self.sync_audio();
    }

    /// Verlaesst den Voice-Channel; die Audiogeraete werden freigegeben (ausser im Mikrofon-Test).
    pub fn leave_voice(&self) {
        self.voice.leave();
        self.sync_audio();
    }

    /// Mit Push-to-Talk entscheidet die Taste; Sprachaktivierung waere doppelt gemoppelt.
    fn effective_input(&self) -> InputSettings {
        let mut s = self.input.locked().clone();
        if self.hotkeys.settings().ptt_enabled {
            s.voice_activation = false;
        }
        s
    }

    /// Nach einer Aenderung der Hotkey-Einstellungen aufrufen.
    pub fn reapply_input(&self) {
        if let Some(a) = self.audio.locked().as_ref() {
            a.set_input(self.effective_input());
        }
    }

    /// Mikrofon/Lautsprecher laufen genau dann, wenn wir in einem Channel oder Gespraech sind
    /// (oder der Mikrofon-Test an ist).
    pub fn sync_audio(&self) {
        use app_audio::service::CallState;
        let capture = self.voice.current().is_some() || self.monitoring();
        let ringing = matches!(self.voice.call_state(), CallState::Incoming { .. });
        let mut a = self.audio.locked();
        if capture || ringing {
            match a.as_ref() {
                None => {
                    let v = self.voice.clone();
                    *a = Some(AudioIo::start(
                        self.voice.core(),
                        Arc::new(move |p| v.send_packet(p)),
                        self.io_config(),
                        self.effective_input(),
                        capture,
                    ));
                }
                Some(io) => io.set_capture(capture),
            }
        } else if let Some(io) = a.as_ref() {
            // Mikrofon sofort frei geben; die Wiedergabe laeuft, bis ausstehende Toene gespielt sind.
            io.set_capture(false);
            if self.voice.core().locked().sfx_pending() {
                let me = self.clone();
                std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_millis(300));
                    me.sync_audio();
                });
            } else {
                let io = a.take();
                drop(io);
            }
        }
    }

    /// Fuer den Testknopf der Toene: Ausgabe kurz oeffnen, falls sie nicht laeuft.
    pub fn sync_audio_for_test(&self) {
        {
            let mut a = self.audio.locked();
            if a.is_none() {
                let v = self.voice.clone();
                *a = Some(AudioIo::start(
                    self.voice.core(),
                    Arc::new(move |p| v.send_packet(p)),
                    self.io_config(),
                    self.effective_input(),
                    false,
                ));
            }
        }
        // Wird wieder geschlossen, sobald der Ton gespielt ist.
        self.sync_audio();
    }

    /// Eingehender Anruf: Aufmerksamkeit anfordern (Taskleiste/Toast, kein Fokus-Diebstahl)
    /// und klingeln, bis er beantwortet oder beendet ist. Bei "Bitte nicht stoeren" klingelt
    /// es nicht; der Anruf erscheint nur still in der App.
    pub fn ring_if_needed(&self) {
        use app_audio::service::CallState;
        let CallState::Incoming { user } = self.voice.call_state() else {
            return;
        };
        if self.presence.local().status == PresenceStatus::DoNotDisturb {
            return;
        }
        if self
            .ringing
            .swap(true, std::sync::atomic::Ordering::Relaxed)
        {
            return;
        }
        send(Signal::IncomingCall(user));
        let (voice, ringing, sounds) = (
            self.voice.clone(),
            self.ringing.clone(),
            self.sounds.clone(),
        );
        std::thread::spawn(move || {
            while matches!(voice.call_state(), CallState::Incoming { .. }) {
                sounds.play(app_audio::sfx::Sound::Ring);
                std::thread::sleep(Duration::from_millis(1400));
            }
            ringing.store(false, std::sync::atomic::Ordering::Relaxed);
        });
    }

    pub fn relay(&self) -> RelaySetting {
        self.relay.locked().clone()
    }

    /// Wirkt nach einem Neustart der App.
    pub fn set_relay(&self, r: RelaySetting) {
        *self.relay.locked() = r.clone();
        let _ = self.db.spawn(
            move |c| app_core::settings::set(c, app_core::settings::RELAY_MODE, &r),
            |_| {},
        );
    }

    pub fn io_config(&self) -> IoConfig {
        self.io_cfg.locked().clone()
    }

    pub fn set_io_config(&self, c: IoConfig) {
        *self.io_cfg.locked() = c.clone();
        if let Some(a) = self.audio.locked().as_ref() {
            a.set_devices(c.clone());
        }
        let _ = self.db.spawn(
            move |conn| app_core::settings::set(conn, app_core::settings::AUDIO_DEVICES, &c),
            |_| {},
        );
    }

    pub fn bitrate(&self) -> u32 {
        *self.bitrate.locked()
    }

    pub fn set_bitrate(&self, kbps: u32) {
        let k = kbps.clamp(app_audio::voice::MIN_KBPS, app_audio::voice::MAX_KBPS);
        *self.bitrate.locked() = k;
        let _ = self.voice.core().locked().set_bitrate(k);
        let _ = self.db.spawn(
            move |c| app_core::settings::set(c, app_core::settings::AUDIO_BITRATE, &k),
            |_| {},
        );
    }

    pub fn video(&self) -> app_video::select::StreamOptions {
        *self.video.locked()
    }

    pub fn set_video(&self, s: app_video::select::StreamOptions) {
        let s = s.sanitized();
        *self.video.locked() = s;
        let _ = self.db.spawn(
            move |c| app_core::settings::set(c, app_core::settings::VIDEO, &s),
            |_| {},
        );
    }

    pub fn notify(&self) -> app_core::notify::NotifySettings {
        self.notify.locked().clone()
    }

    pub fn set_notify(&self, s: app_core::notify::NotifySettings) {
        *self.notify.locked() = s.clone();
        let _ = self
            .db
            .spawn(move |c| app_core::notify::save(c, &s), |_| {});
    }

    pub fn overlay(&self) -> OverlaySettings {
        self.overlay.locked().clone()
    }

    pub fn set_overlay(&self, s: OverlaySettings) {
        *self.overlay.locked() = s.clone();
        let _ = self.db.spawn(
            move |c| app_core::settings::set(c, app_core::settings::OVERLAY, &s),
            |_| {},
        );
    }

    pub fn peer_pref(&self, user: &PublicKey) -> PeerPref {
        self.prefs
            .locked()
            .get(&pref_key(user))
            .copied()
            .unwrap_or_default()
    }

    /// Lautstaerke/Stummschaltung fuer einen Nutzer aendern, anwenden und speichern.
    pub fn set_peer_pref(&self, user: &PublicKey, pref: PeerPref) {
        let all = {
            let mut p = self.prefs.locked();
            if pref == PeerPref::default() {
                p.remove(&pref_key(user));
            } else {
                p.insert(pref_key(user), pref);
            }
            p.clone()
        };
        let _ = self.db.spawn(
            move |c| app_core::settings::set(c, app_core::settings::PEER_PREFS, &all),
            |_| {},
        );
        self.apply_prefs();
    }

    /// Gespeicherte Einstellungen auf die aktuellen Teilnehmer anwenden.
    pub fn apply_prefs(&self) {
        let parts = self.voice.participants();
        let core = self.voice.core();
        let mut c = core.locked();
        for p in parts {
            let pref = self.peer_pref(&p.user);
            c.set_peer_volume(&p.device, if pref.muted { 0.0 } else { pref.volume });
        }
    }

    pub fn input(&self) -> InputSettings {
        self.input.locked().clone()
    }

    /// Eingangseinstellungen setzen, sofort anwenden und speichern.
    pub fn set_input(&self, s: InputSettings) {
        *self.input.locked() = s.clone();
        self.reapply_input();
        let _ = self.db.spawn(
            move |c| app_core::settings::set(c, app_core::settings::AUDIO_INPUT, &s),
            |_| {},
        );
    }

    pub fn monitoring(&self) -> bool {
        self.monitor.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Mikrofon-Test "Mich selbst hoeren" (auch ausserhalb eines Channels).
    pub fn set_monitor(&self, on: bool) {
        self.monitor.store(on, std::sync::atomic::Ordering::Relaxed);
        self.voice.core().locked().set_monitor(on);
        self.sync_audio();
    }

    /// (Pegel, sendet gerade) fuer die Pegelanzeige; (0, false) ohne laufende Audiogeraete.
    pub fn input_level(&self) -> (f32, bool) {
        self.audio
            .locked()
            .as_ref()
            .map_or((0.0, false), AudioIo::input_level)
    }

    /// Gleicht die Reconnect-Ziele mit den Geraeten angenommener Freunde ab: neue Geraete
    /// werden dauerhaft verbunden, Geraete blockierter oder entfernter Freunde aufgegeben
    /// (sonst wuerde die App sie endlos neu anwaehlen und ihnen dabei die eigene IP zeigen).
    pub async fn sync_targets(&self) {
        let Ok(list) = self.friends.list().await else {
            return;
        };
        let want: HashSet<PublicKey> = list
            .iter()
            .filter(|f| f.status == FriendStatus::Accepted)
            .flat_map(|f| f.node_ids())
            .collect();
        let (added, removed) = {
            let mut t = self.targets.locked();
            let added: Vec<PublicKey> = want.difference(&t).copied().collect();
            let removed: Vec<PublicKey> = t.difference(&want).copied().collect();
            *t = want;
            (added, removed)
        };
        for node in removed {
            self.net.remove_target(&node);
        }
        for node in added {
            if let Ok(id) = iroh::EndpointId::from_bytes(&node) {
                self.net.add_target(iroh::EndpointAddr::new(id));
            }
        }
    }
}
