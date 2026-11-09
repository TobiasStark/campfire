//! Oberflaeche fuer Streams (M3-06): Angebote in der Seitenleiste und die Zuschaueransicht
//! im Hauptfenster, als Vollbild und als eigenes Fenster.

use crate::stream::{AvailableStream, StreamService, ViewStats, WatchInfo, WatchState};
use crate::ui::{self, btn, size, space};
use app_core::i18n::tr;
use app_net::PathKind;
use app_proto::video::{RefuseReason, VideoCodec};
use app_proto::PublicKey;
use iced::widget::{button, column, container, row, stack, text, tooltip, Space};
use iced::{window, Alignment, Element, Length, Theme};

#[derive(Debug, Clone)]
pub enum Msg {
    Watch(PublicKey),
    StopWatching,
    /// Stream im Hauptbereich zeigen.
    ShowStream,
    Volume(f32),
    ToggleStats,
    Fullscreen(bool),
    Popout,
    PopoutOpened(window::Id),
    ClosePopout,
    StopStream,
    /// Neues Bild: nur zum Neuzeichnen.
    Frame,
}

#[derive(Default)]
pub struct State {
    pub show: bool,
    pub stats: bool,
    pub fullscreen: bool,
    pub popout: Option<window::Id>,
    pub popout_opening: bool,
}

impl State {
    /// Wird der Stream gerade irgendwo gezeigt (dann braucht die Anzeige neue Bilder)?
    pub fn visible(&self) -> bool {
        self.show || self.fullscreen || self.popout.is_some()
    }
}

/// Wo die Ansicht gezeichnet wird.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Place {
    Main,
    Fullscreen,
    Popout,
}

fn quality_label(a: &AvailableStream) -> String {
    app_core::tr!(
        "{} is streaming ({}){}",
        "{} streamt ({}){}",
        a.name,
        a.quality,
        if a.audio {
            tr(", with audio", ", mit Ton")
        } else {
            ""
        }
    )
}

/// Seitenleiste: eigene Streams und Angebote anderer im Voice-Channel.
pub fn panel<'a>(stream: &StreamService, st: &State) -> Element<'a, Msg> {
    let mut col = column![].spacing(space::S);
    let mut any = false;
    if let Some(o) = stream.outgoing_info() {
        any = true;
        let mut own = column![row![
            ui::tag("LIVE", ui::bg::badge_live),
            text(tr("You are streaming", "Du streamst"))
                .size(size::SMALL)
                .font(ui::SEMIBOLD),
        ]
        .spacing(space::S)
        .align_y(Alignment::Center)]
        .spacing(space::XS);
        own = own.push(match &o.error {
            Some(e) => text(app_core::tr!("Error: {e}", "Fehler: {e}"))
                .size(size::CAPTION)
                .style(ui::tx::danger),
            None => ui::caption(app_core::tr!(
                "{} viewers · {:.1} Mbit/s{}",
                "{} Zuschauer · {:.1} Mbit/s{}",
                o.viewers,
                f64::from(o.kbps) / 1000.0,
                if o.layers > 1 {
                    tr(" · 2 layers", " · 2 Stufen")
                } else {
                    ""
                }
            )),
        });
        if !o.watchers.is_empty() {
            own = own.push(ui::caption(app_core::tr!(
                "Watching: {}",
                "Schauen zu: {}",
                o.watchers.join(", ")
            )));
        }
        if let Some(n) = &o.note {
            own = own.push(ui::caption(n.clone()));
        }
        own = own.push(
            ui::button_small(tr("Stop stream", "Stream beenden"))
                .width(Length::Fill)
                .style(btn::danger)
                .on_press(Msg::StopStream),
        );
        col = col.push(own);
    }
    let watching = stream.watching();
    for a in stream.available() {
        if watching.as_ref().is_some_and(|w| w.device == a.device) {
            continue;
        }
        any = true;
        col = col.push(
            row![
                column![
                    text(app_core::tr!("{} is streaming", "{} streamt", a.name))
                        .size(size::SMALL)
                        .font(ui::SEMIBOLD)
                        .wrapping(text::Wrapping::None),
                    ui::caption(format!(
                        "{}{}",
                        a.quality,
                        if a.audio {
                            tr(" · with audio", " · mit Ton")
                        } else {
                            ""
                        }
                    )),
                ]
                .spacing(2)
                .width(Length::Fill),
                ui::tip(
                    ui::button_small(tr("Watch", "Ansehen"))
                        .style(btn::primary)
                        .on_press(Msg::Watch(a.device)),
                    quality_label(&a),
                    tooltip::Position::Top,
                ),
            ]
            .spacing(space::S)
            .align_y(Alignment::Center),
        );
    }
    if let Some(w) = watching {
        any = true;
        col = col.push(
            row![
                column![
                    ui::caption(tr("You are watching", "Du siehst")),
                    text(w.name.clone())
                        .size(size::SMALL)
                        .font(ui::SEMIBOLD)
                        .wrapping(text::Wrapping::None),
                ]
                .spacing(2)
                .width(Length::Fill),
                ui::button_small(if st.show {
                    tr("Here", "Hier")
                } else {
                    tr("Show", "Zeigen")
                })
                .style(btn::chip(st.show))
                .on_press(Msg::ShowStream),
                ui::tip(
                    button(text("✕").size(size::CAPTION))
                        .padding([4, 7])
                        .style(btn::ghost)
                        .on_press(Msg::StopWatching),
                    tr("Stop watching", "Nicht mehr ansehen"),
                    tooltip::Position::Top,
                ),
            ]
            .spacing(space::XS)
            .align_y(Alignment::Center),
        );
    }
    if !any {
        return Space::new().into();
    }
    container(col)
        .padding(space::M)
        .width(Length::Fill)
        .style(|t: &Theme| container::Style {
            background: Some(ui::tokens(t).surface.into()),
            border: iced::Border::default().width(1).color(ui::tokens(t).border),
            ..Default::default()
        })
        .into()
}

fn refused_text(r: RefuseReason) -> &'static str {
    match r {
        RefuseReason::Capacity => tr(
            "The sender's connection can't support another viewer right now.",
            "Die Leitung des Senders reicht für einen weiteren Zuschauer gerade nicht aus.",
        ),
        RefuseReason::NoCommonCodec => tr(
            "Your computer can't play this stream's video format.",
            "Dein Rechner kann das Videoformat dieses Streams nicht abspielen.",
        ),
        RefuseReason::NotAllowed => tr(
            "You are not allowed to watch this stream.",
            "Du darfst diesen Stream nicht ansehen.",
        ),
        RefuseReason::NoSuchStream => tr(
            "The stream is no longer running.",
            "Der Stream läuft nicht mehr.",
        ),
    }
}

fn path_label(p: PathKind) -> &'static str {
    match p {
        PathKind::Direct => tr("direct", "direkt"),
        PathKind::Relay => "Relay",
        PathKind::None => tr("no path", "kein Pfad"),
    }
}

/// Eine Zeile mit den Messwerten fuer das Overlay.
pub fn stats_line(s: &ViewStats) -> String {
    let codec = match s.codec {
        Some(VideoCodec::H264) => "H.264",
        Some(VideoCodec::Hevc) => "HEVC",
        Some(VideoCodec::Av1) => "AV1",
        None => "?",
    };
    let size = s
        .size
        .map_or_else(String::new, |(w, h)| format!("{w}×{h} · "));
    app_core::tr!(
        "{size}{} fps · {:.1} Mbit/s · {codec} ({}) · layer {} · {} · delay +{} ms · jitter {} ms · loss {} %",
        "{size}{} fps · {:.1} Mbit/s · {codec} ({}) · Stufe {} · {} · Verzögerung +{} ms · Jitter {} ms · Verlust {} %",
        s.fps,
        f64::from(s.kbps) / 1000.0,
        if s.hardware_decode { "GPU" } else { "CPU" },
        s.layer,
        path_label(s.path),
        s.delay_ms,
        s.jitter_ms,
        s.lost_percent,
    )
}

fn centered<'a>(t: &'a str) -> Element<'a, Msg> {
    container(
        text(t)
            .size(size::BODY)
            .color(ui::current().media_fg_dim)
            .align_x(Alignment::Center),
    )
    .center(Length::Fill)
    .padding(space::XL)
    .into()
}

/// Die Zuschaueransicht.
pub fn view<'a>(info: &WatchInfo, volume: f32, st: &State, place: Place) -> Element<'a, Msg> {
    let body: Element<'a, Msg> = match &info.state {
        WatchState::Playing => {
            let video = crate::video_view::view::<Msg>(info.slot.clone());
            if st.stats {
                let overlay = container(
                    text(stats_line(&info.stats))
                        .size(size::CAPTION)
                        .font(iced::Font::MONOSPACE)
                        .color(ui::current().media_fg),
                )
                .padding([6, 10])
                .style(|_| container::Style {
                    background: Some(ui::current().media_scrim.into()),
                    border: iced::Border::default().rounded(ui::radius::M),
                    ..Default::default()
                });
                stack![
                    video,
                    container(overlay)
                        .align_x(Alignment::Start)
                        .padding(space::M)
                ]
                .into()
            } else {
                video
            }
        }
        WatchState::Connecting => centered(tr("Connecting …", "Verbinde …")),
        WatchState::Refused(r) => centered(refused_text(*r)),
        WatchState::Ended => centered(tr("The stream has ended.", "Der Stream ist beendet.")),
        WatchState::Unsupported => centered(tr(
            "This build does not include ffmpeg and can't play streams.",
            "Dieser Build enthält kein ffmpeg und kann Streams nicht abspielen.",
        )),
    };
    let vol = row![
        ui::caption(tr("Volume", "Lautstärke")),
        ui::slider(0.0..=1.0, volume, Msg::Volume)
            .step(0.01)
            .width(160),
        text(format!("{:.0} %", volume * 100.0))
            .size(size::CAPTION)
            .style(ui::tx::muted)
            .width(40),
    ]
    .spacing(space::S)
    .align_y(Alignment::Center);
    let action =
        |label: &'static str, m: Msg| ui::button_small(label).style(btn::ghost).on_press(m);
    let stats = ui::button_small(if st.stats {
        tr("Stats off", "Statistik aus")
    } else {
        tr("Stats", "Statistik")
    })
    .style(btn::chip(st.stats))
    .on_press(Msg::ToggleStats);
    let title = row![
        ui::tag("LIVE", ui::bg::badge_live),
        text(app_core::tr!("{} is streaming", "{} streamt", info.name))
            .size(size::H3)
            .font(ui::SEMIBOLD)
            .style(ui::tx::normal),
    ]
    .spacing(space::S)
    .align_y(Alignment::Center);

    match place {
        Place::Fullscreen => {
            let bar = row![
                title,
                Space::new().width(Length::Fill),
                vol,
                stats,
                ui::button_small(tr("Exit fullscreen", "Vollbild beenden"))
                    .style(btn::secondary)
                    .on_press(Msg::Fullscreen(false)),
            ]
            .spacing(space::M)
            .align_y(Alignment::Center);
            column![
                container(body)
                    .width(Length::Fill)
                    .height(Length::Fill)
                    .style(|_| container::Style {
                        background: Some(ui::current().media_bg.into()),
                        ..Default::default()
                    }),
                container(bar)
                    .padding([space::S, space::L])
                    .width(Length::Fill)
                    .style(ui::bg::sidebar),
            ]
            .into()
        }
        Place::Main | Place::Popout => {
            let mut actions = row![
                stats,
                action(tr("Fullscreen", "Vollbild"), Msg::Fullscreen(true))
            ]
            .spacing(space::XS)
            .align_y(Alignment::Center);
            actions = if place == Place::Popout {
                actions.push(action(
                    tr("Back to main window", "Zurück ins Hauptfenster"),
                    Msg::ClosePopout,
                ))
            } else {
                actions.push(action(tr("Own window", "Eigenes Fenster"), Msg::Popout))
            };
            actions = actions.push(
                ui::button_small(tr("Close", "Schließen"))
                    .style(btn::secondary)
                    .on_press(Msg::StopWatching),
            );
            column![
                ui::header(title, actions),
                container(
                    container(body)
                        .width(Length::Fill)
                        .height(Length::Fill)
                        .clip(true)
                        .style(ui::bg::video)
                )
                .padding([space::M, space::L])
                .height(Length::Fill),
                container(row![Space::new().width(Length::Fill), vol]).padding(
                    iced::Padding::ZERO
                        .left(space::L)
                        .right(space::L)
                        .bottom(space::M)
                ),
            ]
            .height(Length::Fill)
            .into()
        }
    }
}

/// Platzhalter im Hauptfenster, solange der Stream im eigenen Fenster laeuft.
pub fn popped_out<'a>(name: &str) -> Element<'a, Msg> {
    ui::empty_state(
        "⧉",
        app_core::tr!(
            "{name} is playing in its own window",
            "{name} läuft im eigenen Fenster"
        ),
        tr(
            "You can bring the stream back to the main window at any time.",
            "Du kannst den Stream jederzeit zurück ins Hauptfenster holen.",
        ),
        Some(
            ui::button_text(tr("Bring back", "Zurückholen"))
                .style(btn::primary)
                .on_press(Msg::ClosePopout)
                .into(),
        ),
    )
}
