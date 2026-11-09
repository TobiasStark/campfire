//! Einstellungsseite (M1-16): Konto, Audio, Tasten, Overlay, Netzwerk, Allgemein.
//! Die Bereiche fuer Audio, Tasten und Overlay stammen aus `spaces.rs` und werden hier eingebunden.

use crate::core::{Core, RelaySetting};
use crate::spaces;
use crate::ui::{self, btn, size, space, Tone};
use app_audio::io::{list_inputs, list_outputs, IoConfig};
use app_core::friends::{Friend, FriendStatus};
use app_core::identity::{
    mark_backup_done, write_backup_file, Dpapi, Identity, MIN_PASSPHRASE_LEN,
};
use app_core::settings::ThemeChoice;
use app_net::PathKind;
use app_video::select::{
    choose, AudioMode, Backend, Codec, CodecChoice, EncoderChoice, EncoderInfo, Resolution,
};
use iced::widget::{button, column, container, row, text, Space};
use iced::{Alignment, Element, Length, Task};

const DEFAULT_DEVICE: &str = "(Standard)";

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
    SaveBackup,
    BackupSaved(Result<String, String>),
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
    ToggleTheme,
    /// Wird von der App behandelt (Farben gehoeren ihr).
    Colors(crate::colors_ui::ColorMsg),
    /// Grenze fuer automatische Datei-Downloads in MB (0 = nie).
    AutoMb(f32),
    /// Groesse des Datei-Speichers in GB (0 = unbegrenzt).
    CacheGb(f32),
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
    backup_pass: String,
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

fn devices(list: &[String]) -> Vec<String> {
    let mut v = vec![DEFAULT_DEVICE.to_owned()];
    v.extend(list.iter().cloned());
    v
}

impl State {
    /// Beim Oeffnen: Geraetelisten und gespeicherte Werte frisch laden.
    pub fn open(&mut self) {
        self.inputs = list_inputs();
        self.outputs = list_outputs();
        self.name = None;
        self.relay = None;
        self.info = None;
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
                    self.info = Some("Der Name darf nicht leer sein.".into());
                    return Task::none();
                }
                let db = core.db.clone();
                return Task::perform(
                    async move {
                        tokio::task::spawn_blocking(move || {
                            db.call(move |c| {
                                Ok(Identity::load(c, &Dpapi).map_err(err).and_then(|id| {
                                    let mut id = id.ok_or("Keine Identitaet")?;
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
                    Ok(()) => {
                        "Gespeichert. Deine Freunde sehen den neuen Namen ab dem nächsten Start."
                            .into()
                    }
                    Err(e) => format!("Fehler: {e}"),
                });
            }
            Msg::ExportDiag => {
                let extra = format!(
                    "Update: {}
Relay: {:?}",
                    crate::updater::describe(),
                    core.relay().mode
                );
                self.info = Some(match crate::diag::export(&extra) {
                    Ok(p) => format!("Diagnose gespeichert: {}", p.display()),
                    Err(e) => format!("Diagnose fehlgeschlagen: {e}"),
                });
            }
            Msg::BackupPass(s) => self.backup_pass = s,
            Msg::SaveBackup => {
                let pass = self.backup_pass.clone();
                if pass.chars().count() < MIN_PASSPHRASE_LEN {
                    self.info = Some(format!(
                        "Das Passwort braucht mindestens {MIN_PASSPHRASE_LEN} Zeichen."
                    ));
                    return Task::none();
                }
                let db = core.db.clone();
                return Task::perform(
                    async move {
                        tokio::task::spawn_blocking(move || {
                            let id = db
                                .call(|c| Ok(Identity::load(c, &Dpapi).map_err(err)))
                                .map_err(err)??
                                .ok_or("Keine Identitaet")?;
                            let data = id.export_backup(&pass).map_err(err)?;
                            let path = write_backup_file(&crate::app::documents_dir(), &data)
                                .map_err(err)?;
                            let _ = db.call(|c| Ok(mark_backup_done(c)));
                            Ok(path.display().to_string())
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
                        self.backup_pass.clear();
                        format!("Backup gespeichert: {p}")
                    }
                    Err(e) => format!("Backup fehlgeschlagen: {e}"),
                });
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
                    if r.mode == 1 && !r.url.trim().starts_with("http") {
                        self.info = Some("Die Relay-Adresse muss mit https:// beginnen.".into());
                        return Task::none();
                    }
                    core.set_relay(r);
                    if crate::platform::restart_app() {
                        crate::signals::send(crate::signals::Signal::Quit);
                    } else {
                        self.info = Some(
                            "Neustart nicht möglich, bitte Campfire selbst neu starten.".into(),
                        );
                    }
                }
            }
            Msg::Refresh => {
                self.inputs = list_inputs();
                self.outputs = list_outputs();
            }
            Msg::DeviceIn(d) => {
                let mut c = core.io_config();
                c.input = (d != DEFAULT_DEVICE).then_some(d);
                core.set_io_config(c);
            }
            Msg::DeviceOut(d) => {
                let mut c = core.io_config();
                c.output = (d != DEFAULT_DEVICE).then_some(d);
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
            Msg::AutoMb(v) => core.files.set_auto_mb(v.round() as u64),
            Msg::CacheGb(v) => core.files.set_cache_mb(v.round() as u64 * 1024),
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
                    self.info =
                        Some("Zurücksetzen wird mit „Übernehmen“ wirksam (Neustart).".into());
                }
                Tab::Account | Tab::General | Tab::Colors => {}
            },
            Msg::Close | Msg::Sp(_) | Msg::ToggleAutostart | Msg::ToggleTheme | Msg::Colors(_) => {}
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
        let nav = container(
            ui::scroll(
                column![
                    group("Benutzer"),
                    tab_btn("Konto", Tab::Account),
                    group("Sprache und Video"),
                    tab_btn("Audio", Tab::Audio),
                    tab_btn("Tastenkürzel", Tab::Keys),
                    tab_btn("Overlay", Tab::Overlay),
                    tab_btn("Töne", Tab::Sounds),
                    tab_btn("Video", Tab::Video),
                    group("App"),
                    tab_btn("Farben", Tab::Colors),
                    tab_btn("Netzwerk", Tab::Network),
                    tab_btn("Allgemein", Tab::General),
                ]
                .spacing(2)
                .padding([space::S, space::M]),
            )
            .height(Length::Fill),
        )
        .width(210)
        .height(Length::Fill)
        .style(|t: &iced::Theme| iced::widget::container::Style {
            background: Some(iced::Background::Color(ui::tokens(t).sidebar)),
            ..Default::default()
        });

        let (title, intro) = match self.tab {
            Tab::Account => ("Konto", "Name, Freundescode und Sicherung deines Kontos."),
            Tab::Audio => ("Audio", "Mikrofon, Lautsprecher und Sprachqualität."),
            Tab::Keys => (
                "Tastenkürzel",
                "Globale Tasten, die auch im Spiel funktionieren.",
            ),
            Tab::Overlay => ("Overlay", "Zeigt im Spiel, wer gerade spricht."),
            Tab::Sounds => ("Töne", "Kurze Hinweistöne bei Ereignissen."),
            Tab::Video => ("Video", "Bildschirm-Übertragung und Encoder."),
            Tab::Network => ("Netzwerk", "Wie Campfire deine Freunde erreicht."),
            Tab::Colors => (
                "Farben",
                "Jede Farbe der Oberfläche lässt sich selbst einstellen.",
            ),
            Tab::General => ("Allgemein", "Start, Aussehen, Dateien und Diagnose."),
        };
        let body: Element<'a, Msg> = match self.tab {
            Tab::Account => self.account(core),
            Tab::Audio => self.audio(core, sp),
            Tab::Keys => ui::section("Tasten", None, sp.hotkey_panel(core).map(Msg::Sp)),
            Tab::Overlay => ui::section("Anzeige", None, sp.overlay_panel(core).map(Msg::Sp)),
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
                    ui::button_small("Auf Standard zurücksetzen")
                        .style(btn::danger_ghost)
                        .on_press(Msg::Reset),
                ]
                .align_y(Alignment::Center),
            );
        }
        column![
            ui::header(
                text("Einstellungen")
                    .size(size::H3)
                    .font(ui::SEMIBOLD)
                    .style(ui::tx::normal),
                ui::button_small("Schließen")
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
                "Variante",
                Some(
                    "Dunkel und hell haben getrennte Farben. Bearbeitet wird die aktive Variante."
                ),
                row![
                    variant("Dunkel", ThemeChoice::Dark),
                    variant("Hell", ThemeChoice::Light)
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
        let auto_mb = core.files.auto_mb();
        let cache_gb = core.files.cache_mb() / 1024;
        column![
            ui::section(
                "Start und Aussehen",
                None,
                column![
                    ui::setting(
                        "Mit Windows starten",
                        Some("Campfire startet beim Anmelden im Hintergrund.".to_owned()),
                        ui::toggle(autostart).on_toggle(|_| Msg::ToggleAutostart),
                    ),
                    ui::setting(
                        "Design",
                        None,
                        row![
                            ui::button_small("Dunkel")
                                .style(btn::chip(theme == ThemeChoice::Dark))
                                .on_press_maybe(
                                    (theme != ThemeChoice::Dark).then_some(Msg::ToggleTheme)
                                ),
                            ui::button_small("Hell")
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
                "Dateien",
                None,
                column![
                    ui::slider_field(
                        "Automatisch herunterladen",
                        if auto_mb == 0 {
                            "nie (alles per Klick)".to_owned()
                        } else {
                            format!("bis {auto_mb} MB")
                        },
                        ui::slider(0.0..=1000.0, auto_mb as f32, Msg::AutoMb).step(10.0),
                    ),
                    ui::caption("Größere Dateien lädst du per Klick."),
                    ui::slider_field(
                        "Datei-Speicher",
                        if cache_gb == 0 {
                            "unbegrenzt".to_owned()
                        } else {
                            format!("höchstens {cache_gb} GB")
                        },
                        ui::slider(0.0..=100.0, cache_gb as f32, Msg::CacheGb).step(1.0),
                    ),
                    ui::caption(format!(
                        "Belegt: {}. Ist der Speicher voll, werden die ältesten nicht angepinnten Dateien entfernt.",
                        app_core::files::format_size(core.files.used())
                    )),
                ]
                .spacing(space::S),
            ),
            ui::section(
                "Über Campfire",
                None,
                column![
                    ui::muted(crate::updater::describe()),
                    ui::setting(
                        "Diagnose",
                        Some("Speichert Protokolle für die Fehlersuche als Datei.".to_owned()),
                        ui::button_small("Exportieren")
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
            None => ("Suche funktionierende Encoder …".to_owned(), Tone::Info),
            Some(l) if l.is_empty() && !crate::video::built_in() => (
                "Dieser Build enthält keine Video-Unterstützung.".to_owned(),
                Tone::Warning,
            ),
            Some(l) if l.is_empty() => (
                "Kein funktionierender Encoder gefunden.".to_owned(),
                Tone::Warning,
            ),
            Some(l) => (
                format!(
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
            Some(e) => format!("Wird verwendet: {} ({})", e.name, e.backend.label()),
            None => "Wird verwendet: nichts".to_owned(),
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
                "Qualität",
                None,
                column![
                    row![
                        ui::field(
                            "Auflösung",
                            ui::select(
                                Resolution::ALL.map(|r| r.label().to_owned()).to_vec(),
                                Some(o.resolution.label().to_owned()),
                                |l| Msg::Vid(VidChange::Resolution(l))
                            )
                            .width(Length::Fill),
                            None
                        ),
                        ui::field(
                            "Bilder pro Sekunde",
                            row![fps_btn(30), fps_btn(60)].spacing(6),
                            None
                        ),
                    ]
                    .spacing(space::L),
                    ui::slider_field(
                        "Höchste Bitrate",
                        format!("{} Mbit/s", o.max_mbps),
                        ui::slider(1.0..=50.0, o.max_mbps as f32, |v| Msg::Vid(
                            VidChange::Mbps(v)
                        ))
                        .step(1.0),
                    ),
                    ui::setting(
                        "Zweite, leichtere Fassung",
                        Some("Zusätzlich 720p30 für Zuschauer mit schwacher Leitung.".to_owned()),
                        ui::toggle(o.simulcast).on_toggle(|v| Msg::Vid(VidChange::Simulcast(v))),
                    ),
                ]
                .spacing(space::L),
            ),
            ui::section(
                "Ton im Stream",
                Some("Der Voice-Chat ist nie im Stream. Spielsound braucht Windows 11."),
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
                "Probehören",
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
                "Lautstärke",
                None,
                ui::slider_field(
                    "Hinweistöne",
                    format!("{:.0} %", s.volume * 100.0),
                    ui::slider(0.0..=1.0, s.volume, |v| Msg::Snd(SndChange::Volume(v))).step(0.05),
                ),
            ),
            ui::section(
                "Ereignisse",
                Some("Mit den Knöpfen hörst du den Ton probe."),
                column![
                    line(
                        "Beitreten / Verlassen",
                        s.join_leave,
                        SndChange::JoinLeave,
                        vec![
                            test("Beitreten", Sound::Join),
                            test("Verlassen", Sound::Leave)
                        ]
                    ),
                    ui::divider(),
                    line(
                        "Stumm",
                        s.mute,
                        SndChange::Mute,
                        vec![test("an", Sound::Mute), test("aus", Sound::Unmute)]
                    ),
                    ui::divider(),
                    line(
                        "Taub",
                        s.deafen,
                        SndChange::Deafen,
                        vec![test("an", Sound::Deafen), test("aus", Sound::Undeafen)]
                    ),
                    ui::divider(),
                    line(
                        "Push-to-Talk",
                        s.ptt,
                        SndChange::Ptt,
                        vec![test("an", Sound::PttOn), test("aus", Sound::PttOff)]
                    ),
                    ui::divider(),
                    line(
                        "Eingehender Anruf",
                        s.call,
                        SndChange::Call,
                        vec![test("Klingeln", Sound::Ring)]
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
        let pass_ok = self.backup_pass.chars().count() >= MIN_PASSPHRASE_LEN;
        column![
            ui::section(
                "Profil",
                None,
                column![
                    row![
                        ui::avatar(name, &core.user, 56.0),
                        column![
                            text(name.to_owned()).size(size::H3).font(ui::SEMIBOLD),
                            ui::caption("So sehen dich deine Freunde."),
                        ]
                        .spacing(2),
                    ]
                    .spacing(space::L)
                    .align_y(Alignment::Center),
                    ui::field(
                        "Anzeigename",
                        row![
                            ui::input("Name", name)
                                .on_input(Msg::NameInput)
                                .on_submit(Msg::SaveName)
                                .width(Length::Fill),
                            ui::button_text("Speichern")
                                .style(btn::primary)
                                .on_press_maybe(changed.then_some(Msg::SaveName)),
                        ]
                        .spacing(space::S),
                        Some("Deine Freunde sehen den neuen Namen ab dem nächsten Start.".to_owned()),
                    ),
                ]
                .spacing(space::L),
            ),
            ui::section(
                "Freundescode",
                Some("Gib ihn Freunden, damit sie dich hinzufügen können."),
                ui::input("", &core.my_code).font(iced::Font::MONOSPACE),
            ),
            ui::section(
                "Sicherung",
                Some("Speichert dein Konto verschlüsselt als Datei in „Dokumente“. Ohne Sicherung ist dein Konto weg, wenn der Computer kaputtgeht."),
                ui::field(
                    "Passwort für die Sicherung",
                    row![
                        ui::input(
                            &format!("mindestens {MIN_PASSPHRASE_LEN} Zeichen"),
                            &self.backup_pass
                        )
                        .secure(true)
                        .on_input(Msg::BackupPass)
                        .on_submit(Msg::SaveBackup)
                        .width(Length::Fill),
                        ui::button_text("Sicherung speichern")
                            .style(btn::primary)
                            .on_press_maybe(pass_ok.then_some(Msg::SaveBackup)),
                    ]
                    .spacing(space::S),
                    Some(format!(
                        "{} von mindestens {MIN_PASSPHRASE_LEN} Zeichen. Ohne dieses Passwort lässt sich die Sicherung nicht öffnen.",
                        self.backup_pass.chars().count()
                    )),
                ),
            ),
        ]
        .spacing(space::L)
        .into()
    }

    fn audio<'a>(&'a self, core: &'a Core, sp: &'a spaces::State) -> Element<'a, Msg> {
        let cfg = core.io_config();
        let sel = |o: &Option<String>| Some(o.clone().unwrap_or_else(|| DEFAULT_DEVICE.to_owned()));
        let kbps = core.bitrate();
        column![
            ui::section(
                "Geräte",
                None,
                column![
                    ui::field(
                        "Mikrofon",
                        ui::select(devices(&self.inputs), sel(&cfg.input), Msg::DeviceIn)
                            .width(Length::Fill),
                        None
                    ),
                    ui::field(
                        "Lautsprecher / Kopfhörer",
                        ui::select(devices(&self.outputs), sel(&cfg.output), Msg::DeviceOut)
                            .width(Length::Fill),
                        None
                    ),
                    row![
                        ui::caption("Gerät fehlt? Nach dem Einstecken neu einlesen.")
                            .width(Length::Fill),
                        ui::button_small("Geräte neu einlesen")
                            .style(btn::secondary)
                            .on_press(Msg::Refresh),
                    ]
                    .spacing(space::S)
                    .align_y(Alignment::Center),
                ]
                .spacing(space::L),
            ),
            ui::section(
                "Sprachqualität",
                Some("Höhere Bitrate klingt besser und braucht mehr Leitung."),
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
            ui::section("Mikrofon", None, sp.audio_panel(core).map(Msg::Sp)),
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
        let mut relay = column![row![
            mode_btn("Standard", 0),
            mode_btn("Eigene Adresse", 1),
            mode_btn("Nur Relay (versteckt die IP)", 2),
        ]
        .spacing(6)
        .wrap()]
        .spacing(space::M);
        if r.mode == 1 {
            relay = relay.push(ui::field(
                "Adresse",
                ui::input("https://relay.example.org", &r.url).on_input(Msg::RelayUrl),
                None,
            ));
        }
        relay = relay.push(
            row![
                ui::caption("Änderungen gelten nach einem Neustart.").width(Length::Fill),
                ui::button_small("Übernehmen und neu starten")
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
                "nicht verbunden".to_owned()
            } else {
                kinds
                    .iter()
                    .map(|k| match k {
                        PathKind::Direct => "direkt",
                        PathKind::Relay => "über Relay",
                        PathKind::None => "wird aufgebaut",
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
            list = list.push(ui::muted("Noch keine Freunde."));
        }
        column![
            ui::section(
                "Relay-Server",
                Some("Vermittelt, wenn keine direkte Verbindung klappt."),
                relay
            ),
            ui::section("Verbindungen zu Freunden", None, list),
        ]
        .spacing(space::L)
        .into()
    }
}

/// Farbe eines Hinweises anhand seines Inhalts.
fn info_tone(s: &str) -> Tone {
    let l = s.to_lowercase();
    if l.contains("fehl") || l.contains("nicht möglich") || l.contains("darf nicht") {
        Tone::Danger
    } else if l.contains("braucht") || l.contains("muss") {
        Tone::Warning
    } else if l.contains("gespeichert") {
        Tone::Success
    } else {
        Tone::Info
    }
}
