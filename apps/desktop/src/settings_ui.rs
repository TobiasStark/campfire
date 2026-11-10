//! Einstellungsseite (M1-16): Konto, Audio, Tasten, Overlay, Netzwerk, Allgemein.
//! Die Bereiche fuer Audio, Tasten und Overlay stammen aus `spaces.rs` und werden hier eingebunden.

use crate::core::{Core, RelaySetting};
use crate::spaces;
use crate::ui::{self, btn, size, space, Tone};
use app_audio::io::{list_inputs, list_outputs, IoConfig};
use app_core::friends::{Friend, FriendStatus};
use app_core::i18n::{self, tr, Language};
use app_core::identity::{
    generate_passphrase, mark_backup_done, passphrase_bits, passphrase_ok, recovery_commitment,
    set_recovery_commitment, Dpapi, Identity, RecoveryKey, BACKUP_FILE_NAME, MIN_PASSPHRASE_LEN,
};
use app_core::settings::ThemeChoice;
use app_net::PathKind;
use app_video::select::{
    choose, AudioMode, Backend, Codec, CodecChoice, EncoderChoice, EncoderInfo, Resolution,
};
use iced::widget::{button, column, container, row, text, Space};
use iced::{Alignment, Element, Length, Task};
use zeroize::Zeroizing;

fn default_device() -> &'static str {
    tr("(Default)", "(Standard)")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Tab {
    #[default]
    Account,
    Audio,
    Keys,
    Overlay,
    Sounds,
    Video,
    Network,
    Colors,
    General,
}

/// Staerkeanzeige unter einem Backup-Passwortfeld (N-9).
pub fn strength_text(p: &str) -> String {
    let n = p.chars().count();
    let label = if p.is_empty() {
        return app_core::tr!(
            "At least {MIN_PASSPHRASE_LEN} characters, best several random words, or let Campfire generate one. Without this password the backup can't be opened: write it down.",
            "Mindestens {MIN_PASSPHRASE_LEN} Zeichen, am besten mehrere zufällige Wörter, oder lass Campfire eins erzeugen. Ohne dieses Passwort lässt sich die Sicherung nicht öffnen: schreib es auf."
        );
    } else if !passphrase_ok(p) {
        tr("too weak", "zu schwach")
    } else if passphrase_bits(p) < 80.0 {
        tr("okay", "in Ordnung")
    } else {
        tr("strong", "stark")
    };
    app_core::tr!(
        "{n} characters, {label}. Write it down: without it the backup can't be opened.",
        "{n} Zeichen, {label}. Schreib es auf: ohne es lässt sich die Sicherung nicht öffnen."
    )
}

pub fn weak_password_text() -> String {
    app_core::tr!(
        "The password is too weak: at least {MIN_PASSPHRASE_LEN} characters and no simple patterns like “1234” or “password”. Tip: several random words, or “Generate”.",
        "Das Passwort ist zu schwach: mindestens {MIN_PASSPHRASE_LEN} Zeichen und keine einfachen Muster wie „1234“ oder „passwort“. Tipp: mehrere zufällige Wörter oder „Erzeugen“."
    )
}

#[derive(Debug, Clone)]
pub enum Msg {
    Tab(Tab),
    Close,
    /// Wird von der App an `spaces::State` weitergereicht.
    Sp(spaces::Msg),
    NameInput(String),
    SaveName,
    NameSaved(Result<(), String>),
    BackupPass(String),
    /// Zufaelliges Passwort erzeugen und anzeigen.
    BackupGenerate,
    BackupShow(bool),
    SaveBackup,
    /// Gewaehlter Speicherort (`None` = abgebrochen).
    BackupTarget(Option<std::path::PathBuf>),
    BackupSaved(Result<String, String>),
    /// Wiederherstellungsschluessel erzeugen und anzeigen (M-7).
    RecoveryCreate,
    RecoveryCopy,
    /// Nutzer hat den angezeigten Schluessel gesichert.
    RecoveryStored,
    /// Bereich "Schluessel ersetzen" auf-/zuklappen.
    RotateOpen(bool),
    RotateInput(String),
    RotateConfirm(bool),
    Rotate,
    /// Neuer Wiederherstellungsschluessel nach dem Wechsel, oder Fehler.
    Rotated(Result<RecoveryKey, String>),
    RelayMode(u8),
    RelayUrl(String),
    ApplyRelay,
    Refresh,
    DeviceIn(String),
    DeviceOut(String),
    Bitrate(f32),
    Reset,
    Snd(SndChange),
    TestSound(app_audio::sfx::Sound),
    ToggleAutostart,
    ExportDiag,
    /// Gefundenes Update herunterladen (nur nach Klick).
    UpdateDownload,
    /// Heruntergeladenes Update installieren und neu starten (nur nach Klick).
    UpdateInstall,
    ToggleTheme,
    SetLanguage(app_core::i18n::Language),
    /// Wird von der App behandelt (Farben gehoeren ihr).
    Colors(crate::colors_ui::ColorMsg),
    /// Was automatisch heruntergeladen werden darf.
    Auto(app_core::settings::AutoDownload),
    /// Groesse des Datei-Speichers in GB (0 = unbegrenzt).
    CacheGb(f32),
    /// Speichergrenze fuer Nachrichten in MB (beim Ziehen).
    MessagesMb(f32),
    /// Regler losgelassen: speichern und aufraeumen.
    MessagesMbDone,
    /// Ergebnis der Encoder-Suche.
    Probed(Vec<EncoderInfo>),
    Vid(VidChange),
}

#[derive(Debug, Clone)]
pub enum VidChange {
    Encoder(String),
    Codec(String),
    Resolution(String),
    Fps(u32),
    Mbps(f32),
    Simulcast(bool),
    Audio(String),
}

#[derive(Debug, Clone, Copy)]
pub enum SndChange {
    Volume(f32),
    JoinLeave(bool),
    Mute(bool),
    Deafen(bool),
    Ptt(bool),
    Call(bool),
}

#[derive(Default)]
pub struct State {
    pub tab: Tab,
    name: Option<String>,
    /// Wird beim Ersetzen und Verwerfen genullt (N-9).
    backup_pass: Zeroizing<String>,
    backup_show: bool,
    /// Es gibt einen Wiederherstellungsschluessel (beim Oeffnen geladen).
    recovery_set: bool,
    /// Speichergrenze fuer Nachrichten (MB) und belegter Platz (Bytes), beim Oeffnen geladen.
    messages_mb: u64,
    messages_used: u64,
    /// Gerade angezeigter neuer Schluessel; `rotated`: nach einem Wechsel (dann Neustart).
    recovery_new: Option<(RecoveryKey, bool)>,
    rotate_open: bool,
    rotate_input: Zeroizing<String>,
    rotate_confirm: bool,
    relay: Option<RelaySetting>,
    inputs: Vec<String>,
    outputs: Vec<String>,
    /// Funktionierende Encoder; `None`, solange gesucht wird oder noch nicht gesucht wurde.
    encoders: Option<Vec<EncoderInfo>>,
    probing: bool,
    info: Option<String>,
    pub colors: crate::colors_ui::State,
}

fn err<E: std::fmt::Display>(e: E) -> String {
    e.to_string()
}

/// Startet Campfire neu; sonst ein Hinweis, es selbst zu tun.
fn restart(info: &mut Option<String>) {
    if crate::platform::restart_app() {
        crate::signals::send(crate::signals::Signal::Quit);
    } else {
        *info = Some(
            tr(
                "Restart not possible, please restart Campfire yourself.",
                "Neustart nicht möglich, bitte Campfire selbst neu starten.",
            )
            .into(),
        );
    }
}

fn devices(list: &[String]) -> Vec<String> {
    let mut v = vec![default_device().to_owned()];
    v.extend(list.iter().cloned());
    v
}

impl State {
    /// Beim Oeffnen: Geraetelisten und gespeicherte Werte frisch laden.
    pub fn open(&mut self, core: &Core) {
        self.inputs = list_inputs();
        self.outputs = list_outputs();
        self.name = None;
        self.relay = None;
        self.info = None;
        self.recovery_set = core
            .db
            .call(|c| Ok(recovery_commitment(c)))
            .ok()
            .and_then(Result::ok)
            .flatten()
            .is_some();
        use app_core::eventlog::prune::{stored_bytes, LIMIT_MB_DEFAULT, LIMIT_MB_MIN};
        let (mb, used) = core
            .db
            .call(|c| {
                let mb = app_core::settings::get::<u64>(c, app_core::settings::MESSAGES_MB)?;
                Ok((mb, stored_bytes(c)?))
            })
            .unwrap_or_default();
        self.messages_mb = mb.unwrap_or(LIMIT_MB_DEFAULT).max(LIMIT_MB_MIN);
        self.messages_used = used;
    }

    /// Nachrichten, die hier selbst behandelt werden (Close/Sp/Toggle* macht die App).
    pub fn update(&mut self, msg: Msg, core: &Core) -> Task<Msg> {
        match msg {
            Msg::Tab(t) => {
                self.tab = t;
                self.info = None;
                if t == Tab::Video && self.encoders.is_none() && !self.probing {
                    self.probing = true;
                    return Task::perform(
                        async {
                            tokio::task::spawn_blocking(crate::video::probe)
                                .await
                                .unwrap_or_default()
                        },
                        Msg::Probed,
                    );
                }
            }
            Msg::Probed(list) => {
                self.probing = false;
                self.encoders = Some(list);
            }
            Msg::Vid(c) => {
                let mut o = core.video();
                match c {
                    VidChange::Encoder(l) => {
                        o.encoder = Backend::ALL
                            .into_iter()
                            .find(|b| b.label() == l)
                            .map_or(EncoderChoice::Auto, EncoderChoice::Only);
                    }
                    VidChange::Codec(l) => {
                        o.codec = Codec::ALL
                            .into_iter()
                            .find(|c| c.label() == l)
                            .map_or(CodecChoice::Auto, CodecChoice::Only);
                    }
                    VidChange::Resolution(l) => {
                        if let Some(r) = Resolution::ALL.into_iter().find(|r| r.label() == l) {
                            o.resolution = r;
                        }
                    }
                    VidChange::Fps(f) => o.fps = f,
                    VidChange::Mbps(m) => o.max_mbps = m.round() as u32,
                    VidChange::Simulcast(v) => o.simulcast = v,
                    VidChange::Audio(l) => {
                        o.audio = AudioMode::ALL
                            .into_iter()
                            .find(|m| m.label() == l)
                            .unwrap_or_default();
                    }
                }
                core.set_video(o);
            }
            Msg::NameInput(s) => self.name = Some(s),
            Msg::SaveName => {
                let name = self.name.clone().unwrap_or_else(|| core.name.clone());
                let name = name.trim().to_owned();
                if name.is_empty() {
                    self.info = Some(
                        tr(
                            "The name must not be empty.",
                            "Der Name darf nicht leer sein.",
                        )
                        .into(),
                    );
                    return Task::none();
                }
                let db = core.db.clone();
                return Task::perform(
                    async move {
                        tokio::task::spawn_blocking(move || {
                            db.call(move |c| {
                                Ok(Identity::load(c, &Dpapi).map_err(err).and_then(|id| {
                                    let mut id = id.ok_or("No identity")?;
                                    id.display_name = name;
                                    id.save(c, &Dpapi).map_err(err)
                                }))
                            })
                            .map_err(err)
                            .and_then(|r| r)
                        })
                        .await
                        .map_err(err)
                        .and_then(|r| r)
                    },
                    Msg::NameSaved,
                );
            }
            Msg::NameSaved(r) => {
                self.info = Some(match r {
                    Ok(()) => tr(
                        "Saved. Your friends will see the new name from the next start.",
                        "Gespeichert. Deine Freunde sehen den neuen Namen ab dem nächsten Start.",
                    )
                    .into(),
                    Err(e) => app_core::tr!("Error: {e}", "Fehler: {e}"),
                });
            }
            Msg::UpdateDownload => crate::updater::download(),
            Msg::UpdateInstall => {
                if crate::updater::install_and_restart() {
                    crate::signals::send(crate::signals::Signal::Quit);
                }
            }
            Msg::ExportDiag => {
                let extra = format!(
                    "Update: {}
Relay: {:?}",
                    crate::updater::describe(),
                    core.relay().mode
                );
                self.info = Some(match crate::diag::export(&extra) {
                    Ok(p) => {
                        // Zur Durchsicht oeffnen: Logzeilen koennen Namen und Dateinamen
                        // enthalten (I-10).
                        crate::platform::open_path(&p);
                        app_core::tr!(
                            "Diagnostics saved: {}. Please read it before sharing; it can contain names and file names.",
                            "Diagnose gespeichert: {}. Bitte vor dem Teilen durchlesen; sie kann Namen und Dateinamen enthalten.",
                            p.display()
                        )
                    }
                    Err(e) => {
                        app_core::tr!("Diagnostics failed: {e}", "Diagnose fehlgeschlagen: {e}")
                    }
                });
            }
            Msg::BackupPass(s) => self.backup_pass = Zeroizing::new(s),
            Msg::BackupGenerate => {
                self.backup_pass = generate_passphrase();
                self.backup_show = true;
            }
            Msg::BackupShow(v) => self.backup_show = v,
            Msg::SaveBackup => {
                if !passphrase_ok(&self.backup_pass) {
                    self.info = Some(weak_password_text());
                    return Task::none();
                }
                // Ablageort waehlen lassen; "Dokumente" ist oft per OneDrive synchronisiert (N-9).
                return Task::perform(
                    async move {
                        rfd::AsyncFileDialog::new()
                            .set_title(tr("Save backup", "Sicherung speichern"))
                            .set_directory(crate::app::documents_dir())
                            .set_file_name(BACKUP_FILE_NAME)
                            .add_filter("Campfire-Backup", &["age"])
                            .save_file()
                            .await
                            .map(|f| f.path().to_path_buf())
                    },
                    Msg::BackupTarget,
                );
            }
            Msg::BackupTarget(None) => {}
            Msg::BackupTarget(Some(path)) => {
                let pass = self.backup_pass.clone();
                let db = core.db.clone();
                return Task::perform(
                    async move {
                        tokio::task::spawn_blocking(move || {
                            let id = db
                                .call(|c| Ok(Identity::load(c, &Dpapi).map_err(err)))
                                .map_err(err)??
                                .ok_or("No identity")?;
                            let data = id.export_backup(&pass).map_err(err)?;
                            // Der Dialog hat ein Ueberschreiben schon bestaetigen lassen.
                            std::fs::write(&path, &data).map_err(err)?;
                            let _ = db.call(|c| Ok(mark_backup_done(c)));
                            let mut msg = path.display().to_string();
                            if crate::app::cloud_synced(&path) {
                                msg.push_str(tr(
                                    ". Note: this folder is synced to the cloud (e.g. OneDrive). Whoever gets the file there can try passwords offline, so a strong password matters; a USB stick is safer.",
                                    ". Hinweis: Dieser Ordner wird in die Cloud synchronisiert (z. B. OneDrive). Wer dort an die Datei kommt, kann offline Passwörter durchprobieren; ein starkes Passwort ist deshalb wichtig, ein USB-Stick sicherer.",
                                ));
                            }
                            Ok(msg)
                        })
                        .await
                        .map_err(err)
                        .and_then(|r: Result<String, String>| r)
                    },
                    Msg::BackupSaved,
                );
            }
            Msg::BackupSaved(r) => {
                self.info = Some(match r {
                    Ok(p) => {
                        self.backup_pass = Zeroizing::default();
                        self.backup_show = false;
                        app_core::tr!("Backup saved: {p}", "Backup gespeichert: {p}")
                    }
                    Err(e) => app_core::tr!("Backup failed: {e}", "Backup fehlgeschlagen: {e}"),
                });
            }
            Msg::RecoveryCreate => match RecoveryKey::generate() {
                Ok(k) => self.recovery_new = Some((k, false)),
                Err(e) => self.info = Some(app_core::tr!("Error: {e}", "Fehler: {e}")),
            },
            Msg::RecoveryCopy => {
                if let Some((k, _)) = &self.recovery_new {
                    return iced::clipboard::write(k.text.to_string());
                }
            }
            Msg::RecoveryStored => {
                let Some((k, rotated)) = self.recovery_new.take() else {
                    return Task::none();
                };
                if rotated {
                    restart(&mut self.info);
                    return Task::none();
                }
                let c = k.commitment;
                match core
                    .db
                    .call(move |conn| Ok(set_recovery_commitment(conn, &c)))
                    .map_err(err)
                    .and_then(|r| r.map_err(err))
                {
                    Ok(()) => {
                        core.net.set_recovery(Some(c));
                        self.recovery_set = true;
                        self.info = Some(
                            tr(
                                "Recovery key set. Friends learn it the next time they connect to you.",
                                "Wiederherstellungsschlüssel eingerichtet. Freunde erfahren davon, sobald sie sich das nächste Mal mit dir verbinden.",
                            )
                            .into(),
                        );
                    }
                    Err(e) => self.info = Some(app_core::tr!("Error: {e}", "Fehler: {e}")),
                }
            }
            Msg::RotateOpen(v) => {
                self.rotate_open = v;
                self.rotate_confirm = false;
                self.rotate_input = Zeroizing::default();
            }
            Msg::RotateInput(s) => self.rotate_input = Zeroizing::new(s),
            Msg::RotateConfirm(v) => self.rotate_confirm = v,
            Msg::Rotate => {
                let text = self.rotate_input.clone();
                let db = core.db.clone();
                return Task::perform(
                    async move {
                        tokio::task::spawn_blocking(move || {
                            db.call(move |c| {
                                Ok((|| -> Result<RecoveryKey, String> {
                                    let id = Identity::load(c, &Dpapi)
                                        .map_err(err)?
                                        .ok_or("No identity")?;
                                    app_core::identity::rotate(c, &Dpapi, &id, &text).map_err(err)
                                })())
                            })
                            .map_err(err)
                            .and_then(|r| r)
                        })
                        .await
                        .map_err(err)
                        .and_then(|r| r)
                    },
                    Msg::Rotated,
                );
            }
            Msg::Rotated(Ok(k)) => {
                self.rotate_open = false;
                self.rotate_confirm = false;
                self.rotate_input = Zeroizing::default();
                // Den neuen Schluessel anzeigen; nach "Gesichert" startet die App neu.
                self.recovery_new = Some((k, true));
            }
            Msg::Rotated(Err(e)) => {
                self.info = Some(app_core::tr!("Error: {e}", "Fehler: {e}"));
            }
            Msg::RelayMode(m) => {
                let mut r = self.relay.clone().unwrap_or_else(|| core.relay());
                r.mode = m;
                self.relay = Some(r);
            }
            Msg::RelayUrl(u) => {
                let mut r = self.relay.clone().unwrap_or_else(|| core.relay());
                r.url = u;
                self.relay = Some(r);
            }
            Msg::ApplyRelay => {
                if let Some(r) = self.relay.clone() {
                    if matches!(r.mode, 1 | 3) && !app_proto::urls::is_public_https(&r.url) {
                        self.info = Some(
                            tr(
                                "The relay address must start with https:// and point to a public server (no local network addresses).",
                                "Die Relay-Adresse muss mit https:// beginnen und auf einen öffentlichen Server zeigen (keine Adressen im lokalen Netz).",
                            )
                            .into(),
                        );
                        return Task::none();
                    }
                    core.set_relay(r);
                    restart(&mut self.info);
                }
            }
            Msg::Refresh => {
                self.inputs = list_inputs();
                self.outputs = list_outputs();
            }
            Msg::DeviceIn(d) => {
                let mut c = core.io_config();
                c.input = (d != default_device()).then_some(d);
                core.set_io_config(c);
            }
            Msg::DeviceOut(d) => {
                let mut c = core.io_config();
                c.output = (d != default_device()).then_some(d);
                core.set_io_config(c);
            }
            Msg::Bitrate(v) => core.set_bitrate(v as u32),
            Msg::Snd(c) => {
                let mut s = core.sounds.settings();
                match c {
                    SndChange::Volume(v) => s.volume = v,
                    SndChange::JoinLeave(v) => s.join_leave = v,
                    SndChange::Mute(v) => s.mute = v,
                    SndChange::Deafen(v) => s.deafen = v,
                    SndChange::Ptt(v) => s.ptt = v,
                    SndChange::Call(v) => s.call = v,
                }
                core.sounds.set(s);
            }
            Msg::Auto(a) => core.files.set_auto(a),
            Msg::CacheGb(v) => core.files.set_cache_mb(v.round() as u64 * 1024),
            Msg::MessagesMb(v) => {
                self.messages_mb = (v.round() as u64).max(app_core::eventlog::prune::LIMIT_MB_MIN);
            }
            Msg::MessagesMbDone => {
                let mb = self.messages_mb;
                let sync = core.sync.clone();
                let _ = core.db.spawn(
                    move |c| app_core::settings::set(c, app_core::settings::MESSAGES_MB, &mb),
                    move |_| {
                        // Kleiner gestellt: gleich aufraeumen.
                        tokio::spawn(async move {
                            sync.enforce_storage().await;
                        });
                    },
                );
            }
            Msg::TestSound(s) => {
                // Der Test braucht laufende Ausgabe: kurz die Wiedergabe oeffnen.
                core.sounds.play_always(s);
                core.sync_audio_for_test();
            }
            Msg::Reset => match self.tab {
                Tab::Audio => {
                    core.set_input(app_audio::process::InputSettings::default());
                    core.set_io_config(IoConfig::default());
                    core.set_bitrate(app_audio::voice::DEFAULT_KBPS);
                }
                Tab::Keys => {
                    core.hotkeys
                        .update(|s| *s = app_hotkeys::engine::HotkeySettings::default());
                    core.reapply_input();
                }
                Tab::Overlay => core.set_overlay(crate::overlay::OverlaySettings::default()),
                Tab::Sounds => core.sounds.set(crate::sounds::SoundSettings::default()),
                Tab::Video => core.set_video(app_video::select::StreamOptions::default()),
                Tab::Network => {
                    self.relay = Some(RelaySetting::default());
                    self.info = Some(
                        tr(
                            "Reset takes effect with “Apply” (restart).",
                            "Zurücksetzen wird mit „Übernehmen“ wirksam (Neustart).",
                        )
                        .into(),
                    );
                }
                Tab::Account | Tab::General | Tab::Colors => {}
            },
            Msg::Close
            | Msg::Sp(_)
            | Msg::ToggleAutostart
            | Msg::ToggleTheme
            | Msg::SetLanguage(_)
            | Msg::Colors(_) => {}
        }
        Task::none()
    }

    pub fn view<'a>(
        &'a self,
        core: &'a Core,
        sp: &'a spaces::State,
        friends: &'a [Friend],
        autostart: bool,
        theme: ThemeChoice,
        colors: &'a app_core::settings::ColorOverrides,
    ) -> Element<'a, Msg> {
        let tab_btn = |label: &'static str, t: Tab| {
            button(text(label).size(size::BODY))
                .padding([7, 10])
                .width(Length::Fill)
                .style(btn::nav(self.tab == t))
                .on_press(Msg::Tab(t))
        };
        let group = |label: &'static str| {
            container(ui::overline(label))
                .padding(iced::Padding::ZERO.top(space::M).bottom(space::XS).left(10))
        };
        let nav = container(column![
            ui::scroll(
                column![
                    group(tr("User", "Benutzer")),
                    tab_btn(tr("Account", "Konto"), Tab::Account),
                    group(tr("Voice and video", "Sprache und Video")),
                    tab_btn("Audio", Tab::Audio),
                    tab_btn(tr("Hotkeys", "Tastenkürzel"), Tab::Keys),
                    tab_btn("Overlay", Tab::Overlay),
                    tab_btn(tr("Sounds", "Töne"), Tab::Sounds),
                    tab_btn("Video", Tab::Video),
                    group("App"),
                    tab_btn(tr("Colors", "Farben"), Tab::Colors),
                    tab_btn(tr("Network", "Netzwerk"), Tab::Network),
                    tab_btn(tr("General", "Allgemein"), Tab::General),
                ]
                .spacing(2)
                .padding([space::S, space::M]),
            )
            .height(Length::Fill),
            container(ui::caption(format!(
                "Campfire {}",
                crate::updater::version()
            )))
            .padding([space::S, space::L]),
        ])
        .width(210)
        .height(Length::Fill)
        .style(|t: &iced::Theme| iced::widget::container::Style {
            background: Some(iced::Background::Color(ui::tokens(t).sidebar)),
            ..Default::default()
        });

        let (title, intro) = match self.tab {
            Tab::Account => (
                tr("Account", "Konto"),
                tr(
                    "Name, friend code and backup of your account.",
                    "Name, Freundescode und Sicherung deines Kontos.",
                ),
            ),
            Tab::Audio => (
                "Audio",
                tr(
                    "Microphone, speakers and voice quality.",
                    "Mikrofon, Lautsprecher und Sprachqualität.",
                ),
            ),
            Tab::Keys => (
                tr("Hotkeys", "Tastenkürzel"),
                tr(
                    "Global keys that also work in games.",
                    "Globale Tasten, die auch im Spiel funktionieren.",
                ),
            ),
            Tab::Overlay => (
                "Overlay",
                tr(
                    "Shows who is currently speaking in the game.",
                    "Zeigt im Spiel, wer gerade spricht.",
                ),
            ),
            Tab::Sounds => (
                tr("Sounds", "Töne"),
                tr(
                    "Short notification sounds for events.",
                    "Kurze Hinweistöne bei Ereignissen.",
                ),
            ),
            Tab::Video => (
                "Video",
                tr(
                    "Screen sharing and encoder.",
                    "Bildschirm-Übertragung und Encoder.",
                ),
            ),
            Tab::Network => (
                tr("Network", "Netzwerk"),
                tr(
                    "How Campfire reaches your friends.",
                    "Wie Campfire deine Freunde erreicht.",
                ),
            ),
            Tab::Colors => (
                tr("Colors", "Farben"),
                tr(
                    "Every color of the interface can be customized.",
                    "Jede Farbe der Oberfläche lässt sich selbst einstellen.",
                ),
            ),
            Tab::General => (
                tr("General", "Allgemein"),
                tr(
                    "Startup, appearance, files and diagnostics.",
                    "Start, Aussehen, Dateien und Diagnose.",
                ),
            ),
        };
        let body: Element<'a, Msg> = match self.tab {
            Tab::Account => self.account(core),
            Tab::Audio => self.audio(core, sp),
            Tab::Keys => ui::section(
                tr("Keys", "Tasten"),
                None,
                sp.hotkey_panel(core).map(Msg::Sp),
            ),
            Tab::Overlay => ui::section(
                tr("Display", "Anzeige"),
                None,
                sp.overlay_panel(core).map(Msg::Sp),
            ),
            Tab::Sounds => self.sounds(core),
            Tab::Video => self.video(core),
            Tab::Network => self.network(core, friends),
            Tab::Colors => self.colors(colors, theme),
            Tab::General => self.general(core, autostart, theme),
        };
        let mut page = column![column![ui::title(title), ui::muted(intro)].spacing(space::XS)]
            .spacing(space::L);
        if let Some(i) = &self.info {
            page = page.push(ui::notice(info_tone(i), i.as_str()));
        }
        page = page.push(body);
        if matches!(
            self.tab,
            Tab::Audio | Tab::Keys | Tab::Overlay | Tab::Sounds | Tab::Video | Tab::Network
        ) {
            page = page.push(
                row![
                    Space::new().width(Length::Fill),
                    ui::button_small(tr("Reset to default", "Auf Standard zurücksetzen"))
                        .style(btn::danger_ghost)
                        .on_press(Msg::Reset),
                ]
                .align_y(Alignment::Center),
            );
        }
        column![
            ui::header(
                text(tr("Settings", "Einstellungen"))
                    .size(size::H3)
                    .font(ui::SEMIBOLD)
                    .style(ui::tx::normal),
                ui::button_small(tr("Close", "Schließen"))
                    .style(btn::secondary)
                    .on_press(Msg::Close),
            ),
            row![nav, ui::page(page)].height(Length::Fill),
        ]
        .height(Length::Fill)
        .into()
    }

    fn colors<'a>(
        &'a self,
        colors: &'a app_core::settings::ColorOverrides,
        theme: ThemeChoice,
    ) -> Element<'a, Msg> {
        let variant = |label: &'static str, v: ThemeChoice| {
            ui::button_small(label)
                .style(btn::chip(theme == v))
                .on_press_maybe((theme != v).then_some(Msg::ToggleTheme))
        };
        column![
            ui::section(
                tr("Variant", "Variante"),
                Some(tr(
                    "Dark and light have separate colors. The active variant is edited.",
                    "Dunkel und hell haben getrennte Farben. Bearbeitet wird die aktive Variante."
                )),
                row![
                    variant(tr("Dark", "Dunkel"), ThemeChoice::Dark),
                    variant(tr("Light", "Hell"), ThemeChoice::Light)
                ]
                .spacing(6),
            ),
            crate::colors_ui::view(colors, &self.colors, theme).map(Msg::Colors),
        ]
        .spacing(space::L)
        .into()
    }

    fn general<'a>(
        &'a self,
        core: &'a Core,
        autostart: bool,
        theme: ThemeChoice,
    ) -> Element<'a, Msg> {
        let auto = core.files.auto();
        let auto_toggle =
            |label: &'static str,
             hint: Option<String>,
             on: bool,
             set: fn(&mut app_core::settings::AutoDownload, bool)| {
                ui::setting(
                    label,
                    hint,
                    ui::toggle(on).on_toggle(move |v| {
                        let mut a = auto;
                        set(&mut a, v);
                        Msg::Auto(a)
                    }),
                )
            };
        let max_mb = auto.max_mb;
        let cache_gb = core.files.cache_mb() / 1024;
        let messages_mb = self.messages_mb;
        let lang_btn = |l: Language| {
            ui::button_small(l.native_name())
                .style(btn::chip(i18n::language() == l))
                .on_press_maybe((i18n::language() != l).then_some(Msg::SetLanguage(l)))
        };
        column![
            ui::section(
                tr("Startup and appearance", "Start und Aussehen"),
                None,
                column![
                    ui::setting(
                        tr("Start with Windows", "Mit Windows starten"),
                        Some(tr("Campfire starts in the background when you sign in.", "Campfire startet beim Anmelden im Hintergrund.").to_owned()),
                        ui::toggle(autostart).on_toggle(|_| Msg::ToggleAutostart),
                    ),
                    ui::setting(
                        tr("Language", "Sprache"),
                        None,
                        row![
                            lang_btn(Language::English),
                            lang_btn(Language::German),
                        ]
                        .spacing(6),
                    ),
                    ui::setting(
                        tr("Theme", "Design"),
                        None,
                        row![
                            ui::button_small(tr("Dark", "Dunkel"))
                                .style(btn::chip(theme == ThemeChoice::Dark))
                                .on_press_maybe(
                                    (theme != ThemeChoice::Dark).then_some(Msg::ToggleTheme)
                                ),
                            ui::button_small(tr("Light", "Hell"))
                                .style(btn::chip(theme == ThemeChoice::Light))
                                .on_press_maybe(
                                    (theme != ThemeChoice::Light).then_some(Msg::ToggleTheme)
                                ),
                        ]
                        .spacing(6),
                    ),
                ]
                .spacing(space::L),
            ),
            ui::section(
                tr("Files", "Dateien"),
                None,
                column![
                    ui::caption(tr(
                        "Nothing is downloaded automatically unless you allow it here. Everything else is downloaded when you click it. Programs, scripts and unknown file types are never downloaded automatically.",
                        "Automatisch geladen wird nur, was du hier erlaubst. Alles andere lädst du per Klick. Programme, Skripte und unbekannte Dateitypen werden nie automatisch geladen.",
                    )),
                    auto_toggle(
                        tr("Previews of pictures and videos", "Vorschaubilder von Bildern und Videos"),
                        Some(tr("Small preview images (at most 256 KB each).", "Kleine Vorschaubilder (höchstens 256 KB je Bild).").to_owned()),
                        auto.previews,
                        |a, v| a.previews = v,
                    ),
                    auto_toggle(tr("Pictures", "Bilder"), None, auto.images, |a, v| a.images = v),
                    auto_toggle(tr("Videos", "Videos"), None, auto.video, |a, v| a.video = v),
                    auto_toggle(tr("Audio", "Audio"), None, auto.audio, |a, v| a.audio = v),
                    auto_toggle(
                        tr("Documents", "Dokumente"),
                        Some(tr("Text, PDF, Office files without macros.", "Text, PDF, Office-Dateien ohne Makros.").to_owned()),
                        auto.documents,
                        |a, v| a.documents = v,
                    ),
                    auto_toggle(
                        tr("Archives", "Archive"),
                        Some(tr("ZIP, 7z, RAR … often contain programs.", "ZIP, 7z, RAR … enthalten oft Programme.").to_owned()),
                        auto.archives,
                        |a, v| a.archives = v,
                    ),
                    ui::slider_field(
                        tr("Largest automatic download", "Größter automatischer Download"),
                        app_core::tr!("up to {max_mb} MB", "bis {max_mb} MB"),
                        ui::slider(1.0..=1000.0, max_mb as f32, move |v| {
                            Msg::Auto(app_core::settings::AutoDownload {
                                max_mb: v.round().max(1.0) as u64,
                                ..auto
                            })
                        })
                        .step(1.0),
                    ),
                    ui::slider_field(
                        tr("File storage", "Datei-Speicher"),
                        if cache_gb == 0 {
                            tr("unlimited", "unbegrenzt").to_owned()
                        } else {
                            app_core::tr!("at most {cache_gb} GB", "höchstens {cache_gb} GB")
                        },
                        ui::slider(0.0..=100.0, cache_gb as f32, Msg::CacheGb).step(1.0),
                    ),
                    ui::caption(app_core::tr!(
                        "Used: {}. When storage is full, the files unused for the longest time are removed (pinned ones stay). You can download them again from friends who still have them.",
                        "Belegt: {}. Ist der Speicher voll, werden die am längsten ungenutzten Dateien entfernt (angepinnte bleiben). Du kannst sie bei Freunden, die sie noch haben, erneut laden.",
                        app_core::files::format_size(core.files.used())
                    )),
                    ui::slider_field(
                        tr("Message storage", "Nachrichten-Speicher"),
                        app_core::tr!(
                            "at most {}",
                            "höchstens {}",
                            app_core::files::format_size(messages_mb * 1024 * 1024)
                        ),
                        ui::slider(
                            app_core::eventlog::prune::LIMIT_MB_MIN as f32..=10_240.0,
                            messages_mb as f32,
                            Msg::MessagesMb,
                        )
                        .step(100.0)
                        .on_release(Msg::MessagesMbDone),
                    ),
                    ui::caption(app_core::tr!(
                        "Used: about {}. When it is full, the oldest messages of whoever uses the most space are removed first, your own last. Spam pushes out the spammer's messages, not your history.",
                        "Belegt: etwa {}. Ist er voll, werden zuerst die ältesten Nachrichten dessen entfernt, der am meisten Platz belegt, deine eigenen zuletzt. Spam verdrängt so die Nachrichten des Spammers, nicht deinen Verlauf.",
                        app_core::files::format_size(self.messages_used)
                    )),
                ]
                .spacing(space::S),
            ),
            ui::section(
                tr("About Campfire", "Über Campfire"),
                None,
                column![
                    ui::muted(crate::updater::describe()),
                    update_action(),
                    ui::setting(
                        tr("Diagnostics", "Diagnose"),
                        Some(tr("Saves logs for troubleshooting to a file.", "Speichert Protokolle für die Fehlersuche als Datei.").to_owned()),
                        ui::button_small(tr("Export", "Exportieren"))
                            .style(btn::secondary)
                            .on_press(Msg::ExportDiag),
                    ),
                ]
                .spacing(space::L),
            ),
        ]
        .spacing(space::L)
        .into()
    }

    fn video<'a>(&'a self, core: &'a Core) -> Element<'a, Msg> {
        let o = core.video();
        let avail: &[EncoderInfo] = self.encoders.as_deref().unwrap_or(&[]);
        let (status, tone) = match &self.encoders {
            None => (
                tr(
                    "Looking for working encoders …",
                    "Suche funktionierende Encoder …",
                )
                .to_owned(),
                Tone::Info,
            ),
            Some(l) if l.is_empty() && !crate::video::built_in() => (
                tr(
                    "This build does not include video support.",
                    "Dieser Build enthält keine Video-Unterstützung.",
                )
                .to_owned(),
                Tone::Warning,
            ),
            Some(l) if l.is_empty() => (
                tr(
                    "No working encoder found.",
                    "Kein funktionierender Encoder gefunden.",
                )
                .to_owned(),
                Tone::Warning,
            ),
            Some(l) => (
                app_core::tr!(
                    "Working: {}",
                    "Funktionierend: {}",
                    l.iter().map(|e| e.name).collect::<Vec<_>>().join(", ")
                ),
                Tone::Success,
            ),
        };
        // Nur anbieten, was tatsaechlich funktioniert.
        let mut backends = vec!["Auto".to_owned()];
        for b in Backend::ALL {
            if avail.iter().any(|e| e.backend == b) {
                backends.push(b.label().to_owned());
            }
        }
        let mut codecs = vec!["Auto".to_owned()];
        for c in Codec::ALL {
            if avail.iter().any(|e| e.codec == c) {
                codecs.push(c.label().to_owned());
            }
        }
        let sel_backend = match o.encoder {
            EncoderChoice::Auto => "Auto".to_owned(),
            EncoderChoice::Only(b) => b.label().to_owned(),
        };
        let sel_codec = match o.codec {
            CodecChoice::Auto => "Auto".to_owned(),
            CodecChoice::Only(c) => c.label().to_owned(),
        };
        let picked = choose(&o, avail, &[]);
        let summary = match picked.best() {
            Some(e) => app_core::tr!(
                "In use: {} ({})",
                "Wird verwendet: {} ({})",
                e.name,
                e.backend.label()
            ),
            None => tr("In use: nothing", "Wird verwendet: nichts").to_owned(),
        };
        let fps_btn = |f: u32| {
            ui::button_small(format!("{f} fps"))
                .style(btn::chip(o.fps == f))
                .on_press(Msg::Vid(VidChange::Fps(f)))
        };
        let mut encoder = column![
            ui::notice(tone, status),
            row![
                ui::field(
                    "Encoder",
                    ui::select(backends, Some(sel_backend), |l| Msg::Vid(
                        VidChange::Encoder(l)
                    ))
                    .width(Length::Fill),
                    None
                ),
                ui::field(
                    "Codec",
                    ui::select(codecs, Some(sel_codec), |l| Msg::Vid(VidChange::Codec(l)))
                        .width(Length::Fill),
                    None
                ),
            ]
            .spacing(space::L),
            ui::muted(summary),
        ]
        .spacing(space::M);
        if let Some(n) = picked.note {
            encoder = encoder.push(ui::notice(Tone::Danger, n));
        }
        column![
            ui::section("Encoder", None, encoder),
            ui::section(
                tr("Quality", "Qualität"),
                None,
                column![
                    row![
                        ui::field(
                            tr("Resolution", "Auflösung"),
                            ui::select(
                                Resolution::ALL.map(|r| r.label().to_owned()).to_vec(),
                                Some(o.resolution.label().to_owned()),
                                |l| Msg::Vid(VidChange::Resolution(l))
                            )
                            .width(Length::Fill),
                            None
                        ),
                        ui::field(
                            tr("Frames per second", "Bilder pro Sekunde"),
                            row![fps_btn(30), fps_btn(60)].spacing(6),
                            None
                        ),
                    ]
                    .spacing(space::L),
                    ui::slider_field(
                        tr("Maximum bitrate", "Höchste Bitrate"),
                        format!("{} Mbit/s", o.max_mbps),
                        ui::slider(1.0..=50.0, o.max_mbps as f32, |v| Msg::Vid(
                            VidChange::Mbps(v)
                        ))
                        .step(1.0),
                    ),
                    ui::setting(
                        tr("Second, lighter version", "Zweite, leichtere Fassung"),
                        Some(
                            tr(
                                "Additional 720p30 for viewers with a weak connection.",
                                "Zusätzlich 720p30 für Zuschauer mit schwacher Leitung."
                            )
                            .to_owned()
                        ),
                        ui::toggle(o.simulcast).on_toggle(|v| Msg::Vid(VidChange::Simulcast(v))),
                    ),
                ]
                .spacing(space::L),
            ),
            ui::section(
                tr("Audio in the stream", "Ton im Stream"),
                Some(tr(
                    "Voice chat is never in the stream. Game audio needs Windows 11.",
                    "Der Voice-Chat ist nie im Stream. Spielsound braucht Windows 11."
                )),
                ui::select(
                    AudioMode::ALL.map(|m| m.label().to_owned()).to_vec(),
                    Some(o.audio.label().to_owned()),
                    |l| Msg::Vid(VidChange::Audio(l))
                )
                .width(Length::Fill),
            ),
        ]
        .spacing(space::L)
        .into()
    }

    fn sounds<'a>(&'a self, core: &'a Core) -> Element<'a, Msg> {
        use app_audio::sfx::Sound;
        let s = core.sounds.settings();
        let test = |label: &'static str, snd: Sound| {
            ui::tip(
                ui::button_small(label)
                    .style(btn::ghost)
                    .on_press(Msg::TestSound(snd)),
                tr("Preview", "Probehören"),
                iced::widget::tooltip::Position::Top,
            )
        };
        let line = |label: &'static str,
                    on: bool,
                    f: fn(bool) -> SndChange,
                    tests: Vec<Element<'a, Msg>>| {
            let mut r = row![text(label).size(size::BODY).width(Length::Fill)]
                .spacing(space::XS)
                .align_y(Alignment::Center);
            for t in tests {
                r = r.push(t);
            }
            r.push(Space::new().width(space::S))
                .push(ui::toggle(on).on_toggle(move |v| Msg::Snd(f(v))))
        };
        column![
            ui::section(
                tr("Volume", "Lautstärke"),
                None,
                ui::slider_field(
                    tr("Notification sounds", "Hinweistöne"),
                    format!("{:.0} %", s.volume * 100.0),
                    ui::slider(0.0..=1.0, s.volume, |v| Msg::Snd(SndChange::Volume(v))).step(0.05),
                ),
            ),
            ui::section(
                tr("Events", "Ereignisse"),
                Some(tr(
                    "Use the buttons to preview each sound.",
                    "Mit den Knöpfen hörst du den Ton probe."
                )),
                column![
                    line(
                        tr("Join / Leave", "Beitreten / Verlassen"),
                        s.join_leave,
                        SndChange::JoinLeave,
                        vec![
                            test(tr("Join", "Beitreten"), Sound::Join),
                            test(tr("Leave", "Verlassen"), Sound::Leave)
                        ]
                    ),
                    ui::divider(),
                    line(
                        tr("Muted", "Stumm"),
                        s.mute,
                        SndChange::Mute,
                        vec![
                            test(tr("on", "an"), Sound::Mute),
                            test(tr("off", "aus"), Sound::Unmute)
                        ]
                    ),
                    ui::divider(),
                    line(
                        tr("Deafened", "Taub"),
                        s.deafen,
                        SndChange::Deafen,
                        vec![
                            test(tr("on", "an"), Sound::Deafen),
                            test(tr("off", "aus"), Sound::Undeafen)
                        ]
                    ),
                    ui::divider(),
                    line(
                        "Push-to-Talk",
                        s.ptt,
                        SndChange::Ptt,
                        vec![
                            test(tr("on", "an"), Sound::PttOn),
                            test(tr("off", "aus"), Sound::PttOff)
                        ]
                    ),
                    ui::divider(),
                    line(
                        tr("Incoming call", "Eingehender Anruf"),
                        s.call,
                        SndChange::Call,
                        vec![test(tr("Ring", "Klingeln"), Sound::Ring)]
                    ),
                ]
                .spacing(space::M),
            ),
        ]
        .spacing(space::L)
        .into()
    }

    fn account<'a>(&'a self, core: &'a Core) -> Element<'a, Msg> {
        let name = self.name.as_deref().unwrap_or(&core.name);
        let changed = name.trim() != core.name && !name.trim().is_empty();
        let pass_ok = passphrase_ok(&self.backup_pass);
        column![
            ui::section(
                tr("Profile", "Profil"),
                None,
                column![
                    row![
                        ui::avatar(name, &core.user, 56.0),
                        column![
                            text(name.to_owned()).size(size::H3).font(ui::SEMIBOLD),
                            ui::caption(tr("This is how your friends see you.", "So sehen dich deine Freunde.")),
                        ]
                        .spacing(2),
                    ]
                    .spacing(space::L)
                    .align_y(Alignment::Center),
                    ui::field(
                        tr("Display name", "Anzeigename"),
                        row![
                            ui::input("Name", name)
                                .on_input(Msg::NameInput)
                                .on_submit(Msg::SaveName)
                                .width(Length::Fill),
                            ui::button_text(tr("Save", "Speichern"))
                                .style(btn::primary)
                                .on_press_maybe(changed.then_some(Msg::SaveName)),
                        ]
                        .spacing(space::S),
                        Some(tr("Your friends will see the new name from the next start.", "Deine Freunde sehen den neuen Namen ab dem nächsten Start.").to_owned()),
                    ),
                ]
                .spacing(space::L),
            ),
            ui::section(
                tr("Friend code", "Freundescode"),
                Some(tr("Give it to friends so they can add you.", "Gib ihn Freunden, damit sie dich hinzufügen können.")),
                ui::input("", &core.my_code).font(iced::Font::MONOSPACE),
            ),
            ui::section(
                tr("Backup", "Sicherung"),
                Some(tr("Saves your account encrypted as a file in a place you choose (best: a USB stick). Without a backup your account is gone if the computer breaks.", "Speichert dein Konto verschlüsselt als Datei an einem Ort deiner Wahl (am besten ein USB-Stick). Ohne Sicherung ist dein Konto weg, wenn der Computer kaputtgeht.")),
                ui::field(
                    tr("Backup password", "Passwort für die Sicherung"),
                    row![
                        ui::input(
                            &app_core::tr!("at least {MIN_PASSPHRASE_LEN} characters", "mindestens {MIN_PASSPHRASE_LEN} Zeichen"),
                            &self.backup_pass
                        )
                        .secure(!self.backup_show)
                        .on_input(Msg::BackupPass)
                        .on_submit(Msg::SaveBackup)
                        .width(Length::Fill),
                        ui::button_small(if self.backup_show {
                            tr("Hide", "Verbergen")
                        } else {
                            tr("Show", "Zeigen")
                        })
                        .style(btn::ghost)
                        .on_press(Msg::BackupShow(!self.backup_show)),
                        ui::button_small(tr("Generate", "Erzeugen"))
                            .style(btn::secondary)
                            .on_press(Msg::BackupGenerate),
                        ui::button_text(tr("Save backup…", "Sicherung speichern …"))
                            .style(btn::primary)
                            .on_press_maybe(pass_ok.then_some(Msg::SaveBackup)),
                    ]
                    .spacing(space::S)
                    .align_y(Alignment::Center),
                    Some(strength_text(&self.backup_pass)),
                ),
            ),
            self.recovery_section(),
        ]
        .spacing(space::L)
        .into()
    }

    /// Wiederherstellungsschluessel einrichten und im Notfall den Kontoschluessel ersetzen (M-7).
    fn recovery_section(&self) -> Element<'_, Msg> {
        let body: Element<'_, Msg> = if let Some((k, rotated)) = &self.recovery_new {
            let mut c = column![
                ui::notice(
                    Tone::Warning,
                    tr(
                        "Write this key down or store it in a password manager, not on this computer. It is shown only once. Without it you can't recover your account if this device is stolen.",
                        "Schreib diesen Schlüssel auf oder leg ihn in einen Passwort-Manager, nicht auf diesem Computer. Er wird nur einmal angezeigt. Ohne ihn kannst du dein Konto nicht retten, wenn dieses Gerät gestohlen wird.",
                    ),
                ),
                ui::input("", &k.text).font(iced::Font::MONOSPACE),
                row![
                    ui::button_small(tr("Copy", "Kopieren"))
                        .style(btn::secondary)
                        .on_press(Msg::RecoveryCopy),
                    Space::new().width(Length::Fill),
                    ui::button_text(if *rotated {
                        tr("I saved it, restart", "Gesichert, neu starten")
                    } else {
                        tr("I saved it", "Gesichert")
                    })
                    .style(btn::primary)
                    .on_press(Msg::RecoveryStored),
                ]
                .spacing(space::S)
                .align_y(Alignment::Center),
            ]
            .spacing(space::M);
            if *rotated {
                c = c.push(ui::caption(tr(
                    "Your account key has been replaced. This is your new recovery key; the old one no longer works.",
                    "Dein Kontoschlüssel wurde ersetzt. Das ist dein neuer Wiederherstellungsschlüssel; der alte gilt nicht mehr.",
                )));
            }
            c.into()
        } else if !self.recovery_set {
            row![
                ui::caption(tr(
                    "Not set up yet. Set it up while your account is safe: friends only accept the first key they see.",
                    "Noch nicht eingerichtet. Richte ihn ein, solange dein Konto sicher ist: Freunde merken sich nur den ersten Schlüssel, den sie sehen.",
                ))
                .width(Length::Fill),
                ui::button_text(tr("Create recovery key", "Schlüssel erstellen"))
                    .style(btn::primary)
                    .on_press(Msg::RecoveryCreate),
            ]
            .spacing(space::S)
            .align_y(Alignment::Center)
            .into()
        } else if !self.rotate_open {
            row![
                ui::caption(tr(
                    "Recovery key is set up.",
                    "Wiederherstellungsschlüssel ist eingerichtet."
                ))
                .width(Length::Fill),
                ui::button_small(tr("Replace account key…", "Kontoschlüssel ersetzen …"))
                    .style(btn::danger_ghost)
                    .on_press(Msg::RotateOpen(true)),
            ]
            .spacing(space::S)
            .align_y(Alignment::Center)
            .into()
        } else {
            let ready = app_proto::recovery::decode_secret(&self.rotate_input).is_some();
            let action: Element<'_, Msg> = if self.rotate_confirm {
                row![
                    Space::new().width(Length::Fill),
                    ui::button_small(tr("Cancel", "Abbrechen"))
                        .style(btn::ghost)
                        .on_press(Msg::RotateConfirm(false)),
                    ui::button_text(tr("Replace key now", "Schlüssel jetzt ersetzen"))
                        .style(btn::danger)
                        .on_press(Msg::Rotate),
                ]
                .spacing(space::S)
                .into()
            } else {
                row![
                    Space::new().width(Length::Fill),
                    ui::button_small(tr("Close", "Schließen"))
                        .style(btn::ghost)
                        .on_press(Msg::RotateOpen(false)),
                    ui::button_text(tr("Replace key…", "Schlüssel ersetzen …"))
                        .style(btn::danger)
                        .on_press_maybe(ready.then_some(Msg::RotateConfirm(true))),
                ]
                .spacing(space::S)
                .into()
            };
            column![
                ui::notice(
                    Tone::Danger,
                    tr(
                        "Only if a device with your account was stolen or your backup leaked. Campfire creates a new account key and tells your friends; they switch to it automatically and block the old one. Your spaces of the old key disappear from this device: ask a member to invite you again. Old backups become useless, make a new one afterwards.",
                        "Nur wenn ein Gerät mit deinem Konto gestohlen wurde oder deine Sicherung in fremde Hände kam. Campfire erzeugt einen neuen Kontoschlüssel und meldet ihn deinen Freunden; sie wechseln automatisch und sperren den alten. Deine Spaces des alten Schlüssels verschwinden von diesem Gerät: lass dich von einem Mitglied neu einladen. Alte Sicherungen werden wertlos, mach danach eine neue.",
                    ),
                ),
                ui::field(
                    tr("Recovery key", "Wiederherstellungsschlüssel"),
                    ui::input("CFR-…", &self.rotate_input)
                        .secure(true)
                        .on_input(Msg::RotateInput)
                        .font(iced::Font::MONOSPACE),
                    None,
                ),
                action,
            ]
            .spacing(space::M)
            .into()
        };
        ui::section(
            tr("Recovery key", "Wiederherstellungsschlüssel"),
            Some(tr(
                "A second key that never stays on your devices. If one is stolen, you can use it to move your account to a new key that your friends accept.",
                "Ein zweiter Schlüssel, der nie auf deinen Geräten bleibt. Wird eins gestohlen, ziehst du damit dein Konto auf einen neuen Schlüssel um, den deine Freunde übernehmen.",
            )),
            body,
        )
    }

    fn audio<'a>(&'a self, core: &'a Core, sp: &'a spaces::State) -> Element<'a, Msg> {
        let cfg = core.io_config();
        let sel =
            |o: &Option<String>| Some(o.clone().unwrap_or_else(|| default_device().to_owned()));
        let kbps = core.bitrate();
        column![
            ui::section(
                tr("Devices", "Geräte"),
                None,
                column![
                    ui::field(
                        tr("Microphone", "Mikrofon"),
                        ui::select(devices(&self.inputs), sel(&cfg.input), Msg::DeviceIn)
                            .width(Length::Fill),
                        None
                    ),
                    ui::field(
                        tr("Speakers / headphones", "Lautsprecher / Kopfhörer"),
                        ui::select(devices(&self.outputs), sel(&cfg.output), Msg::DeviceOut)
                            .width(Length::Fill),
                        None
                    ),
                    row![
                        ui::caption(tr(
                            "Device missing? Rescan after plugging it in.",
                            "Gerät fehlt? Nach dem Einstecken neu einlesen."
                        ))
                        .width(Length::Fill),
                        ui::button_small(tr("Rescan devices", "Geräte neu einlesen"))
                            .style(btn::secondary)
                            .on_press(Msg::Refresh),
                    ]
                    .spacing(space::S)
                    .align_y(Alignment::Center),
                ]
                .spacing(space::L),
            ),
            ui::section(
                tr("Voice quality", "Sprachqualität"),
                Some(tr(
                    "A higher bitrate sounds better and needs more bandwidth.",
                    "Höhere Bitrate klingt besser und braucht mehr Leitung."
                )),
                ui::slider_field(
                    "Bitrate",
                    format!("{kbps} kbit/s"),
                    ui::slider(
                        app_audio::voice::MIN_KBPS as f32..=app_audio::voice::MAX_KBPS as f32,
                        kbps as f32,
                        Msg::Bitrate
                    ),
                ),
            ),
            ui::section(
                tr("Microphone", "Mikrofon"),
                None,
                sp.audio_panel(core).map(Msg::Sp)
            ),
        ]
        .spacing(space::L)
        .into()
    }

    fn network<'a>(&'a self, core: &'a Core, friends: &'a [Friend]) -> Element<'a, Msg> {
        let r = self.relay.clone().unwrap_or_else(|| core.relay());
        let mode_btn = |label: &'static str, m: u8| {
            ui::button_small(label)
                .style(btn::chip(r.mode == m))
                .on_press(Msg::RelayMode(m))
        };
        let custom = matches!(r.mode, 1 | 3);
        let mut relay = column![row![
            mode_btn(tr("Default", "Standard"), 0),
            ui::button_small(tr("Custom address", "Eigene Adresse"))
                .style(btn::chip(custom))
                .on_press(Msg::RelayMode(1)),
            mode_btn(
                tr("Relay only (hides your IP)", "Nur Relay (versteckt die IP)"),
                2
            ),
        ]
        .spacing(6)
        .wrap()]
        .spacing(space::M);
        if custom {
            relay = relay.push(ui::field(
                tr("Address", "Adresse"),
                ui::input("https://relay.example.org", &r.url).on_input(Msg::RelayUrl),
                None,
            ));
            relay = relay.push(
                ui::button_small(tr(
                    "Only via this relay (hides your IP)",
                    "Nur über dieses Relay (versteckt die IP)",
                ))
                .style(btn::chip(r.mode == 3))
                .on_press(Msg::RelayMode(if r.mode == 3 { 1 } else { 3 })),
            );
        }
        relay = relay.push(ui::caption(tr(
            "Without \"Relay only\", friends can see your IP address while you are connected (direct connections are faster). Your friend code never reveals it: friend requests always go through a relay.",
            "Ohne \"Nur Relay\" sehen Freunde deine IP-Adresse, solange ihr verbunden seid (direkte Verbindungen sind schneller). Dein Freundescode verrät sie nie: Freundschaftsanfragen laufen immer über ein Relay.",
        )));
        relay = relay.push(ui::caption(tr(
            "With the default setting, the relay and discovery servers of n0 (the network library) see your device ID, IP address and when you are online, but never content. Your own relay avoids that.",
            "Mit der Standardeinstellung sehen die Relay- und Discovery-Server von n0 (der Netzwerk-Bibliothek) deine Geräte-ID, IP-Adresse und wann du online bist, aber nie Inhalte. Ein eigenes Relay vermeidet das.",
        )));
        relay = relay.push(
            row![
                ui::caption(tr(
                    "Changes apply after a restart.",
                    "Änderungen gelten nach einem Neustart."
                ))
                .width(Length::Fill),
                ui::button_small(tr("Apply and restart", "Übernehmen und neu starten"))
                    .style(btn::primary)
                    .on_press_maybe(self.relay.is_some().then_some(Msg::ApplyRelay)),
            ]
            .spacing(space::S)
            .align_y(Alignment::Center),
        );
        let peers = core.net.peers();
        let mut list = column![].spacing(2);
        let mut any = false;
        for f in friends
            .iter()
            .filter(|f| f.status == FriendStatus::Accepted)
        {
            any = true;
            let devs: Vec<_> = peers.iter().filter(|p| p.user == f.user).collect();
            let kinds: Vec<PathKind> = devs.iter().map(|p| core.net.path_kind(&p.device)).collect();
            let status = if kinds.is_empty() {
                tr("not connected", "nicht verbunden").to_owned()
            } else {
                kinds
                    .iter()
                    .map(|k| match k {
                        PathKind::Direct => tr("direct", "direkt"),
                        PathKind::Relay => tr("via relay", "über Relay"),
                        PathKind::None => tr("connecting", "wird aufgebaut"),
                    })
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            let style: fn(&iced::Theme) -> text::Style = if kinds.contains(&PathKind::Direct) {
                ui::tx::success
            } else if kinds.is_empty() {
                ui::tx::faint
            } else {
                ui::tx::muted
            };
            list = list.push(
                container(
                    row![
                        ui::avatar(&f.name, &f.user, 28.0),
                        text(f.name.as_str())
                            .size(size::BODY)
                            .font(ui::SEMIBOLD)
                            .width(Length::Fill),
                        text(status).size(size::SMALL).style(style),
                    ]
                    .spacing(space::M)
                    .align_y(Alignment::Center),
                )
                .padding([6, 0]),
            );
        }
        if !any {
            list = list.push(ui::muted(tr("No friends yet.", "Noch keine Freunde.")));
        }
        column![
            ui::section(
                tr("Relay server", "Relay-Server"),
                Some(tr(
                    "Mediates when no direct connection works.",
                    "Vermittelt, wenn keine direkte Verbindung klappt."
                )),
                relay
            ),
            ui::section(
                tr("Connections to friends", "Verbindungen zu Freunden"),
                None,
                list
            ),
        ]
        .spacing(space::L)
        .into()
    }
}

/// Farbe eines Hinweises anhand seines Inhalts.
fn info_tone(s: &str) -> Tone {
    let l = s.to_lowercase();
    let any = |keys: &[&str]| keys.iter().any(|k| l.contains(k));
    if any(&[
        "fehl",
        "nicht möglich",
        "darf nicht",
        "fail",
        "error",
        "not possible",
        "must not",
    ]) {
        Tone::Danger
    } else if any(&["braucht", "muss", "needs", "must"]) {
        Tone::Warning
    } else if any(&["gespeichert", "saved"]) {
        Tone::Success
    } else {
        Tone::Info
    }
}

/// Knopf fuer das Update: erst nach Klick herunterladen, erst nach Klick installieren.
fn update_action<'a>() -> Element<'a, Msg> {
    use crate::updater::Status;
    match crate::updater::status() {
        Status::Available(v) => ui::setting(
            app_core::tr!("Update to {v}", "Update auf {v}"),
            Some(
                tr(
                    "Updates are only downloaded and installed when you confirm.",
                    "Updates werden nur heruntergeladen und installiert, wenn du zustimmst.",
                )
                .to_owned(),
            ),
            ui::button_small(tr("Download", "Herunterladen"))
                .style(btn::primary)
                .on_press(Msg::UpdateDownload),
        ),
        Status::Ready(v) => ui::setting(
            app_core::tr!("Install {v}", "{v} installieren"),
            Some(
                tr(
                    "Campfire closes, installs the update and starts again.",
                    "Campfire schließt sich, installiert das Update und startet neu.",
                )
                .to_owned(),
            ),
            ui::button_small(tr("Restart and install", "Neu starten und installieren"))
                .style(btn::primary)
                .on_press(Msg::UpdateInstall),
        ),
        _ => iced::widget::Space::new().into(),
    }
}
