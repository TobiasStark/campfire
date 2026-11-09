//! iced-Oberflaeche: Setup, Hauptlayout, Freunde.

use crate::core::{self, Boot, Core};
use crate::overlay;
use crate::platform;
use crate::settings_ui;
use crate::signals::{self, Signal};
use crate::spaces;
use crate::ui::{self, btn, size, space, Tone};
use app_audio::service::CallState;
use app_core::friends::{Friend, FriendStatus};
use app_core::i18n::tr;
use app_core::identity::{
    backup_done, mark_backup_done, write_backup_file, Identity, BACKUP_FILE_NAME,
    MIN_PASSPHRASE_LEN,
};
use app_core::settings::{self, ColorOverrides, ThemeChoice};
use app_core::storage::Db;
use app_proto::control::PresenceStatus;
use app_proto::PublicKey;
use iced::widget::{button, column, container, row, text, tooltip, Space};
use iced::{window, Alignment, Element, Length, Subscription, Task, Theme};
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub enum Msg {
    Booted(Result<Boot, String>),
    Sig(Signal),
    Opened(window::Id),
    Closed(window::Id),
    Tick,
    Focus(bool),
    OpenSettings,
    Settings(settings_ui::Msg),
    OverlayOpened(window::Id),
    OverlayHandle(u64),
    OverlayScreen(Option<iced::Size>),
    Call(PublicKey),
    CallAccept,
    CallDecline,
    CallCancel,
    CallHangup,
    // Setup
    Name(String),
    Pass(String),
    Create,
    ImportPath(String),
    ImportPass(String),
    ToggleImport,
    Import,
    SetupDone(Result<Box<Core>, String>),
    // Hauptansicht
    Friends(Vec<Friend>),
    CodeInput(String),
    NoteInput(String),
    AddFriend,
    AddResult(Result<(), String>),
    Accept(PublicKey),
    Decline(PublicKey),
    Block(PublicKey),
    Remove(PublicKey),
    /// Entfernen eines Freundes erst nach Rueckfrage.
    AskRemove(Option<PublicKey>),
    /// Einstellungen beim Konto (Sicherung) oeffnen.
    OpenBackup,
    Done(Result<(), String>),
    CopyCode,
    ToggleTheme,
    SetLanguage(app_core::i18n::Language),
    LanguageLoaded(Option<app_core::i18n::Language>),
    ToggleAutostart,
    SetStatus(PresenceStatus),
    BackupHint(bool),
    ColorsLoaded(ColorOverrides),
    Spaces(spaces::Msg),
    Stream(crate::stream_ui::Msg),
    /// Datei auf das Fenster gezogen.
    FileDropped(std::path::PathBuf),
}

#[derive(Default)]
struct Setup {
    db: Option<Db>,
    name: String,
    pass: String,
    imp_path: String,
    imp_pass: String,
    /// Import-Bereich ausgeklappt.
    show_import: bool,
    busy: bool,
    /// Warnung fuer "ohne Backup" wurde schon gezeigt; der zweite Klick faehrt fort.
    skip_warned: bool,
    /// Hinweis "es liegt schon ein Backup in Dokumente" wurde gezeigt; der zweite Klick legt
    /// trotzdem ein neues Konto an (das alte Backup bleibt erhalten).
    existing_warned: bool,
    error: Option<String>,
}

enum Phase {
    Loading,
    Setup(Setup),
    Main(Box<Core>),
    Failed(String),
}

pub struct App {
    phase: Phase,
    win: Option<window::Id>,
    background: bool,
    theme: ThemeChoice,
    /// Eigene Farben des Nutzers (gespeichert unter `settings::COLORS`).
    colors: ColorOverrides,
    autostart: bool,
    start: Instant,
    friends: Vec<Friend>,
    code_input: String,
    note_input: String,
    info: Option<String>,
    /// Fehler beim letzten Anrufversuch (z. B. offline); steht in der Anrufleiste.
    call_error: Option<String>,
    confirm_remove: Option<PublicKey>,
    backup_hint: bool,
    pending_requests: bool,
    mem: f64,
    spaces: spaces::State,
    settings_open: bool,
    settings: settings_ui::State,
    overlay: Option<window::Id>,
    overlay_opening: bool,
    overlay_geom: Option<(iced::Size, iced::Point)>,
    screen: Option<iced::Size>,
    stream: crate::stream_ui::State,
}

fn err<E: std::fmt::Display>(e: E) -> String {
    e.to_string()
}

fn trace(what: &str, start: Instant) {
    if std::env::var_os("CAMPFIRE_TRACE").is_some() {
        tracing::info!("[{:>6} ms] {what}", start.elapsed().as_millis());
    }
}

impl App {
    pub fn boot(start: Instant, background: bool) -> (Self, Task<Msg>) {
        let mut tasks = vec![Task::perform(core::boot(), Msg::Booted)];
        if !background {
            tasks.push(Self::open_window());
        }
        (
            Self {
                phase: Phase::Loading,
                win: None,
                background,
                theme: ThemeChoice::Dark,
                colors: ColorOverrides::default(),
                autostart: platform::autostart_enabled(),
                start,
                friends: Vec::new(),
                code_input: String::new(),
                note_input: String::new(),
                info: None,
                call_error: None,
                confirm_remove: None,
                backup_hint: false,
                pending_requests: false,
                mem: 0.0,
                spaces: spaces::State::default(),
                settings_open: false,
                settings: settings_ui::State::default(),
                overlay: None,
                overlay_opening: false,
                overlay_geom: None,
                screen: None,
                stream: Default::default(),
            },
            Task::batch(tasks),
        )
    }

    fn open_window() -> Task<Msg> {
        let (_, t) = window::open(window::Settings {
            size: iced::Size::new(960.0, 620.0),
            min_size: Some(iced::Size::new(640.0, 420.0)),
            ..Default::default()
        });
        t.map(Msg::Opened)
    }

    fn core(&self) -> Option<&Core> {
        match &self.phase {
            Phase::Main(c) => Some(c),
            _ => None,
        }
    }

    fn reload_friends(&self) -> Task<Msg> {
        let Some(c) = self.core() else {
            return Task::none();
        };
        let f = c.friends.clone();
        Task::perform(
            async move { f.list().await.unwrap_or_default() },
            Msg::Friends,
        )
    }

    fn entered_main(&mut self, c: Box<Core>) -> Task<Msg> {
        let db = c.db.clone();
        let sp = self.spaces.reload(&c).map(Msg::Spaces);
        self.phase = Phase::Main(c);
        trace("Kern bereit", self.start);
        let load = Task::perform(
            async move {
                tokio::task::spawn_blocking(move || {
                    db.call(|c| {
                        let theme = settings::get::<ThemeChoice>(c, settings::THEME)?;
                        let done = backup_done(c).unwrap_or(true);
                        Ok((theme, done))
                    })
                    .unwrap_or((None, true))
                })
                .await
                .unwrap_or((None, true))
            },
            |(t, done)| {
                if let Some(t) = t {
                    Msg::Sig(Signal::Theme(t))
                } else {
                    Msg::BackupHint(!done)
                }
            },
        );
        let db = self.core().map(|c| c.db.clone());
        let colors = Task::perform(
            async move {
                let db = db?;
                tokio::task::spawn_blocking(move || {
                    db.call(|c| settings::get::<ColorOverrides>(c, settings::COLORS))
                        .ok()
                        .flatten()
                })
                .await
                .ok()
                .flatten()
            },
            |c| Msg::ColorsLoaded(c.unwrap_or_default()),
        );
        let db = self.core().map(|c| c.db.clone());
        let lang = Task::perform(
            async move {
                let db = db?;
                tokio::task::spawn_blocking(move || {
                    db.call(|c| settings::get::<app_core::i18n::Language>(c, settings::LANGUAGE))
                        .ok()
                        .flatten()
                })
                .await
                .ok()
                .flatten()
            },
            Msg::LanguageLoaded,
        );
        Task::batch([self.reload_friends(), load, sp, colors, lang])
    }

    fn save_setting<T: serde::Serialize + Send + 'static>(&self, key: &'static str, v: T) {
        if let Some(c) = self.core() {
            let _ = c.db.spawn(move |conn| settings::set(conn, key, &v), |_| {});
        }
    }

    pub fn update(&mut self, msg: Msg) -> Task<Msg> {
        let t = self.update_inner(msg);
        Task::batch([t, self.sync_overlay()])
    }

    /// Overlay-Fenster oeffnen/schliessen/anpassen, je nach Einstellung und Voice-Zustand.
    fn sync_overlay(&mut self) -> Task<Msg> {
        let Phase::Main(c) = &self.phase else {
            return Task::none();
        };
        let s = c.overlay();
        let want = s.enabled && c.voice.current().is_some();
        match (want, self.overlay) {
            (true, None) if !self.overlay_opening => {
                self.overlay_opening = true;
                let screen = self.screen.unwrap_or(iced::Size::new(1920.0, 1080.0));
                let (size, pos) = overlay::geometry(&s, 1, screen);
                let (_, open) = window::open(window::Settings {
                    size,
                    position: window::Position::Specific(pos),
                    decorations: false,
                    transparent: true,
                    resizable: false,
                    level: window::Level::AlwaysOnTop,
                    ..Default::default()
                });
                open.map(Msg::OverlayOpened)
            }
            (false, Some(id)) => {
                self.overlay = None;
                self.overlay_geom = None;
                window::close(id)
            }
            (true, Some(id)) => {
                let Some(screen) = self.screen else {
                    return Task::none();
                };
                let n = overlay::rows(c, &self.friends, &s).len();
                let geom = overlay::geometry(&s, n, screen);
                if self.overlay_geom == Some(geom) {
                    return Task::none();
                }
                self.overlay_geom = Some(geom);
                Task::batch([window::resize(id, geom.0), window::move_to(id, geom.1)])
            }
            _ => Task::none(),
        }
    }

    fn update_stream(&mut self, m: crate::stream_ui::Msg) -> Task<Msg> {
        use crate::stream_ui::Msg as S;
        let Phase::Main(c) = &self.phase else {
            return Task::none();
        };
        match m {
            S::Watch(d) => {
                c.stream.watch(d);
                self.stream.show = true;
            }
            S::StopWatching => {
                c.stream.unwatch();
                self.stream.show = false;
                return self.leave_fullscreen_and_popout();
            }
            S::ShowStream => self.stream.show = true,
            S::Volume(v) => c.stream.set_volume(v),
            S::ToggleStats => self.stream.stats = !self.stream.stats,
            S::Fullscreen(on) => {
                self.stream.fullscreen = on;
                if let Some(w) = self.win {
                    return window::set_mode(
                        w,
                        if on {
                            window::Mode::Fullscreen
                        } else {
                            window::Mode::Windowed
                        },
                    );
                }
            }
            S::Popout => {
                if self.stream.popout.is_none() && !self.stream.popout_opening {
                    self.stream.popout_opening = true;
                    let (_, open) = window::open(window::Settings {
                        size: iced::Size::new(1280.0, 760.0),
                        ..Default::default()
                    });
                    return open.map(|id| Msg::Stream(S::PopoutOpened(id)));
                }
            }
            S::PopoutOpened(id) => {
                self.stream.popout_opening = false;
                self.stream.popout = Some(id);
                if self.stream.fullscreen {
                    return self.update_stream(S::Fullscreen(false));
                }
            }
            S::ClosePopout => {
                if let Some(id) = self.stream.popout.take() {
                    self.stream.show = true;
                    return window::close(id);
                }
            }
            S::StopStream => {
                c.stream.stop();
            }
            S::Frame => {}
        }
        Task::none()
    }

    fn leave_fullscreen_and_popout(&mut self) -> Task<Msg> {
        let mut tasks = Vec::new();
        if self.stream.fullscreen {
            self.stream.fullscreen = false;
            if let Some(w) = self.win {
                tasks.push(window::set_mode(w, window::Mode::Windowed));
            }
        }
        if let Some(id) = self.stream.popout.take() {
            tasks.push(window::close(id));
        }
        Task::batch(tasks)
    }

    fn update_inner(&mut self, msg: Msg) -> Task<Msg> {
        match msg {
            Msg::Booted(Ok(Boot::Ready(c))) => return self.entered_main(c),
            Msg::Booted(Ok(Boot::NeedsSetup(db))) => {
                // Test-/Messhilfe: Identität ohne UI anlegen.
                if let Ok(name) = std::env::var("CAMPFIRE_DEV_NAME") {
                    return Task::perform(create(db, name, String::new()), Msg::SetupDone);
                }
                // Liegt schon ein Backup in Dokumente (z. B. nach Neuinstallation per OneDrive
                // zurueckgekommen), ist Importieren fast immer gemeint: Pfad vorbelegen.
                let existing = documents_backup();
                self.phase = Phase::Setup(Setup {
                    db: Some(db),
                    show_import: existing.is_file(),
                    imp_path: if existing.is_file() {
                        existing.display().to_string()
                    } else {
                        String::new()
                    },
                    ..Setup::default()
                });
                // Ohne Identität gibt es nichts im Hintergrund zu tun: Fenster zeigen.
                if self.win.is_none() && self.background {
                    return Self::open_window();
                }
            }
            Msg::Booted(Err(e)) => self.phase = Phase::Failed(e),
            Msg::Opened(id) => {
                self.win = Some(id);
                self.spaces.set_window(true);
                trace("Fenster offen", self.start);
                if let Phase::Main(c) = &self.phase {
                    return self.spaces.set_focus(true, c).map(Msg::Spaces);
                }
            }
            Msg::Closed(id) => {
                if self.stream.popout == Some(id) {
                    self.stream.popout = None;
                    self.stream.show = true;
                }
                if self.win == Some(id) {
                    self.win = None;
                    self.spaces.set_window(false);
                    self.info = None;
                    if let Phase::Main(c) = &self.phase {
                        let _ = self.spaces.set_focus(false, c);
                    }
                    // Tray-Modus: Oberflaechenzustand freigeben.
                    self.friends.shrink_to_fit();
                }
            }
            Msg::Stream(m) => return self.update_stream(m),
            Msg::FileDropped(p) => {
                return self.update(Msg::Spaces(crate::spaces::Msg::Chat(
                    crate::chat_ui::Msg::Dropped(p),
                )));
            }
            Msg::Tick => self.mem = platform::private_mb(),
            Msg::Focus(f) => {
                if let Phase::Main(c) = &self.phase {
                    return self.spaces.set_focus(f, c).map(Msg::Spaces);
                }
            }
            Msg::OpenSettings => {
                self.settings.open();
                self.settings_open = true;
            }
            Msg::OpenBackup => {
                self.settings.open();
                self.settings.tab = settings_ui::Tab::Account;
                self.settings_open = true;
            }
            Msg::AskRemove(u) => self.confirm_remove = u,
            Msg::Settings(m) => match m {
                settings_ui::Msg::Close => self.settings_open = false,
                settings_ui::Msg::ToggleAutostart => {
                    return self.update_inner(Msg::ToggleAutostart)
                }
                settings_ui::Msg::ToggleTheme => return self.update_inner(Msg::ToggleTheme),
                settings_ui::Msg::SetLanguage(l) => return self.update_inner(Msg::SetLanguage(l)),
                settings_ui::Msg::Colors(m) => {
                    let (changed, task) = crate::colors_ui::update(
                        &mut self.colors,
                        &mut self.settings.colors,
                        self.theme,
                        m,
                    );
                    if changed {
                        ui::apply_colors(&self.colors);
                        self.save_setting(settings::COLORS, self.colors.clone());
                    }
                    return task.map(|m| Msg::Settings(settings_ui::Msg::Colors(m)));
                }
                settings_ui::Msg::Sp(m) => return self.update_inner(Msg::Spaces(m)),
                m => {
                    if let Phase::Main(c) = &self.phase {
                        return self.settings.update(m, c).map(Msg::Settings);
                    }
                }
            },
            Msg::OverlayOpened(id) => {
                self.overlay_opening = false;
                self.overlay = Some(id);
                self.overlay_geom = None;
                return Task::batch([
                    window::enable_mouse_passthrough::<Msg>(id),
                    window::raw_id::<Msg>(id).map(Msg::OverlayHandle),
                    window::monitor_size(id).map(Msg::OverlayScreen),
                ]);
            }
            Msg::OverlayHandle(raw) => platform::make_overlay_window(raw),
            Msg::OverlayScreen(sz) => {
                if sz.is_some() {
                    self.screen = sz;
                }
            }
            Msg::Call(u) => {
                if let Phase::Main(c) = &self.phase {
                    // Aus einem Voice-Channel heraus anrufen: Channel verlassen, aber nur, wenn
                    // der Freund ueberhaupt erreichbar ist.
                    let reachable = c.net.peers().iter().any(|p| p.user == u);
                    let in_channel = c
                        .voice
                        .current()
                        .is_some_and(|cur| cur.0 != app_audio::service::CALL_SPACE);
                    if reachable && in_channel {
                        c.leave_voice();
                    }
                    self.call_error = c.voice.call(u).err().map(str::to_owned);
                }
            }
            Msg::CallAccept => {
                if let Phase::Main(c) = &self.phase {
                    c.leave_voice();
                    c.voice.accept_call();
                }
            }
            Msg::CallDecline => {
                if let Phase::Main(c) = &self.phase {
                    c.voice.decline_call();
                }
            }
            Msg::CallCancel => {
                if let Phase::Main(c) = &self.phase {
                    c.voice.cancel_call();
                }
            }
            Msg::CallHangup => {
                self.call_error = None;
                if let Phase::Main(c) = &self.phase {
                    c.voice.hangup();
                }
            }
            Msg::Sig(Signal::Show) => {
                if self.win.is_none() {
                    return Self::open_window();
                }
                return window::gain_focus(self.win.unwrap());
            }
            Msg::Sig(Signal::OpenChannel(space, channel)) => {
                let show = match self.win {
                    None => Self::open_window(),
                    Some(w) => window::gain_focus(w),
                };
                if let Phase::Main(c) = &self.phase {
                    let go = self
                        .spaces
                        .update(crate::spaces::Msg::GoTo(space, channel), c)
                        .map(Msg::Spaces);
                    return Task::batch([show, go]);
                }
                return show;
            }
            Msg::Sig(Signal::Quit) => {
                if let Some(c) = self.core() {
                    let n = c.net.clone();
                    return Task::perform(async move { n.shutdown().await }, |()| {
                        Msg::Sig(Signal::Exit)
                    });
                }
                return iced::exit();
            }
            Msg::Sig(Signal::Exit) => return iced::exit(),
            Msg::Sig(Signal::Theme(t)) => self.theme = t,
            Msg::Sig(Signal::FriendsChanged(new_request)) => {
                if new_request {
                    self.pending_requests = true;
                }
                return self.reload_friends();
            }
            Msg::Sig(
                Signal::PresenceChanged
                | Signal::VoiceChanged
                | Signal::HotkeysChanged
                | Signal::StreamChanged
                | Signal::FilesChanged,
            ) => {}
            Msg::Sig(Signal::Hotkey(a)) => {
                use app_hotkeys::engine::Action;
                let m = match a {
                    Action::ToggleMute => crate::spaces::Msg::ToggleMute,
                    Action::ToggleDeafen => crate::spaces::Msg::ToggleDeaf,
                    Action::PushToTalk => return Task::none(),
                    Action::ToggleStream => crate::spaces::Msg::ToggleStream,
                    Action::ToggleOverlay => {
                        if let Phase::Main(c) = &self.phase {
                            let mut o = c.overlay();
                            o.enabled = !o.enabled;
                            c.set_overlay(o);
                        }
                        return Task::none();
                    }
                };
                return self.update(Msg::Spaces(m));
            }
            Msg::Sig(Signal::SpacesChanged) => {
                if let Phase::Main(c) = &self.phase {
                    return self.spaces.reload(c).map(Msg::Spaces);
                }
            }
            Msg::Spaces(crate::spaces::Msg::WatchStream(d)) => {
                return self.update_stream(crate::stream_ui::Msg::Watch(d));
            }
            Msg::Spaces(spaces::Msg::CallUser(u)) => return self.update_inner(Msg::Call(u)),
            Msg::Spaces(spaces::Msg::AcceptCall) => return self.update_inner(Msg::CallAccept),
            Msg::Spaces(spaces::Msg::DeclineCall) => return self.update_inner(Msg::CallDecline),
            Msg::Spaces(spaces::Msg::CancelCall) => return self.update_inner(Msg::CallCancel),
            Msg::Spaces(spaces::Msg::HangUp) => return self.update_inner(Msg::CallHangup),
            Msg::Spaces(m) => {
                // Wer einen Channel oder Chat oeffnet, will nicht mehr den Stream im Hauptbereich.
                if matches!(
                    m,
                    spaces::Msg::OpenText(_)
                        | spaces::Msg::GoTo(..)
                        | spaces::Msg::OpenDm(_)
                        | spaces::Msg::ShowMembers
                ) {
                    self.stream.show = false;
                }
                if let Phase::Main(c) = &self.phase {
                    return self.spaces.update(m, c).map(Msg::Spaces);
                }
            }
            Msg::ToggleImport => {
                if let Phase::Setup(st) = &mut self.phase {
                    st.show_import = !st.show_import;
                }
            }
            Msg::Name(s) => {
                if let Phase::Setup(st) = &mut self.phase {
                    st.name = s;
                }
            }
            Msg::Pass(s) => {
                if let Phase::Setup(st) = &mut self.phase {
                    st.pass = s;
                    st.skip_warned = false;
                    st.error = None;
                }
            }
            Msg::ImportPath(s) => {
                if let Phase::Setup(st) = &mut self.phase {
                    st.imp_path = s;
                }
            }
            Msg::ImportPass(s) => {
                if let Phase::Setup(st) = &mut self.phase {
                    st.imp_pass = s;
                }
            }
            Msg::Create => {
                if let Phase::Setup(st) = &mut self.phase {
                    let Some(db) = st.db.clone() else {
                        return Task::none();
                    };
                    // Das Passwort gilt so, wie es eingegeben ist (auch Leerzeichen am Rand):
                    // beim Import wird es genauso verlangt.
                    let pass_len = st.pass.chars().count();
                    if pass_len > 0 && pass_len < MIN_PASSPHRASE_LEN {
                        st.error = Some(app_core::tr!(
                            "The password needs at least {MIN_PASSPHRASE_LEN} characters.",
                            "Das Passwort braucht mindestens {MIN_PASSPHRASE_LEN} Zeichen."
                        ));
                        return Task::none();
                    }
                    if documents_backup().is_file() && !st.existing_warned {
                        st.existing_warned = true;
                        st.error = Some(app_core::tr!(
                            "There is already a backup in Documents ({}). If that is your existing account, choose “I already have an account”: a new account has none of your friendships. Click again to create a new account anyway (the old backup is kept).",
                            "In Dokumente liegt schon ein Backup ({}). Wenn das dein bisheriges Konto ist, wähle „Ich habe schon ein Konto“: Ein neues Konto hat keine deiner Freundschaften. Klicke noch einmal, um trotzdem ein neues Konto anzulegen (das alte Backup bleibt erhalten).",
                            documents_backup().display()
                        ));
                        return Task::none();
                    }
                    if pass_len == 0 && !st.skip_warned {
                        st.skip_warned = true;
                        st.error = Some(
                            tr("Without a backup, account and friends are gone if your PC breaks or is reinstalled. Enter a password, or click again to start without a backup anyway (the app will remind you later).", "Ohne Sicherung sind Konto und Freunde weg, wenn dein PC kaputtgeht oder neu aufgesetzt wird. Gib ein Passwort ein, oder klicke noch einmal, um trotzdem ohne Sicherung zu starten (die App erinnert dich später daran).").into(),
                        );
                        return Task::none();
                    }
                    st.busy = true;
                    st.error = None;
                    let (name, pass) = (st.name.clone(), st.pass.clone());
                    return Task::perform(create(db, name, pass), Msg::SetupDone);
                }
            }
            Msg::Import => {
                if let Phase::Setup(st) = &mut self.phase {
                    let Some(db) = st.db.clone() else {
                        return Task::none();
                    };
                    st.busy = true;
                    st.error = None;
                    let (path, pass) = (st.imp_path.clone(), st.imp_pass.clone());
                    return Task::perform(import(db, path, pass), Msg::SetupDone);
                }
            }
            Msg::SetupDone(Ok(c)) => return self.entered_main(c),
            Msg::SetupDone(Err(e)) => {
                if let Phase::Setup(st) = &mut self.phase {
                    st.busy = false;
                    st.error = Some(e);
                }
            }
            Msg::Friends(list) => {
                if !list.iter().any(|f| f.status == FriendStatus::PendingIn) {
                    self.pending_requests = false;
                }
                self.friends = list;
            }
            Msg::CodeInput(s) => self.code_input = s,
            Msg::NoteInput(s) => self.note_input = s,
            Msg::AddFriend => {
                let Some(c) = self.core() else {
                    return Task::none();
                };
                let (f, code, note) = (
                    c.friends.clone(),
                    self.code_input.clone(),
                    self.note_input.clone(),
                );
                return Task::perform(
                    async move { f.add_friend(&code, &note).await.map(|_| ()).map_err(err) },
                    Msg::AddResult,
                );
            }
            Msg::AddResult(r) => match r {
                Ok(()) => {
                    self.code_input.clear();
                    self.note_input.clear();
                    self.info = Some(
                        tr(
                            "The request will be delivered as soon as your friend is online.",
                            "Anfrage wird zugestellt, sobald dein Freund online ist.",
                        )
                        .into(),
                    );
                    return self.reload_friends();
                }
                Err(e) => self.info = Some(e),
            },
            Msg::Accept(u) | Msg::Decline(u) | Msg::Block(u) | Msg::Remove(u) => {
                self.confirm_remove = None;
                let Some(c) = self.core() else {
                    return Task::none();
                };
                let core = c.clone();
                let f = c.friends.clone();
                let accept = matches!(msg, Msg::Accept(_));
                let decline = matches!(msg, Msg::Decline(_));
                let block = matches!(msg, Msg::Block(_));
                return Task::perform(
                    async move {
                        let r = if accept {
                            f.accept(u).await
                        } else if decline {
                            f.decline(u).await
                        } else if block {
                            f.block(u).await
                        } else {
                            f.remove(u).await
                        };
                        // Reconnect-Ziele nachziehen (Blockieren/Entfernen gibt sie auf).
                        core.sync_targets().await;
                        r.map_err(err)
                    },
                    Msg::Done,
                );
            }
            Msg::Done(r) => {
                if let Err(e) = r {
                    self.info = Some(e);
                }
                return self.reload_friends();
            }
            Msg::CopyCode => {
                if let Some(code) = self.core().map(|c| c.my_code.clone()) {
                    self.info = Some(tr("Code copied.", "Code kopiert.").into());
                    return iced::clipboard::write(code);
                }
            }
            Msg::ToggleTheme => {
                self.theme = match self.theme {
                    ThemeChoice::Dark => ThemeChoice::Light,
                    ThemeChoice::Light => ThemeChoice::Dark,
                };
                self.save_setting(settings::THEME, self.theme);
            }
            Msg::LanguageLoaded(l) => {
                if let Some(l) = l {
                    app_core::i18n::set_language(l);
                    crate::refresh_tray_language();
                }
            }
            Msg::SetLanguage(l) => {
                app_core::i18n::set_language(l);
                crate::refresh_tray_language();
                self.save_setting(settings::LANGUAGE, l);
            }
            Msg::ToggleAutostart => {
                let want = !self.autostart;
                match platform::set_autostart(want) {
                    Ok(()) => {
                        self.autostart = want;
                        self.save_setting(settings::AUTOSTART, want);
                    }
                    Err(e) => self.info = Some(format!("Autostart: {e}")),
                }
            }
            Msg::SetStatus(s) => {
                if let Some(c) = self.core() {
                    c.local
                        .update(|p| p.status = s, app_core::presence::idle_time());
                }
            }
            Msg::BackupHint(b) => self.backup_hint = b,
            Msg::ColorsLoaded(c) => {
                ui::apply_colors(&c);
                self.colors = c;
            }
        }
        Task::none()
    }

    pub fn subscription(&self) -> Subscription<Msg> {
        let mut subs = vec![
            Subscription::run(signals::stream).map(Msg::Sig),
            window::close_events().map(Msg::Closed),
            iced::event::listen_with(|e, _, _| match e {
                iced::Event::Window(window::Event::Focused) => Some(Msg::Focus(true)),
                iced::Event::Window(window::Event::Unfocused) => Some(Msg::Focus(false)),
                _ => None,
            }),
        ];
        if let Phase::Main(c) = &self.phase {
            if self.stream.visible() && c.stream.watching().is_some() {
                // Bei jedem Bildaufbau neu zeichnen, damit neue Videobilder erscheinen.
                subs.push(window::frames().map(|_| Msg::Stream(crate::stream_ui::Msg::Frame)));
            }
            if self.stream.fullscreen {
                subs.push(iced::keyboard::listen().filter_map(|e| match e {
                    iced::keyboard::Event::KeyPressed {
                        key: iced::keyboard::Key::Named(iced::keyboard::key::Named::Escape),
                        ..
                    } => Some(Msg::Stream(crate::stream_ui::Msg::Fullscreen(false))),
                    _ => None,
                }));
            }
        }
        subs.push(iced::event::listen_with(|e, _, _| match e {
            iced::Event::Window(window::Event::FileDropped(p)) => Some(Msg::FileDropped(p)),
            _ => None,
        }));
        if self.win.is_some() {
            subs.push(iced::time::every(Duration::from_secs(2)).map(|_| Msg::Tick));
            if matches!(&self.phase, Phase::Main(c) if c.voice.current().is_some() || c.monitoring())
            {
                // "Wer spricht": Pegelanzeige aktualisieren.
                subs.push(
                    iced::time::every(Duration::from_millis(150))
                        .map(|_| Msg::Sig(Signal::VoiceChanged)),
                );
            }
        }
        Subscription::batch(subs)
    }

    pub fn theme(&self, _: window::Id) -> Theme {
        crate::ui::theme(self.theme)
    }

    pub fn title(&self, id: window::Id) -> String {
        if self.stream.popout == Some(id) {
            return "Campfire – Stream".into();
        }
        if let Phase::Main(c) = &self.phase {
            let name = |u: &PublicKey| {
                self.friends
                    .iter()
                    .find(|f| f.user == *u)
                    .map_or_else(|| tr("Unknown", "Unbekannt").to_owned(), |f| f.name.clone())
            };
            match c.voice.call_state() {
                CallState::Incoming { user } => {
                    return app_core::tr!(
                        "Campfire – {} is calling",
                        "Campfire – {} ruft an",
                        name(&user)
                    );
                }
                CallState::Outgoing { user } => {
                    return app_core::tr!(
                        "Campfire – Calling {}",
                        "Campfire – Rufe {} an",
                        name(&user)
                    );
                }
                CallState::Active { user } => {
                    return app_core::tr!(
                        "Campfire – In a call with {}",
                        "Campfire – Im Gespräch mit {}",
                        name(&user)
                    );
                }
                CallState::Idle => {}
            }
            if c.voice.current().is_some() {
                if let Some(ch) = self.spaces.voice_channel_name() {
                    return app_core::tr!("Campfire – Voice: {}", "Campfire – Sprache: {}", ch);
                }
            }
        }
        "Campfire".into()
    }

    pub fn view(&self, id: window::Id) -> Element<'_, Msg> {
        if let (Some(_), Phase::Main(c)) = (self.overlay.filter(|o| *o == id), &self.phase) {
            let s = c.overlay();
            return overlay::view(&s, &overlay::rows(c, &self.friends, &s));
        }
        if let Phase::Main(c) = &self.phase {
            use crate::stream_ui::{view as stream_view, Place};
            if self.stream.popout == Some(id) {
                return match c.stream.watching() {
                    Some(w) => stream_view(&w, c.stream.volume(), &self.stream, Place::Popout)
                        .map(Msg::Stream),
                    None => surface(ui::empty_state(
                        "▢",
                        tr("No stream", "Kein Stream"),
                        tr(
                            "The stream has ended. You can close this window.",
                            "Der Stream ist beendet. Du kannst dieses Fenster schließen.",
                        ),
                        None,
                    )),
                };
            }
            if self.stream.fullscreen && self.win == Some(id) {
                if let Some(w) = c.stream.watching() {
                    return stream_view(&w, c.stream.volume(), &self.stream, Place::Fullscreen)
                        .map(Msg::Stream);
                }
            }
        }
        match &self.phase {
            Phase::Loading => surface(
                column![ui::logo(56.0), ui::muted(tr("Campfire is starting …", "Campfire startet …"))]
                    .spacing(space::L)
                    .align_x(Alignment::Center),
            ),
            Phase::Failed(e) => surface(
                container(
                    column![
                        ui::logo(48.0),
                        ui::title(tr("Start failed", "Start fehlgeschlagen")),
                        ui::notice(Tone::Danger, e.as_str()),
                        ui::muted(
                            tr("Restart Campfire. If that doesn't help, export diagnostics in the settings.", "Starte Campfire neu. Hilft das nicht, exportiere in den Einstellungen eine Diagnose.")
                        ),
                    ]
                    .spacing(space::M),
                )
                .max_width(460)
                .padding(space::XL)
                .style(ui::bg::card),
            ),
            Phase::Setup(st) => view_setup(st),
            Phase::Main(c) => self.view_main(c),
        }
    }

    fn view_main<'a>(&'a self, c: &'a Core) -> Element<'a, Msg> {
        let rail = self.spaces.bar().map(Msg::Spaces);
        let sidebar = container(column![
            container(self.spaces.channels(c, &self.friends).map(Msg::Spaces)).height(Length::Fill),
            self.spaces
                .voice_dock(c, &self.friends)
                .map_or_else(|| Element::from(Space::new()), |d| d.map(Msg::Spaces)),
            crate::stream_ui::panel(&c.stream, &self.stream).map(Msg::Stream),
            self.user_bar(c),
        ])
        .width(ui::SIDEBAR_WIDTH)
        .height(Length::Fill)
        .style(ui::bg::sidebar);

        let content: Element<'a, Msg> = if self.settings_open {
            self.settings
                .view(
                    c,
                    &self.spaces,
                    &self.friends,
                    self.autostart,
                    self.theme,
                    &self.colors,
                )
                .map(Msg::Settings)
        } else if let Some(w) = c.stream.watching().filter(|_| self.stream.show) {
            if self.stream.popout.is_some() {
                crate::stream_ui::popped_out(&w.name).map(Msg::Stream)
            } else {
                let place = crate::stream_ui::Place::Main;
                crate::stream_ui::view(&w, c.stream.volume(), &self.stream, place).map(Msg::Stream)
            }
        } else if self.spaces.sel.is_some() {
            self.spaces.main(c, &self.friends).map(Msg::Spaces)
        } else {
            self.view_friends(c)
        };
        let mut main = column![];
        if let Some(b) = self.call_bar(c) {
            main = main.push(container(b).padding([space::M, space::L]));
        }
        let main = container(main.push(content))
            .width(Length::Fill)
            .height(Length::Fill)
            .style(ui::bg::content);
        row![rail, sidebar, main].into()
    }

    /// Eigenes Profil unten in der Seitenleiste: Status und Einstellungen.
    fn user_bar<'a>(&'a self, c: &'a Core) -> Element<'a, Msg> {
        let status = c.presence.local().status;
        let me = ui::tip(
            ui::avatar_status(&c.name, &c.user, 32.0, Some(status), ui::on_rail),
            app_core::tr!("Memory: {:.0} MB", "Arbeitsspeicher: {:.0} MB", self.mem),
            tooltip::Position::Top,
        );
        container(
            row![
                me,
                column![
                    text(c.name.as_str())
                        .size(size::SMALL)
                        .font(ui::SEMIBOLD)
                        .wrapping(text::Wrapping::None),
                    ui::select(
                        &STATUS_CHOICES[..],
                        Some(StatusChoice(status)),
                        |c: StatusChoice| Msg::SetStatus(c.0)
                    )
                    .text_size(size::CAPTION)
                    .padding([2, 6]),
                ]
                .spacing(2)
                .width(Length::Fill),
                ui::tip(
                    button(
                        container(text("⚙").size(18).line_height(1.0))
                            .center_x(Length::Fixed(20.0))
                            .center_y(Length::Fixed(20.0)),
                    )
                    .padding(6)
                    .style(btn::chip_or_ghost(self.settings_open))
                    .on_press(if self.settings_open {
                        Msg::Settings(settings_ui::Msg::Close)
                    } else {
                        Msg::OpenSettings
                    }),
                    tr("Settings", "Einstellungen"),
                    tooltip::Position::Top,
                ),
            ]
            .spacing(space::S + 2.0)
            .align_y(Alignment::Center),
        )
        .padding([space::S, space::S + 2.0])
        .width(Length::Fill)
        .style(ui::bg::rail)
        .into()
    }

    /// Anrufleiste: Klingeln, Annehmen/Ablehnen, laufendes Gespraech.
    fn call_bar<'a>(&'a self, c: &'a Core) -> Option<Element<'a, Msg>> {
        let name = |u: &PublicKey| {
            self.friends
                .iter()
                .find(|f| f.user == *u)
                .map_or_else(|| tr("Unknown", "Unbekannt").to_owned(), |f| f.name.clone())
        };
        let who = |u: &PublicKey, title: String, sub: &'static str| {
            row![
                ui::avatar(&name(u), u, 36.0),
                column![
                    text(title).size(size::BODY).font(ui::SEMIBOLD),
                    ui::caption(sub),
                ]
                .spacing(2),
            ]
            .spacing(space::M)
            .align_y(Alignment::Center)
        };
        let (tone, content): (Tone, Element<'a, Msg>) = match c.voice.call_state() {
            CallState::Idle => {
                let (tone, note) = match &self.call_error {
                    Some(e) => (Tone::Warning, e.clone()),
                    None => (Tone::Info, c.voice.call_note()?.to_owned()),
                };
                (
                    tone,
                    row![
                        text(note).size(size::SMALL).width(Length::Fill),
                        ui::button_small("OK")
                            .style(btn::secondary)
                            .on_press(Msg::CallHangup),
                    ]
                    .spacing(space::S)
                    .align_y(Alignment::Center)
                    .into(),
                )
            }
            CallState::Incoming { user } => (
                Tone::Info,
                row![
                    who(
                        &user,
                        app_core::tr!("{} is calling …", "{} ruft an …", name(&user)),
                        tr("Incoming call", "Eingehender Anruf")
                    ),
                    Space::new().width(Length::Fill),
                    ui::button_text(tr("Accept", "Annehmen"))
                        .style(btn::success)
                        .on_press(Msg::CallAccept),
                    ui::button_text(tr("Decline", "Ablehnen"))
                        .style(btn::danger)
                        .on_press(Msg::CallDecline),
                ]
                .spacing(space::S)
                .align_y(Alignment::Center)
                .into(),
            ),
            CallState::Outgoing { user } => (
                Tone::Info,
                row![
                    who(
                        &user,
                        app_core::tr!("Calling {} …", "Rufe {} an …", name(&user)),
                        tr("Ringing", "Es klingelt")
                    ),
                    Space::new().width(Length::Fill),
                    ui::button_text(tr("Cancel", "Abbrechen"))
                        .style(btn::danger)
                        .on_press(Msg::CallCancel),
                ]
                .spacing(space::S)
                .align_y(Alignment::Center)
                .into(),
            ),
            CallState::Active { user } => (Tone::Success, self.active_call(c, user)),
        };
        Some(ui::banner(tone, content))
    }

    /// Inhalt der Anrufleiste waehrend eines Gespraechs: wer, wie lange, ob er spricht,
    /// Mikro/Ton (mit Zustand) und Auflegen.
    fn active_call<'a>(&'a self, c: &'a Core, user: PublicKey) -> Element<'a, Msg> {
        let name = self
            .friends
            .iter()
            .find(|f| f.user == user)
            .map_or_else(|| tr("Unknown", "Unbekannt").to_owned(), |f| f.name.clone());
        let (muted, deaf) = (self.spaces.muted(), self.spaces.deafened());
        let speaking = c
            .voice
            .core()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .speaking();
        let parts = c.voice.participants();
        let peer = parts.iter().find(|p| p.user == user);
        let talking = peer.is_some_and(|p| speaking.contains(&p.device));
        let since = c
            .voice
            .connected_since()
            .map(|t| ui::fmt_duration(t.elapsed()))
            .unwrap_or_default();
        let sub = match peer {
            // Gegenueber hat sich noch nicht gemeldet: Audio baut sich gerade auf.
            None => app_core::tr!("Connecting … {}", "Verbinde … {}", since),
            Some(p) if p.deafened => {
                app_core::tr!("{} · {} hears nothing", "{} · {} hört nichts", since, name)
            }
            Some(p) if p.muted => {
                app_core::tr!("{} · {} is muted", "{} · {} ist stumm", since, name)
            }
            Some(_) => since,
        };
        let toggle = |off: bool,
                      on_label: &'static str,
                      off_label: &'static str,
                      m: spaces::Msg,
                      on_tip: &'static str,
                      off_tip: &'static str| {
            ui::tip(
                ui::button_small(if off { off_label } else { on_label })
                    .style(if off { btn::danger } else { btn::secondary })
                    .on_press(Msg::Spaces(m)),
                if off { off_tip } else { on_tip },
                tooltip::Position::Bottom,
            )
        };
        row![
            ui::avatar_ring(
                &name,
                &user,
                36.0,
                talking.then_some(ui::speaking_ring as fn(&ui::Tokens) -> iced::Color),
            ),
            column![
                text(app_core::tr!(
                    "In a call with {}",
                    "Im Gespräch mit {}",
                    name
                ))
                .size(size::BODY)
                .font(ui::SEMIBOLD),
                ui::caption(sub),
            ]
            .spacing(2),
            Space::new().width(Length::Fill),
            toggle(
                muted,
                tr("Mic on", "Mikro an"),
                tr("Mic is off", "Mikro ist aus"),
                spaces::Msg::ToggleMute,
                tr("Mute microphone", "Mikrofon stumm schalten"),
                tr("Turn microphone back on", "Mikrofon wieder einschalten"),
            ),
            toggle(
                deaf,
                tr("Sound on", "Ton an"),
                tr("Sound is off", "Ton ist aus"),
                spaces::Msg::ToggleDeaf,
                tr("Stop hearing (and mute)", "Nichts mehr hören (und stumm)"),
                tr("Hear again", "Wieder hören"),
            ),
            ui::button_text(tr("Hang up", "Auflegen"))
                .style(btn::danger)
                .on_press(Msg::CallHangup),
        ]
        .spacing(space::S)
        .align_y(Alignment::Center)
        .into()
    }

    /// Startseite: Freunde, Anfragen, eigener Code, neuer Space.
    fn view_friends<'a>(&'a self, c: &'a Core) -> Element<'a, Msg> {
        let with = |s: FriendStatus| self.friends.iter().filter(move |f| f.status == s);
        let mut page = column![ui::muted(
            tr("Send your code to a friend. They paste it on their side, and you are connected.", "Schicke deinen Code an einen Freund. Er fügt ihn bei sich ein, und ihr seid verbunden.")
        )]
        .spacing(space::L);

        if self.backup_hint {
            page = page.push(ui::banner(
                Tone::Warning,
                row![
                    column![
                        text(tr("No backup yet", "Noch keine Sicherung"))
                            .size(size::SMALL)
                            .font(ui::SEMIBOLD),
                        text(tr("Without a backup, friends and account are gone if the computer breaks.", "Ohne Sicherung sind Freunde und Konto weg, wenn der Computer kaputtgeht."))
                            .size(size::SMALL),
                    ]
                    .spacing(2)
                    .width(Length::Fill),
                    ui::button_small(tr("Create backup", "Sicherung anlegen"))
                        .style(btn::secondary)
                        .on_press(Msg::OpenBackup),
                ]
                .spacing(space::M)
                .align_y(Alignment::Center),
            ));
        }
        if let Some(i) = &self.info {
            page = page.push(ui::notice(Tone::Info, i.as_str()));
        }

        let reqs: Vec<&Friend> = with(FriendStatus::PendingIn).collect();
        if !reqs.is_empty() {
            let n = reqs.len();
            let mut list = column![].spacing(2);
            for f in reqs {
                let mut info = column![text(f.name.as_str()).size(size::BODY).font(ui::SEMIBOLD)]
                    .spacing(2)
                    .width(Length::Fill);
                if let Some(n) = f.note.as_deref().filter(|n| !n.is_empty()) {
                    info = info.push(ui::caption(format!("„{n}“")));
                }
                list = list.push(person_row(
                    ui::avatar(&f.name, &f.user, 36.0),
                    info,
                    row![
                        ui::button_small(tr("Accept", "Annehmen"))
                            .style(btn::success)
                            .on_press(Msg::Accept(f.user)),
                        ui::button_small(tr("Decline", "Ablehnen"))
                            .style(btn::secondary)
                            .on_press(Msg::Decline(f.user)),
                        ui::tip(
                            ui::button_small(tr("Block", "Blockieren"))
                                .style(btn::danger_ghost)
                                .on_press(Msg::Block(f.user)),
                            tr(
                                "This person can no longer send you requests",
                                "Diese Person kann dir keine Anfragen mehr schicken"
                            ),
                            tooltip::Position::Top,
                        ),
                    ]
                    .spacing(6)
                    .into(),
                ));
            }
            page = page.push(ui::section(
                app_core::tr!("Requests · {n}", "Anfragen · {n}"),
                None,
                list,
            ));
        }

        let can_add = !self.code_input.trim().is_empty();
        page = page.push(ui::section(
            tr("Add friend", "Freund hinzufügen"),
            None,
            column![
                ui::field(
                    tr("Your code", "Dein Code"),
                    row![
                        ui::input("", &c.my_code)
                            .font(iced::Font::MONOSPACE)
                            .size(size::SMALL)
                            .width(Length::Fill),
                        ui::button_text(tr("Copy", "Kopieren"))
                            .style(btn::secondary)
                            .on_press(Msg::CopyCode),
                    ]
                    .spacing(space::S)
                    .align_y(Alignment::Center),
                    None,
                ),
                ui::field(
                    tr("Your friend's code", "Code deines Freundes"),
                    column![
                        ui::input(
                            tr("Paste code here", "Code hier einfügen"),
                            &self.code_input
                        )
                        .on_input(Msg::CodeInput)
                        .on_submit(Msg::AddFriend),
                        row![
                            ui::input(
                                tr(
                                    "Note (optional), e.g. who you are",
                                    "Notiz (optional), z. B. wer du bist"
                                ),
                                &self.note_input
                            )
                            .on_input(Msg::NoteInput)
                            .on_submit(Msg::AddFriend)
                            .width(Length::Fill),
                            ui::button_text(tr("Send request", "Anfrage senden"))
                                .style(btn::primary)
                                .on_press_maybe(can_add.then_some(Msg::AddFriend)),
                        ]
                        .spacing(space::S)
                        .align_y(Alignment::Center),
                    ]
                    .spacing(space::S),
                    None,
                ),
            ]
            .spacing(space::L),
        ));

        let accepted: Vec<&Friend> = with(FriendStatus::Accepted).collect();
        let count = accepted.len();
        let friends: Element<'a, Msg> = if accepted.is_empty() {
            container(ui::muted(
                tr("No friends yet. Paste a friend's code above or send them yours.", "Noch keine Freunde. Füge oben den Code eines Freundes ein oder schicke ihm deinen."),
            ))
            .padding([space::S, 0.0])
            .into()
        } else {
            let mut list = column![].spacing(2);
            for f in accepted {
                let status = c.presence.visible(&f.user).map(|p| p.status);
                let online = status.is_some();
                let in_call =
                    matches!(c.voice.call_state(), CallState::Active { user } if user == f.user);
                let info = column![
                    text(f.name.as_str()).size(size::BODY).font(ui::SEMIBOLD),
                    if in_call {
                        Element::from(
                            text(tr("In a call with you", "Im Gespräch mit dir"))
                                .size(size::CAPTION)
                                .style(ui::tx::success),
                        )
                    } else {
                        ui::caption(ui::status_label(status)).into()
                    },
                ]
                .spacing(2)
                .width(Length::Fill);
                let actions: Element<'a, Msg> = if self.confirm_remove == Some(f.user) {
                    row![
                        text(tr("Really remove?", "Wirklich entfernen?"))
                            .size(size::CAPTION)
                            .style(ui::tx::danger),
                        ui::button_small(tr("Remove", "Entfernen"))
                            .style(btn::danger)
                            .on_press(Msg::Remove(f.user)),
                        ui::button_small(tr("Cancel", "Abbrechen"))
                            .style(btn::ghost)
                            .on_press(Msg::AskRemove(None)),
                    ]
                    .spacing(6)
                    .align_y(Alignment::Center)
                    .into()
                } else {
                    let unread = self.spaces.dm_unread(&f.user).unread;
                    let mut dm = row![text(tr("Message", "Nachricht"))
                        .size(size::CAPTION)
                        .font(ui::SEMIBOLD)]
                    .spacing(6)
                    .align_y(Alignment::Center);
                    if unread > 0 {
                        dm = dm.push(ui::count_badge(unread, true));
                    }
                    let state = c.voice.call_state();
                    let can_call = online && state == CallState::Idle;
                    let call: Element<'a, Msg> = match state {
                        CallState::Active { user } if user == f.user => {
                            ui::button_small(tr("Hang up", "Auflegen"))
                                .style(btn::danger)
                                .on_press(Msg::CallHangup)
                                .into()
                        }
                        CallState::Incoming { user } if user == f.user => {
                            ui::button_small(tr("Accept", "Annehmen"))
                                .style(btn::success)
                                .on_press(Msg::CallAccept)
                                .into()
                        }
                        CallState::Outgoing { user } if user == f.user => {
                            ui::button_small(tr("Cancel call", "Anruf abbrechen"))
                                .style(btn::danger)
                                .on_press(Msg::CallCancel)
                                .into()
                        }
                        _ => ui::tip(
                            ui::button_small(tr("Call", "Anrufen"))
                                .style(btn::secondary)
                                .on_press_maybe(can_call.then_some(Msg::Call(f.user))),
                            if can_call {
                                tr("Start voice call", "Sprachanruf starten")
                            } else if online {
                                tr("You are already in a call", "Du bist schon in einem Anruf")
                            } else {
                                tr(
                                    "Calling only works when your friend is online",
                                    "Anrufen geht nur, wenn dein Freund online ist",
                                )
                            },
                            tooltip::Position::Top,
                        ),
                    };
                    row![
                        button(dm)
                            .padding([4, 10])
                            .style(btn::secondary)
                            .on_press(Msg::Spaces(crate::spaces::Msg::OpenDm(f.user))),
                        call,
                        ui::tip(
                            button(text("✕").size(size::CAPTION))
                                .padding([4, 8])
                                .style(btn::danger_ghost)
                                .on_press(Msg::AskRemove(Some(f.user))),
                            tr("Remove friend", "Freund entfernen"),
                            tooltip::Position::Top,
                        ),
                    ]
                    .spacing(6)
                    .align_y(Alignment::Center)
                    .into()
                };
                list = list.push(person_row(
                    ui::avatar_status(&f.name, &f.user, 36.0, status, ui::on_surface),
                    info,
                    actions,
                ));
            }
            list.into()
        };
        page = page.push(ui::section(
            app_core::tr!("Friends · {count}", "Freunde · {count}"),
            None,
            friends,
        ));

        let out: Vec<&Friend> = with(FriendStatus::PendingOut).collect();
        if !out.is_empty() {
            let mut list = column![].spacing(2);
            let n = out.len();
            for f in out {
                list = list.push(person_row(
                    ui::avatar(&f.name, &f.user, 36.0),
                    column![
                        text(f.name.as_str()).size(size::BODY).font(ui::SEMIBOLD),
                        ui::caption(tr("waiting for reply", "wartet auf Antwort")),
                    ]
                    .spacing(2)
                    .width(Length::Fill),
                    ui::button_small(tr("Withdraw", "Zurückziehen"))
                        .style(btn::ghost)
                        .on_press(Msg::Remove(f.user))
                        .into(),
                ));
            }
            page = page.push(ui::section(
                app_core::tr!("Sent · {n}", "Gesendet · {n}"),
                None,
                list,
            ));
        }
        page = page.push(self.spaces.create_form().map(Msg::Spaces));

        column![
            ui::header(
                text(tr("Friends", "Freunde"))
                    .size(size::H3)
                    .font(ui::SEMIBOLD)
                    .style(ui::tx::normal),
                Space::new(),
            ),
            ui::page_with_id(crate::spaces::HOME_PAGE, page),
        ]
        .height(Length::Fill)
        .into()
    }
}

/// Zeile einer Personenliste: Avatar, Angaben, Aktionen.
fn person_row<'a>(
    avatar: Element<'a, Msg>,
    info: impl Into<Element<'a, Msg>>,
    actions: Element<'a, Msg>,
) -> Element<'a, Msg> {
    container(
        row![avatar, info.into(), actions]
            .spacing(space::M)
            .align_y(Alignment::Center),
    )
    .padding([6, 0])
    .into()
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct StatusChoice(PresenceStatus);

static STATUS_CHOICES: [StatusChoice; 3] = [
    StatusChoice(PresenceStatus::Online),
    StatusChoice(PresenceStatus::DoNotDisturb),
    StatusChoice(PresenceStatus::Invisible),
];

impl std::fmt::Display for StatusChoice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(ui::status_label(Some(self.0)))
    }
}

/// Ganze Flaeche im Inhalts-Hintergrund, Inhalt mittig.
fn surface<'a>(e: impl Into<Element<'a, Msg>>) -> Element<'a, Msg> {
    container(e)
        .center(Length::Fill)
        .padding(space::XL)
        .style(ui::bg::content)
        .into()
}

fn view_setup(st: &Setup) -> Element<'_, Msg> {
    let ready = !st.busy;
    let name_ok = !st.name.trim().is_empty();
    let form: Element<'_, Msg> = if st.show_import {
        column![
            column![
                ui::heading(tr("Restore account", "Konto wiederherstellen")),
                ui::muted(
                    tr("Choose the backup file (.age) and enter the password you set when creating it.", "Wähle die Sicherungsdatei (.age) und gib das Passwort ein, das du beim Anlegen vergeben hast.")
                ),
            ]
            .spacing(space::XS),
            ui::field(
                tr("Backup file", "Sicherungsdatei"),
                ui::input(tr("Path to the backup file", "Pfad zur Sicherungsdatei"), &st.imp_path).on_input(Msg::ImportPath),
                Some(tr("Tip: right-click the file → “Copy as path”.", "Tipp: Rechtsklick auf die Datei → „Als Pfad kopieren“.").to_owned()),
            ),
            ui::field(
                tr("Password", "Passwort"),
                ui::input(tr("Password", "Passwort"), &st.imp_pass)
                    .secure(true)
                    .on_input(Msg::ImportPass)
                    .on_submit(Msg::Import),
                None,
            ),
            ui::button_text(tr("Restore", "Wiederherstellen"))
                .width(Length::Fill)
                .style(btn::primary)
                .on_press_maybe((ready && !st.imp_path.trim().is_empty()).then_some(Msg::Import)),
            ui::button_small(tr("Back – create a new account", "Zurück – neues Konto anlegen"))
                .style(btn::link)
                .on_press(Msg::ToggleImport),
        ]
        .spacing(space::L)
        .into()
    } else {
        let skip = st.skip_warned && st.pass.is_empty();
        column![
            ui::field(
                tr("What should your friends call you?", "Wie sollen dich deine Freunde nennen?"),
                ui::input(tr("Your name", "Dein Name"), &st.name)
                    .on_input(Msg::Name)
                    .on_submit(Msg::Create),
                None,
            ),
            ui::field(
                tr("Backup password (recommended)", "Passwort für die Sicherung (empfohlen)"),
                ui::input(tr("Password (optional)", "Passwort (optional)"), &st.pass)
                    .secure(true)
                    .on_input(Msg::Pass)
                    .on_submit(Msg::Create),
                Some(app_core::tr!(
                    "With a password (at least {MIN_PASSPHRASE_LEN} characters) Campfire creates a backup in “Documents”. Without a backup your account is gone if the computer breaks.",
                    "Mit einem Passwort (mindestens {MIN_PASSPHRASE_LEN} Zeichen) legt Campfire eine Sicherung in „Dokumente“ an. Ohne Sicherung ist dein Konto weg, wenn der Computer kaputtgeht."
                )),
            ),
            ui::button_text(if skip {
                tr("Start without a backup anyway", "Trotzdem ohne Sicherung starten")
            } else {
                tr("Let's go", "Los geht’s")
            })
            .width(Length::Fill)
            .style(if skip { btn::secondary } else { btn::primary })
            .on_press_maybe((ready && name_ok).then_some(Msg::Create)),
            ui::button_small(tr("I already have an account", "Ich habe schon ein Konto"))
                .style(btn::link)
                .on_press(Msg::ToggleImport),
        ]
        .spacing(space::L)
        .into()
    };
    let mut card = column![
        column![
            ui::logo(48.0),
            text(tr("Welcome to Campfire", "Willkommen bei Campfire"))
                .size(size::H1)
                .font(ui::BOLD)
                .style(ui::tx::normal),
            ui::muted(
                tr("Talk and write with your friends – directly between your computers, without servers.", "Sprich und schreib mit deinen Freunden – direkt zwischen euren Computern, ohne Server.")
            ),
        ]
        .spacing(space::M),
        form,
    ]
    .spacing(space::XL);
    if let Some(e) = &st.error {
        card = card.push(ui::notice(Tone::Danger, e.as_str()));
    }
    if st.busy {
        card = card.push(ui::muted(tr("One moment …", "Einen Moment …")));
    }
    surface(
        container(card)
            .max_width(460)
            .padding(space::XXL)
            .style(ui::bg::card),
    )
}

/// Ordner fuer Backups ("Dokumente").
pub fn documents_dir() -> std::path::PathBuf {
    let home = std::env::var_os("USERPROFILE").unwrap_or_default();
    std::path::PathBuf::from(home).join("Documents")
}

/// Wo ein Backup unter dem Standardnamen liegt (bzw. liegen wuerde).
pub fn documents_backup() -> std::path::PathBuf {
    documents_dir().join(BACKUP_FILE_NAME)
}

async fn create(db: Db, name: String, pass: String) -> Result<Box<Core>, String> {
    let id = Identity::generate(&name).map_err(err)?;
    let mut backed_up = false;
    if !pass.is_empty() {
        let data = id.export_backup(&pass).map_err(err)?;
        // Nie ein vorhandenes Backup ersetzen (es koennte das einzige eines anderen Kontos sein).
        write_backup_file(&documents_dir(), &data).map_err(|e| {
            app_core::tr!("Backup not writable: {e}", "Backup nicht schreibbar: {e}")
        })?;
        backed_up = true;
    }
    let core = core::finish_setup(db.clone(), id).await?;
    if backed_up {
        let d = db.clone();
        let _ = tokio::task::spawn_blocking(move || d.call(|c| Ok(mark_backup_done(c)))).await;
    }
    Ok(Box::new(core))
}

async fn import(db: Db, path: String, pass: String) -> Result<Box<Core>, String> {
    let data = std::fs::read(path.trim().trim_matches('"'))
        .map_err(|e| app_core::tr!("File: {e}", "Datei: {e}"))?;
    let id = Identity::import_backup(&data, &pass).map_err(err)?;
    let core = core::finish_setup(db.clone(), id).await?;
    let d = db.clone();
    let _ = tokio::task::spawn_blocking(move || d.call(|c| Ok(mark_backup_done(c)))).await;
    Ok(Box::new(core))
}
