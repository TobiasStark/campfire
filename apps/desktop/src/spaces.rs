//! Spaces: Liste, Channels (Voice), Mitglieder, Einladungen (M1-09).

use crate::core::{Core, PeerPref};
use app_audio::process::InputSettings;
use app_audio::sfx::Sound;
use app_core::eventlog::dm;
use app_core::eventlog::unread::{self, Unread};
use app_core::eventlog::{consent, space_view, SpaceInfo, SpaceView};
use app_core::i18n::tr;
use app_core::notify::{self, Incoming, Mode};
use std::collections::HashMap;

/// Direktnachrichten: Gegenueber -> (Space, Channel).
pub type Dms = HashMap<PublicKey, (SpaceId, Option<ChannelId>)>;

/// Ungelesenes je Channel (alle Spaces) und je Space.
pub type Unreads = (HashMap<ChannelId, Unread>, HashMap<SpaceId, Unread>);
use crate::ui::{self, btn, size, space};
use app_core::friends::{Friend, FriendStatus};
use app_hotkeys::engine::{binding_name, Action};
use app_proto::event::{ChannelId, ChannelKind, EventKind, SpaceId};
use app_proto::PublicKey;
use iced::widget::{button, column, container, row, stack, text, tooltip, Space};
use iced::{Alignment, Element, Length, Task};

/// Ergebnis von `reload`: Spaces, gewaehlte Ansicht, Ungelesenes, neue Nachrichten, DMs,
/// offene Einladungen.
pub type LoadedData = (
    Vec<SpaceInfo>,
    Option<(SpaceId, SpaceView)>,
    Unreads,
    Option<(Vec<Incoming>, i64)>,
    Dms,
    Vec<SpaceInfo>,
);

/// Kennung der scrollbaren Startseite (fuer den Sprung zum Space-Formular).
pub const HOME_PAGE: &str = "home-page";
/// Kennung des Namensfelds fuer neue Spaces.
const NEW_SPACE_INPUT: &str = "new-space-name";

#[derive(Debug, Clone)]
pub enum Msg {
    Loaded(Box<LoadedData>),
    Select(Option<SpaceId>),
    /// Rail-"+": Startseite oeffnen, zum Formular scrollen, Namensfeld fokussieren.
    NewSpace,
    NewSpaceName(String),
    CreateSpace,
    Created(Result<SpaceId, String>),
    NewChannelName(String),
    CreateChannel,
    Edit(ChannelId, String),
    EditText(String),
    SaveEdit,
    CancelEdit,
    Move(ChannelId, i32),
    DeleteChannel(ChannelId),
    /// Loeschen eines Channels erst nach Rueckfrage.
    AskDelete(Option<ChannelId>),
    Invite(PublicKey),
    /// Einladung annehmen (M-2); `Accepted`, sobald gespeichert.
    AcceptInvite(SpaceId),
    Accepted(SpaceId),
    /// Einladung ablehnen = austreten.
    DeclineInvite(SpaceId),
    /// "Space verlassen" erst nach Rueckfrage.
    AskLeave(bool),
    LeaveSpace,
    Published(Result<(), String>),
    JoinVoice(ChannelId),
    LeaveVoice,
    ToggleMute,
    ToggleDeaf,
    /// Verbindungswerte des Gespraechs in der Sprach-Leiste ein-/ausblenden.
    ToggleVoiceStats,
    Denoise(bool),
    VoiceActivation(bool),
    Threshold(f32),
    Agc(bool),
    Gain(f32),
    Monitor(bool),
    HkCapture(Action),
    HkCancel,
    HkClear(Action),
    HkPtt(bool),
    HkRelease(f32),
    Ov(OvChange),
    PeerMenu(Option<PublicKey>),
    PeerVolume(PublicKey, f32),
    PeerMute(PublicKey, bool),
    Chat(crate::chat_ui::Msg),
    OpenText(ChannelId),
    ShowMembers,
    CreateTextChannel,
    /// Space waehlen und Text-Channel oeffnen (Toast-Klick).
    GoTo(SpaceId, ChannelId),
    CycleChannel(ChannelId),
    CycleSpace,
    /// Channel-Verwaltung (Umbenennen, Reihenfolge, Meldungen) ein-/ausblenden.
    ToggleManage,
    ToggleDnd,
    /// Direktnachricht mit einem Freund oeffnen (und beim ersten Mal anlegen).
    OpenDm(PublicKey),
    /// Quelle fuer einen Stream waehlen (M3-01).
    OpenPicker,
    Picker(crate::source_picker::Msg),
    StopShare,
    /// Gewaehlte Quelle an den Voice-Channel senden (M3-06).
    StartStream,
    /// Stream an/aus per Hotkey.
    ToggleStream,
    /// Stream eines Teilnehmers ansehen (wird in der App behandelt).
    WatchStream(PublicKey),
    DmReady(Result<(SpaceId, ChannelId), String>),
    /// Freund anrufen (wird in der App behandelt).
    CallUser(PublicKey),
    /// Eingehenden Anruf annehmen / ablehnen, ausgehenden abbrechen, auflegen (App).
    AcceptCall,
    DeclineCall,
    CancelCall,
    HangUp,
}

#[derive(Debug, Clone, Copy)]
pub enum OvChange {
    Enabled(bool),
    Corner(u8),
    Scale(f32),
    Opacity(f32),
    OnlySpeaking(bool),
}

#[derive(Default)]
pub struct State {
    list: Vec<SpaceInfo>,
    /// Offene Einladungen von Freunden (M-2).
    invites: Vec<SpaceInfo>,
    confirm_leave: bool,
    pub sel: Option<SpaceId>,
    view: SpaceView,
    new_space: String,
    new_channel: String,
    edit: Option<(ChannelId, String)>,
    muted: bool,
    deafened: bool,
    peer_menu: Option<PublicKey>,
    chat: crate::chat_ui::Chat,
    unread: Unreads,
    /// Eingangs-Wasserzeichen der Toasts; `None`, bis der erste Ladevorgang es gesetzt hat.
    mark: Option<i64>,
    pending_open: Option<(SpaceId, ChannelId)>,
    window_open: bool,
    dnd: bool,
    manage: bool,
    confirm_delete: Option<ChannelId>,
    /// Nach dem Wechsel in diesen Space den ersten Textkanal öffnen.
    auto_open: Option<SpaceId>,
    dms: Dms,
    dm_busy: bool,
    picker: Option<crate::source_picker::State>,
    /// Gewaehlte Bildquelle; gestreamt wird ab M3-03.
    share: Option<crate::source_picker::Choice>,
    pub info: Option<String>,
    /// (Channel, Space) des Voice-Channels, in dem wir sind; bleibt sichtbar, auch wenn ein
    /// anderer Space gewaehlt ist.
    voice_label: Option<(String, String)>,
    /// Verbindungswerte in der Sprach-Leiste anzeigen.
    voice_stats: bool,
}

fn err<E: std::fmt::Display>(e: E) -> String {
    e.to_string()
}

fn random_id() -> [u8; 16] {
    let mut id = [0u8; 16];
    getrandom::fill(&mut id).expect("Zufall");
    id
}

fn short(user: &PublicKey) -> String {
    user.iter().take(3).map(|b| format!("{b:02x}")).collect()
}

impl State {
    /// Laedt Space-Liste und die Ansicht des gewaehlten Spaces neu.
    pub fn reload(&self, core: &Core) -> Task<Msg> {
        let db = core.db.clone();
        let sel = self.sel;
        let (me, my_name) = (core.user, core.name.clone());
        let since = self.mark;
        Task::perform(
            async move {
                tokio::task::spawn_blocking(move || {
                    db.call(move |c| {
                        // Nur angenommene Spaces; Einladungen getrennt (M-2).
                        let lists = consent::space_lists(c, &me)?;
                        let list = lists.shown;
                        let shown = |s: &SpaceId| list.iter().any(|i| i.space_id == *s);
                        let sel = sel.filter(|s| list.iter().any(|i| i.space_id == *s));
                        let view = match sel {
                            Some(s) => Some((s, space_view(c, &s)?)),
                            None => None,
                        };
                        let mut per_channel = HashMap::new();
                        for s in &list {
                            per_channel.extend(unread::channel_unread(
                                c,
                                &s.space_id,
                                &me,
                                &my_name,
                            )?);
                        }
                        let mut per_space = unread::space_unread(c, &me, &my_name)?;
                        per_space.retain(|s, _| shown(s));
                        let incoming = match since {
                            Some(s) => notify::incoming(c, &me, &my_name, s)?,
                            None => (Vec::new(), notify::watermark(c)?),
                        };
                        let mut dms = dm::dm_index(c, &me)?;
                        dms.retain(|_, (s, _)| shown(s));
                        Ok((
                            list,
                            view,
                            (per_channel, per_space),
                            Some(incoming),
                            dms,
                            lists.invites,
                        ))
                    })
                    .unwrap_or_default()
                })
                .await
                .unwrap_or_default()
            },
            |d| Msg::Loaded(Box::new(d)),
        )
    }

    /// Ungelesenes der Direktnachricht mit `peer`.
    pub fn dm_unread(&self, peer: &PublicKey) -> Unread {
        self.dms
            .get(peer)
            .and_then(|(s, _)| self.unread.1.get(s))
            .copied()
            .unwrap_or_default()
    }

    fn selected_is_dm(&self) -> bool {
        self.sel.is_some_and(|s| self.is_dm_space(&s))
    }

    /// DM nur laut `dm_index` (genau zwei Mitglieder), nicht schon wegen des Namens (N-11):
    /// Ein Space mit dem reservierten Namen und mehr Mitgliedern erscheint normal in der Leiste.
    fn is_dm_space(&self, s: &SpaceId) -> bool {
        self.dms.values().any(|(sp, _)| sp == s)
    }

    pub fn set_window(&mut self, open: bool) {
        self.window_open = open;
    }

    /// Zeigt Toasts fuer neu eingegangene Nachrichten. Beim ersten Ladevorgang wird nur das
    /// Wasserzeichen gesetzt, damit der Start nicht alles Ungelesene meldet.
    fn toast_incoming(&mut self, core: &Core, inc: Option<(Vec<Incoming>, i64)>) {
        let Some((list, mark)) = inc else { return };
        if self.mark.is_some_and(|m| mark <= m) {
            return;
        }
        let first = self.mark.is_none();
        self.mark = Some(mark);
        if first || self.chat.focused() {
            return;
        }
        let settings = core.notify();
        let shown: Vec<Incoming> = list
            .into_iter()
            .filter(|m| {
                settings.allows(&m.space, &m.channel, m.mention, m.dm)
                    && !(self.window_open && self.chat.target == Some((m.space, m.channel)))
            })
            .collect();
        if shown.len() > 3 {
            let m = &shown[0];
            crate::toast::show(
                "Campfire",
                &app_core::tr!("{} new messages", "{} neue Nachrichten", shown.len()),
                Some((m.space, m.channel)),
            );
            return;
        }
        for m in shown {
            let title = if m.dm {
                m.author_name.clone()
            } else if m.mention {
                app_core::tr!(
                    "{} mentioned you in {} #{}",
                    "{} erwähnt dich in {} #{}",
                    m.author_name,
                    m.space_name,
                    m.channel_name
                )
            } else {
                format!("{} · #{}", m.space_name, m.channel_name)
            };
            let body = if m.mention || m.dm {
                m.body.clone()
            } else {
                format!("{}: {}", m.author_name, m.body)
            };
            crate::toast::show(&title, &body, Some((m.space, m.channel)));
        }
    }

    /// Fensterfokus an den Chat weitergeben (Lesestand).
    pub fn set_focus(&mut self, focused: bool, core: &Core) -> Task<Msg> {
        self.chat.set_focus(focused, core).map(Msg::Chat)
    }

    fn create_channel(&mut self, core: &Core, kind: ChannelKind) -> Task<Msg> {
        let (Some(space), name) = (self.sel, self.new_channel.trim().to_owned()) else {
            return Task::none();
        };
        if name.is_empty() {
            return Task::none();
        }
        self.new_channel.clear();
        let position = self
            .view
            .channels
            .iter()
            .map(|c| c.position + 1)
            .max()
            .unwrap_or(0);
        Self::publish(
            core,
            Some(space),
            EventKind::ChannelCreate {
                channel: random_id(),
                kind,
                name,
                position,
            },
        )
    }

    fn publish(core: &Core, space: Option<SpaceId>, kind: EventKind) -> Task<Msg> {
        let sync = core.sync.clone();
        Task::perform(
            async move { sync.publish(space, kind).await.map(|_| ()).map_err(err) },
            Msg::Published,
        )
    }

    /// Austreten (eigenes `MemberRemove`). Danach bin ich laut Projektion kein Mitglied mehr,
    /// der Space verschwindet und seine Zustimmung verfaellt.
    fn leave(&mut self, core: &Core, space: SpaceId) -> Task<Msg> {
        if core.voice.current().is_some_and(|c| c.0 == space) {
            core.leave_voice();
            self.voice_label = None;
        }
        Self::publish(
            core,
            Some(space),
            EventKind::MemberRemove {
                user: core.user,
                last_seq: 0,
            },
        )
    }

    fn is_admin(&self, core: &Core) -> bool {
        self.view
            .members
            .iter()
            .any(|m| m.admin && m.user == core.user)
    }

    pub fn update(&mut self, msg: Msg, core: &Core) -> Task<Msg> {
        match msg {
            Msg::Loaded(data) => {
                let (list, view, unread, incoming, dms, invites) = *data;
                self.list = list;
                self.invites = invites;
                self.dms = dms;
                self.unread = unread;
                self.dnd = core.notify().dnd;
                let (u, m) = self
                    .unread
                    .1
                    .values()
                    .fold((0, 0), |a, x| (a.0 + x.unread, a.1 + x.mentions));
                crate::set_tray_badge(u, m);
                self.toast_incoming(core, incoming);
                match view {
                    Some((s, v)) => {
                        self.sel = Some(s);
                        self.view = v;
                    }
                    None => {
                        self.sel = None;
                        self.view = SpaceView::default();
                    }
                }
                if let Some((s, ch)) = self.pending_open.take() {
                    if let Some(c) = (self.sel == Some(s))
                        .then(|| self.view.channels.iter().find(|c| c.id == ch))
                        .flatten()
                    {
                        let name = c.name.clone();
                        return self.chat.open(s, ch, name, core).map(Msg::Chat);
                    }
                }
                // Neu betretener Space: gleich den ersten Textkanal zeigen, statt einer leeren Seite.
                if self.sel.is_some() && self.auto_open == self.sel {
                    self.auto_open = None;
                    if let (Some(s), Some(c)) = (
                        self.sel,
                        self.view
                            .channels
                            .iter()
                            .find(|c| c.kind == ChannelKind::Text),
                    ) {
                        let (id, name) = (c.id, c.name.clone());
                        return self.chat.open(s, id, name, core).map(Msg::Chat);
                    }
                }
                // Channel geloescht oder Space gewechselt: Chat schliessen, sonst aktualisieren.
                let still = self.chat.target.is_some_and(|(s, c)| {
                    self.sel == Some(s) && self.view.channels.iter().any(|x| x.id == c)
                });
                if still {
                    return self.chat.refresh(core).map(Msg::Chat);
                }
                self.chat.close();
            }
            Msg::Chat(m) => {
                let marked = matches!(m, crate::chat_ui::Msg::Marked);
                let t = self.chat.update(m, core).map(Msg::Chat);
                // Lesestand geaendert: Zaehler neu laden.
                return if marked {
                    Task::batch([t, self.reload(core)])
                } else {
                    t
                };
            }
            Msg::OpenText(ch) => {
                self.picker = None;
                if let Some(space) = self.sel {
                    let name = self
                        .view
                        .channels
                        .iter()
                        .find(|c| c.id == ch)
                        .map_or_else(String::new, |c| c.name.clone());
                    return self.chat.open(space, ch, name, core).map(Msg::Chat);
                }
            }
            Msg::ShowMembers => self.chat.close(),
            Msg::GoTo(space, channel) => {
                self.sel = Some(space);
                self.edit = None;
                self.info = None;
                self.pending_open = Some((space, channel));
                self.chat.close();
                return self.reload(core);
            }
            Msg::CycleChannel(ch) => {
                if let Some(space) = self.sel {
                    let mut s = core.notify();
                    let next = s.mode(&space, &ch).next();
                    s.set_channel(&ch, next);
                    core.set_notify(s);
                }
            }
            Msg::CycleSpace => {
                if let Some(space) = self.sel {
                    let mut s = core.notify();
                    let next = s.space_mode(&space).next();
                    s.set_space(&space, next);
                    core.set_notify(s);
                }
            }
            Msg::ToggleManage => {
                self.manage = !self.manage;
                self.edit = None;
                self.confirm_delete = None;
            }
            Msg::OpenDm(peer) => match self.dms.get(&peer).copied() {
                Some((s, Some(ch))) => return self.update(Msg::GoTo(s, ch), core),
                Some((_, None)) => {
                    self.info = Some(
                        tr(
                            "The direct message is still being set up.",
                            "Die Direktnachricht wird noch eingerichtet.",
                        )
                        .into(),
                    );
                }
                None if self.dm_busy => {}
                None => {
                    self.dm_busy = true;
                    let sync = core.sync.clone();
                    return Task::perform(
                        async move {
                            let space = sync
                                .publish(
                                    None,
                                    EventKind::SpaceCreate {
                                        name: dm::DM_NAME.into(),
                                    },
                                )
                                .await
                                .map_err(err)?
                                .body
                                .space_id;
                            sync.publish(Some(space), EventKind::MemberAdd { user: peer })
                                .await
                                .map_err(err)?;
                            let channel = random_id();
                            sync.publish(
                                Some(space),
                                EventKind::ChannelCreate {
                                    channel,
                                    kind: ChannelKind::Text,
                                    name: dm::DM_CHANNEL.into(),
                                    position: 0,
                                },
                            )
                            .await
                            .map_err(err)?;
                            Ok((space, channel))
                        },
                        Msg::DmReady,
                    );
                }
            },
            Msg::DmReady(r) => {
                self.dm_busy = false;
                match r {
                    Ok((s, ch)) => return self.update(Msg::GoTo(s, ch), core),
                    Err(e) => self.info = Some(e),
                }
            }
            Msg::OpenPicker => {
                self.chat.close();
                let (st, t) = crate::source_picker::State::open(self.share.as_ref());
                self.picker = Some(st);
                return t.map(Msg::Picker);
            }
            Msg::Picker(m) => {
                if let Some(p) = &mut self.picker {
                    let (t, out) = p.update(m);
                    match out {
                        crate::source_picker::Outcome::Chosen(c) => {
                            self.share = Some(c);
                            self.picker = None;
                        }
                        crate::source_picker::Outcome::Closed => self.picker = None,
                        crate::source_picker::Outcome::None => {}
                    }
                    return t.map(Msg::Picker);
                }
            }
            Msg::StopShare => {
                core.stream.stop();
                self.share = None;
            }
            Msg::ToggleStream => {
                if core.stream.is_streaming() {
                    core.stream.stop();
                } else if let Some(c) = &self.share {
                    if let Err(e) = core.stream.start(c.source.id, c.cursor, core.video()) {
                        self.info = Some(e);
                    }
                } else {
                    self.info = Some(
                        tr(
                            "Choose a source first (Share screen …).",
                            "Wähle zuerst eine Quelle (Bildschirm teilen …).",
                        )
                        .to_owned(),
                    );
                }
            }
            // Werden in der App behandelt.
            Msg::WatchStream(_)
            | Msg::CallUser(_)
            | Msg::AcceptCall
            | Msg::DeclineCall
            | Msg::CancelCall
            | Msg::HangUp => {}
            Msg::StartStream => {
                if let Some(c) = &self.share {
                    if let Err(e) = core.stream.start(c.source.id, c.cursor, core.video()) {
                        self.info = Some(e);
                    }
                }
            }
            Msg::ToggleDnd => {
                let mut s = core.notify();
                s.dnd = !s.dnd;
                self.dnd = s.dnd;
                core.set_notify(s);
            }
            Msg::CreateTextChannel => return self.create_channel(core, ChannelKind::Text),
            Msg::JoinVoice(ch) => {
                if let Some(space) = self.sel {
                    let channel = self
                        .view
                        .channels
                        .iter()
                        .find(|c| c.id == ch)
                        .map_or_else(String::new, |c| c.name.clone());
                    let space_name = self
                        .list
                        .iter()
                        .find(|s| s.space_id == space)
                        .map_or_else(String::new, |s| s.name.clone());
                    self.voice_label = Some((channel, space_name));
                    core.join_voice(space, ch);
                    core.voice.set_state(self.muted, self.deafened);
                }
            }
            Msg::LeaveVoice => core.leave_voice(),
            Msg::Denoise(v) => core.set_input(InputSettings {
                denoise: v,
                ..core.input()
            }),
            Msg::VoiceActivation(v) => core.set_input(InputSettings {
                voice_activation: v,
                ..core.input()
            }),
            Msg::Threshold(v) => core.set_input(InputSettings {
                threshold_db: v,
                ..core.input()
            }),
            Msg::Agc(v) => core.set_input(InputSettings {
                agc: v,
                ..core.input()
            }),
            Msg::Gain(v) => core.set_input(InputSettings {
                gain: v,
                ..core.input()
            }),
            Msg::Monitor(v) => core.set_monitor(v),
            Msg::HkCapture(a) => core.hotkeys.capture(a),
            Msg::HkCancel => core.hotkeys.cancel_capture(),
            Msg::HkClear(a) => core.hotkeys.update(|s| s.set_binding(a, None)),
            Msg::HkPtt(v) => {
                core.hotkeys.update(|s| s.ptt_enabled = v);
                core.reapply_input();
            }
            Msg::HkRelease(v) => core.hotkeys.update(|s| s.ptt_release_ms = v as u32),
            Msg::Ov(c) => {
                let mut o = core.overlay();
                match c {
                    OvChange::Enabled(v) => o.enabled = v,
                    OvChange::Corner(v) => o.corner = v,
                    OvChange::Scale(v) => o.scale = v,
                    OvChange::Opacity(v) => o.opacity = v,
                    OvChange::OnlySpeaking(v) => o.only_speaking = v,
                }
                core.set_overlay(o);
            }
            Msg::PeerMenu(u) => {
                self.peer_menu = if self.peer_menu == u { None } else { u };
            }
            Msg::PeerVolume(u, v) => {
                let p = core.peer_pref(&u);
                core.set_peer_pref(&u, PeerPref { volume: v, ..p });
            }
            Msg::PeerMute(u, m) => {
                let p = core.peer_pref(&u);
                core.set_peer_pref(&u, PeerPref { muted: m, ..p });
            }
            Msg::ToggleMute => {
                self.muted = !self.muted;
                if !self.muted {
                    self.deafened = false;
                }
                core.voice.set_state(self.muted, self.deafened);
                core.sounds.play(if self.muted {
                    Sound::Mute
                } else {
                    Sound::Unmute
                });
            }
            Msg::ToggleVoiceStats => self.voice_stats = !self.voice_stats,
            Msg::ToggleDeaf => {
                self.deafened = !self.deafened;
                core.voice.set_state(self.muted, self.deafened);
                core.sounds.play(if self.deafened {
                    Sound::Deafen
                } else {
                    Sound::Undeafen
                });
            }
            Msg::Select(s) => {
                self.picker = None;
                self.confirm_leave = false;
                self.sel = s;
                self.auto_open = s;
                self.edit = None;
                self.info = None;
                self.chat.close();
                return self.reload(core);
            }
            Msg::NewSpace => {
                self.picker = None;
                self.sel = None;
                self.auto_open = None;
                self.edit = None;
                self.info = None;
                self.chat.close();
                return Task::batch([
                    self.reload(core),
                    iced::widget::operation::snap_to_end(HOME_PAGE),
                    iced::widget::operation::focus(NEW_SPACE_INPUT),
                ]);
            }
            Msg::NewSpaceName(s) => self.new_space = s,
            Msg::CreateSpace => {
                let name = self.new_space.trim().to_owned();
                if name.is_empty() {
                    return Task::none();
                }
                let sync = core.sync.clone();
                return Task::perform(
                    async move {
                        sync.publish(None, EventKind::SpaceCreate { name })
                            .await
                            .map(|e| e.body.space_id)
                            .map_err(err)
                    },
                    Msg::Created,
                );
            }
            Msg::Created(Ok(id)) => {
                self.new_space.clear();
                self.sel = Some(id);
                return self.reload(core);
            }
            Msg::Created(Err(e)) => self.info = Some(e),
            Msg::NewChannelName(s) => self.new_channel = s,
            Msg::CreateChannel => return self.create_channel(core, ChannelKind::Voice),
            Msg::Edit(id, name) => self.edit = Some((id, name)),
            Msg::EditText(s) => {
                if let Some((_, t)) = &mut self.edit {
                    *t = s;
                }
            }
            Msg::CancelEdit => self.edit = None,
            Msg::SaveEdit => {
                if let (Some(space), Some((channel, name))) = (self.sel, self.edit.take()) {
                    let name = name.trim().to_owned();
                    if !name.is_empty() {
                        return Self::publish(
                            core,
                            Some(space),
                            EventKind::ChannelUpdate {
                                channel,
                                name: Some(name),
                                position: None,
                            },
                        );
                    }
                }
            }
            Msg::Move(id, dir) => {
                let Some(space) = self.sel else {
                    return Task::none();
                };
                let ch = &self.view.channels;
                let Some(i) = ch.iter().position(|c| c.id == id) else {
                    return Task::none();
                };
                let j = i as i32 + dir;
                if j < 0 || j as usize >= ch.len() {
                    return Task::none();
                }
                let (a, b) = (&ch[i], &ch[j as usize]);
                // Positionen tauschen; bei Gleichstand ein Stueck verschieben.
                let (pa, pb) = if a.position == b.position {
                    if dir < 0 {
                        (b.position, b.position + 1)
                    } else {
                        (b.position + 1, b.position)
                    }
                } else {
                    (b.position, a.position)
                };
                let upd = |channel, position| EventKind::ChannelUpdate {
                    channel,
                    name: None,
                    position: Some(position),
                };
                return Task::batch([
                    Self::publish(core, Some(space), upd(a.id, pa)),
                    Self::publish(core, Some(space), upd(b.id, pb)),
                ]);
            }
            Msg::AskDelete(c) => self.confirm_delete = c,
            Msg::DeleteChannel(channel) => {
                self.confirm_delete = None;
                if let Some(space) = self.sel {
                    return Self::publish(core, Some(space), EventKind::ChannelDelete { channel });
                }
            }
            Msg::Invite(user) => {
                if let Some(space) = self.sel {
                    self.info = Some(tr("Invited. Offline friends get the space when they next come online.", "Eingeladen. Offline-Freunde bekommen den Space beim nächsten Online-Gehen.").into());
                    return Self::publish(core, Some(space), EventKind::MemberAdd { user });
                }
            }
            Msg::AcceptInvite(space) => {
                let db = core.db.clone();
                return Task::perform(
                    async move {
                        tokio::task::spawn_blocking(move || {
                            db.call(move |c| consent::accept(c, &space))
                        })
                        .await
                    },
                    move |_| Msg::Accepted(space),
                );
            }
            Msg::Accepted(space) => {
                self.sel = Some(space);
                self.auto_open = Some(space);
                return self.reload(core);
            }
            Msg::DeclineInvite(space) => {
                self.invites.retain(|s| s.space_id != space);
                return self.leave(core, space);
            }
            Msg::AskLeave(b) => self.confirm_leave = b,
            Msg::LeaveSpace => {
                self.confirm_leave = false;
                if let Some(space) = self.sel.take() {
                    self.chat.close();
                    return self.leave(core, space);
                }
            }
            Msg::Published(r) => {
                if let Err(e) = r {
                    self.info = Some(e);
                }
                return self.reload(core);
            }
        }
        Task::none()
    }

    // ---------- Ansichten ----------

    /// Warnung bei widerspruechlichem Verlauf eines Mitglieds (N-14).
    fn equivocation_warning<'a>(
        &self,
        name_of: &dyn Fn(&PublicKey) -> String,
    ) -> Option<Element<'a, Msg>> {
        if self.view.equivocators.is_empty() {
            return None;
        }
        let names = self
            .view
            .equivocators
            .iter()
            .map(name_of)
            .collect::<Vec<_>>()
            .join(", ");
        Some(ui::notice(
            ui::Tone::Warning,
            app_core::tr!(
                "Conflicting history from {names}: two different versions of the same message were signed with their key. Members may see different messages from them. This happens when the same account runs on two computers at once (for example after restoring a backup) or when the app was tampered with.",
                "Widersprüchlicher Verlauf von {names}: Mit diesem Schlüssel wurden zwei verschiedene Fassungen derselben Nachricht signiert. Mitglieder sehen von dieser Person womöglich unterschiedliche Nachrichten. Das passiert, wenn dasselbe Konto gleichzeitig auf zwei Rechnern läuft (etwa nach dem Wiederherstellen eines Backups) oder die App manipuliert wurde."
            ),
        ))
    }

    fn peer_name(friends: &[Friend], user: &PublicKey) -> String {
        friends.iter().find(|f| f.user == *user).map_or_else(
            || app_core::tr!("Unknown ({})", "Unbekannt ({})", short(user)),
            |f| f.name.clone(),
        )
    }

    /// Gegenueber der gewaehlten Direktnachricht.
    fn dm_peer(&self, core: &Core, friends: &[Friend]) -> Option<(PublicKey, String)> {
        if !self.selected_is_dm() {
            return None;
        }
        let user = self
            .view
            .members
            .iter()
            .find(|m| m.user != core.user)
            .map(|m| m.user)?;
        Some((user, Self::peer_name(friends, &user)))
    }

    /// Space-Leiste links: Start (Freunde, Direktnachrichten), Spaces, neuer Space, Ruhemodus.
    pub fn bar(&self) -> Element<'_, Msg> {
        let dm_unread = self
            .dms
            .values()
            .filter_map(|(s, _)| self.unread.1.get(s))
            .fold(Unread::default(), |a, u| Unread {
                unread: a.unread + u.unread,
                mentions: a.mentions + u.mentions,
            });
        let home_sel = self.sel.is_none() || self.selected_is_dm();
        let home = rail_item(
            text("K").size(20).font(ui::BOLD).line_height(1.0).into(),
            home_sel,
            false,
            (dm_unread.unread > 0 || !self.invites.is_empty())
                .then(|| ui::count_badge(dm_unread.unread + self.invites.len() as u32, true)),
            tr(
                "Home: friends and direct messages",
                "Start: Freunde und Direktnachrichten",
            )
            .to_owned(),
            Msg::Select(None),
        );
        let mut list = column![].spacing(space::S);
        for s in self.list.iter().filter(|s| !self.is_dm_space(&s.space_id)) {
            let u = self.unread.1.get(&s.space_id).copied().unwrap_or_default();
            let selected = self.sel == Some(s.space_id);
            list = list.push(rail_item(
                text(ui::initials(&s.name))
                    .size(size::H3)
                    .font(ui::SEMIBOLD)
                    .line_height(1.0)
                    .into(),
                selected,
                u.unread > 0,
                (u.mentions > 0).then(|| ui::count_badge(u.mentions, true)),
                s.name.clone(),
                Msg::Select(Some(s.space_id)),
            ));
        }
        list = list.push(rail_item(
            text("+")
                .size(24)
                .line_height(1.0)
                .style(ui::tx::success)
                .into(),
            false,
            false,
            None,
            tr("Create new space", "Neuen Space anlegen").to_owned(),
            Msg::NewSpace,
        ));
        let dnd = ui::tip(
            button(
                container(text("☾").size(18).line_height(1.0))
                    .center_x(Length::Fixed(44.0))
                    .center_y(Length::Fixed(36.0)),
            )
            .padding(0)
            .style(if self.dnd { btn::danger } else { btn::ghost })
            .on_press(Msg::ToggleDnd),
            if self.dnd {
                tr(
                    "Do-not-disturb is on: no notifications. Click to end.",
                    "Ruhemodus ist an: keine Meldungen. Klicken zum Beenden.",
                )
            } else {
                tr(
                    "Do-not-disturb: temporarily turn off notifications",
                    "Ruhemodus: Meldungen vorübergehend ausschalten",
                )
            },
            tooltip::Position::Right,
        );
        container(
            column![
                home,
                container(
                    container(Space::new())
                        .width(32)
                        .height(2)
                        .style(ui::bg::divider)
                )
                .center_x(Length::Fill),
                ui::scroll(list).height(Length::Fill),
                container(dnd).center_x(Length::Fill),
            ]
            .spacing(space::S)
            .padding([space::M, 0.0]),
        )
        .width(ui::RAIL_WIDTH)
        .height(Length::Fill)
        .style(ui::bg::rail)
        .into()
    }

    /// Seitenleiste: Direktnachrichten (Start) bzw. Channels des gewaehlten Spaces.
    pub fn channels<'a>(&'a self, core: &'a Core, friends: &'a [Friend]) -> Element<'a, Msg> {
        let Some(space) = self.sel.filter(|_| !self.selected_is_dm()) else {
            return self.home_sidebar(core, friends);
        };
        let admin = self.is_admin(core);
        let name = self
            .list
            .iter()
            .find(|s| s.space_id == space)
            .map_or("", |s| s.name.as_str());
        let current = core.voice.current();
        let notify = core.notify();

        let header = ui::header(
            text(name)
                .size(size::H3)
                .font(ui::SEMIBOLD)
                .style(ui::tx::normal)
                .wrapping(text::Wrapping::None),
            ui::tip(
                button(
                    text(if self.manage {
                        tr("Done", "Fertig")
                    } else {
                        tr("Manage", "Verwalten")
                    })
                    .size(size::CAPTION),
                )
                .padding([4, 8])
                .style(if self.manage {
                    btn::primary
                } else {
                    btn::ghost
                })
                .on_press(Msg::ToggleManage),
                tr(
                    "Manage channels and set notifications",
                    "Channels verwalten und Benachrichtigungen einstellen",
                ),
                tooltip::Position::Bottom,
            ),
        );

        let mut col = column![].spacing(2).padding([space::S, space::S]);
        if self.manage {
            col = col.push(
                container(ui::setting(
                    tr("Notifications", "Meldungen"),
                    Some(tr("for the whole space", "für den ganzen Space").to_owned()),
                    ui::button_small(notify.space_mode(&space).label())
                        .style(btn::chip(true))
                        .on_press(Msg::CycleSpace),
                ))
                .padding([space::S, space::S]),
            );
            // Der Gruender kann nicht austreten (einziger Admin).
            if !admin {
                let leave: Element<'_, Msg> = if self.confirm_leave {
                    column![
                        text(tr(
                            "Leave this space? You will no longer get its messages. Only the founder can add you again.",
                            "Diesen Space verlassen? Du bekommst dann keine Nachrichten mehr daraus. Nur der Gründer kann dich wieder hinzufügen.",
                        ))
                        .size(size::CAPTION)
                        .style(ui::tx::danger),
                        row![
                            ui::button_small(tr("Leave", "Verlassen"))
                                .style(btn::danger)
                                .on_press(Msg::LeaveSpace),
                            ui::button_small(tr("Cancel", "Abbrechen"))
                                .style(btn::ghost)
                                .on_press(Msg::AskLeave(false)),
                        ]
                        .spacing(space::XS),
                    ]
                    .spacing(6)
                    .into()
                } else {
                    ui::setting(
                        tr("Leave space", "Space verlassen"),
                        None,
                        ui::button_small(tr("Leave", "Verlassen"))
                            .style(btn::danger_ghost)
                            .on_press(Msg::AskLeave(true)),
                    )
                };
                col = col.push(container(leave).padding([space::S, space::S]));
            }
        }
        // Textkanäle zuerst, dann Sprachkanäle.
        let ordered = self
            .view
            .channels
            .iter()
            .filter(|c| c.kind == ChannelKind::Text)
            .chain(
                self.view
                    .channels
                    .iter()
                    .filter(|c| c.kind != ChannelKind::Text),
            );
        let mut last_kind = None;
        for c in ordered {
            let is_text = c.kind == ChannelKind::Text;
            if last_kind != Some(is_text) {
                last_kind = Some(is_text);
                col = col.push(
                    container(ui::overline(if is_text {
                        tr("Text channels", "Textkanäle")
                    } else {
                        tr("Voice channels", "Sprachkanäle")
                    }))
                    .padding(
                        iced::Padding::ZERO
                            .top(space::M)
                            .bottom(space::XS)
                            .left(space::S),
                    ),
                );
            }
            if let Some((_, t)) = self.edit.as_ref().filter(|(id, _)| *id == c.id) {
                col = col.push(
                    column![
                        ui::input("Name", t)
                            .on_input(Msg::EditText)
                            .on_submit(Msg::SaveEdit),
                        row![
                            ui::button_small(tr("Save", "Speichern"))
                                .style(btn::primary)
                                .on_press(Msg::SaveEdit),
                            ui::button_small(tr("Cancel", "Abbrechen"))
                                .style(btn::ghost)
                                .on_press(Msg::CancelEdit),
                        ]
                        .spacing(space::XS),
                    ]
                    .spacing(6)
                    .padding([space::XS, 0.0]),
                );
                continue;
            }
            let here = !is_text && current.is_some_and(|cur| cur == (space, c.id));
            let open = is_text && self.chat.target.is_some_and(|(_, ch)| ch == c.id);
            let u = self.unread.0.get(&c.id).copied().unwrap_or_default();
            let unread = is_text && u.unread > 0 && !open;
            let mut label = row![
                container(
                    text(if is_text { "#" } else { "🔊" })
                        .size(if is_text { size::H3 } else { size::SMALL })
                        .style(ui::tx::faint)
                )
                .width(18)
                .center_x(18),
                text(c.name.as_str())
                    .size(size::BODY)
                    .font(if unread || open || here {
                        ui::SEMIBOLD
                    } else {
                        iced::Font::DEFAULT
                    })
                    .wrapping(text::Wrapping::None)
                    .width(Length::Fill),
            ]
            .spacing(6)
            .align_y(Alignment::Center);
            if is_text && u.mentions > 0 {
                label = label.push(ui::count_badge(u.mentions, true));
            } else if unread {
                label = label.push(ui::count_badge(u.unread, false));
            }
            let press = if is_text {
                Msg::OpenText(c.id)
            } else {
                Msg::JoinVoice(c.id)
            };
            let selected = open || here;
            col = col.push(
                button(label)
                    .padding([6, 8])
                    .width(Length::Fill)
                    .style(btn::nav_item(selected, unread))
                    .on_press(press),
            );
            if self.manage {
                col = col.push(self.manage_row(c, is_text, admin, notify.mode(&space, &c.id)));
            }
            if here {
                col = col.push(self.voice_members(core, friends));
            }
        }
        if self.view.channels.is_empty() {
            col = col.push(
                container(ui::muted(if admin {
                    tr(
                        "No channels yet. Create the first one below.",
                        "Noch keine Channels. Lege unten den ersten an.",
                    )
                } else {
                    tr("No channels yet.", "Noch keine Channels.")
                }))
                .padding(space::S),
            );
        }
        if admin && (self.manage || self.view.channels.is_empty()) {
            let ok = !self.new_channel.trim().is_empty();
            col = col
                .push(
                    container(
                        column![
                            ui::overline(tr("New channel", "Neuer Channel")),
                            ui::input("Name", &self.new_channel)
                                .on_input(Msg::NewChannelName)
                                .on_submit(Msg::CreateTextChannel),
                            row![
                                ui::button_small("# Text")
                                    .style(btn::secondary)
                                    .on_press_maybe(ok.then_some(Msg::CreateTextChannel)),
                                ui::button_small(tr("🔊 Voice", "🔊 Sprache"))
                                    .style(btn::secondary)
                                    .on_press_maybe(ok.then_some(Msg::CreateChannel)),
                            ]
                            .spacing(6),
                        ]
                        .spacing(space::S),
                    )
                    .padding(space::M)
                    .style(ui::bg::card),
                )
                .push(Space::new().height(space::S));
        }
        let list = ui::scroll(col).height(Length::Fill);
        column![header, list].height(Length::Fill).into()
    }

    /// Verwaltung eines Channels: Meldungen, Reihenfolge, Umbenennen, Loeschen.
    fn manage_row<'a>(
        &self,
        c: &'a app_core::eventlog::ChannelRow,
        is_text: bool,
        admin: bool,
        mode: Mode,
    ) -> Element<'a, Msg> {
        if self.confirm_delete == Some(c.id) {
            return container(
                column![
                    text(app_core::tr!("Delete “{}”?", "„{}“ löschen?", c.name))
                        .size(size::CAPTION)
                        .style(ui::tx::danger),
                    row![
                        ui::button_small(tr("Delete", "Löschen"))
                            .style(btn::danger)
                            .on_press(Msg::DeleteChannel(c.id)),
                        ui::button_small(tr("Cancel", "Abbrechen"))
                            .style(btn::ghost)
                            .on_press(Msg::AskDelete(None)),
                    ]
                    .spacing(space::XS),
                ]
                .spacing(6),
            )
            .padding(iced::Padding::ZERO.left(32).bottom(space::S))
            .into();
        }
        let mut r = row![].spacing(2).align_y(Alignment::Center);
        if is_text {
            r = r.push(ui::tip(
                ui::button_small(match mode {
                    Mode::All => tr("All", "Alle"),
                    Mode::Mentions => tr("@ only", "Nur @"),
                    Mode::Nothing => tr("Muted", "Stumm"),
                })
                .padding([2, 8])
                .style(btn::chip(mode != Mode::All))
                .on_press(Msg::CycleChannel(c.id)),
                tr(
                    "Notifications for this channel: click to switch",
                    "Meldungen für diesen Channel: klicken zum Wechseln",
                ),
                tooltip::Position::Top,
            ));
        }
        r = r.push(Space::new().width(Length::Fill));
        if admin {
            r = r
                .push(mini(
                    "↑",
                    tr("Move up", "Nach oben"),
                    Msg::Move(c.id, -1),
                    btn::ghost,
                ))
                .push(mini(
                    "↓",
                    tr("Move down", "Nach unten"),
                    Msg::Move(c.id, 1),
                    btn::ghost,
                ))
                .push(mini(
                    "✎",
                    tr("Rename", "Umbenennen"),
                    Msg::Edit(c.id, c.name.clone()),
                    btn::ghost,
                ))
                .push(mini(
                    "✕",
                    tr("Delete", "Löschen"),
                    Msg::AskDelete(Some(c.id)),
                    btn::danger_ghost,
                ));
        }
        container(r)
            .padding(iced::Padding::ZERO.left(32).bottom(space::XS))
            .into()
    }

    /// Teilnehmer des Sprachkanals, in dem man selbst ist.
    fn voice_members<'a>(&'a self, core: &'a Core, friends: &'a [Friend]) -> Element<'a, Msg> {
        let speaking = core
            .voice
            .core()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .speaking();
        let state_text = |muted: bool, deaf: bool| -> Option<&'static str> {
            if deaf {
                Some(tr("hears nothing", "hört nichts"))
            } else if muted {
                Some(tr("muted", "stumm"))
            } else {
                None
            }
        };
        let mut me = row![
            ui::avatar(&core.name, &core.user, 22.0),
            text(app_core::tr!("{} (you)", "{} (du)", core.name))
                .size(size::SMALL)
                .style(ui::tx::muted)
                .wrapping(text::Wrapping::None)
                .width(Length::Fill),
        ]
        .spacing(space::S)
        .align_y(Alignment::Center);
        if let Some(s) = state_text(self.muted, self.deafened) {
            me = me.push(text(s).size(size::OVERLINE).style(ui::tx::danger));
        }
        if core.stream.is_streaming() {
            me = me.push(ui::tag("LIVE", ui::bg::badge_live));
        }
        let mut col = column![container(me).padding([4, 8])].spacing(0);
        for p in core.voice.participants() {
            let name = Self::peer_name(friends, &p.user);
            let pref = core.peer_pref(&p.user);
            let talking = speaking.contains(&p.device);
            let mut line = row![
                ui::avatar_ring(
                    &name,
                    &p.user,
                    22.0,
                    talking.then_some(ui::speaking_ring as fn(&ui::Tokens) -> iced::Color),
                ),
                text(name)
                    .size(size::SMALL)
                    .style(if talking {
                        ui::tx::normal
                    } else {
                        ui::tx::muted
                    })
                    .wrapping(text::Wrapping::None)
                    .width(Length::Fill),
            ]
            .spacing(space::S)
            .align_y(Alignment::Center);
            if let Some(s) = state_text(p.muted, p.deafened) {
                line = line.push(text(s).size(size::OVERLINE).style(ui::tx::danger));
            }
            if pref.muted {
                line = line.push(
                    text(tr("muted for you", "für dich stumm"))
                        .size(size::OVERLINE)
                        .style(ui::tx::faint),
                );
            } else if (pref.volume - 1.0).abs() > 0.005 {
                line = line.push(ui::caption(format!("{:.0} %", pref.volume * 100.0)));
            }
            let menu_open = self.peer_menu == Some(p.user);
            let entry = iced::widget::mouse_area(
                button(line)
                    .padding([4, 8])
                    .width(Length::Fill)
                    .style(btn::nav(menu_open))
                    .on_press(Msg::PeerMenu(Some(p.user))),
            )
            .on_right_press(Msg::PeerMenu(Some(p.user)));
            let entry: Element<'a, Msg> = if core.stream.is_live(&p.device) {
                row![
                    entry,
                    ui::tip(
                        button(text("LIVE").size(size::OVERLINE).font(ui::BOLD))
                            .padding([3, 6])
                            .style(btn::danger)
                            .on_press(Msg::WatchStream(p.device)),
                        tr("Watch stream", "Stream ansehen"),
                        tooltip::Position::Top,
                    ),
                ]
                .spacing(4)
                .align_y(Alignment::Center)
                .into()
            } else {
                ui::tip(
                    entry,
                    tr(
                        "Click: volume only for you",
                        "Klicken: Lautstärke nur für dich",
                    ),
                    tooltip::Position::Right,
                )
            };
            col = col.push(entry);
            if menu_open {
                col = col.push(
                    container(
                        column![
                            row![
                                ui::caption(tr("Volume only for you", "Lautstärke nur für dich"))
                                    .width(Length::Fill),
                                text(format!("{:.0} %", pref.volume * 100.0))
                                    .size(size::CAPTION)
                                    .font(ui::SEMIBOLD),
                            ],
                            ui::slider(0.0..=2.0, pref.volume, move |v| {
                                Msg::PeerVolume(p.user, v)
                            })
                            .step(0.05),
                            ui::check(pref.muted, tr("Mute for me", "Für mich stummschalten"))
                                .on_toggle(move |m| Msg::PeerMute(p.user, m)),
                        ]
                        .spacing(space::S),
                    )
                    .padding(space::M)
                    .style(ui::bg::card),
                );
            }
        }
        container(col)
            .padding(iced::Padding::ZERO.left(space::L).bottom(space::XS))
            .into()
    }

    pub fn muted(&self) -> bool {
        self.muted
    }

    pub fn deafened(&self) -> bool {
        self.deafened
    }

    /// Name des Voice-Channels, in dem wir gerade sind (fuer Fenstertitel usw.).
    pub fn voice_channel_name(&self) -> Option<&str> {
        self.voice_label.as_ref().map(|(c, _)| c.as_str())
    }

    /// Sprach-Leiste unten in der Seitenleiste, solange man in einem Voice-Channel oder
    /// Gespraech ist. Sie bleibt auf jeder Seite sichtbar (Start, DMs, andere Spaces).
    pub fn voice_dock<'a>(
        &'a self,
        core: &'a Core,
        friends: &'a [Friend],
    ) -> Option<Element<'a, Msg>> {
        use app_audio::service::{CallState, CALL_SPACE};
        let cur = core.voice.current()?;
        let elapsed = core
            .voice
            .connected_since()
            .map(|t| ui::fmt_duration(t.elapsed()))
            .unwrap_or_default();
        let status = |label: &'static str| {
            row![
                text("●").size(size::CAPTION).style(ui::tx::success),
                text(label)
                    .size(size::SMALL)
                    .font(ui::SEMIBOLD)
                    .style(ui::tx::success)
                    .wrapping(text::Wrapping::None)
                    .width(Length::Fill),
                ui::caption(elapsed.clone()),
            ]
            .spacing(6)
            .align_y(Alignment::Center)
        };
        let (head, where_, leave): (_, Element<'a, Msg>, Element<'a, Msg>) =
            if cur.0 == CALL_SPACE {
                let peer = match core.voice.call_state() {
                    CallState::Active { user } => Some(user),
                    _ => None,
                };
                let name = peer.map_or_else(
                    || tr("Unknown", "Unbekannt").to_owned(),
                    |u| Self::peer_name(friends, &u),
                );
                let talking = peer.is_some_and(|u| {
                    let speaking = core
                        .voice
                        .core()
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .speaking();
                    core.voice
                        .participants()
                        .iter()
                        .any(|p| p.user == u && speaking.contains(&p.device))
                });
                let who = row![
                    match peer {
                        Some(u) => ui::avatar_ring(
                            &name,
                            &u,
                            20.0,
                            talking.then_some(ui::speaking_ring as fn(&ui::Tokens) -> iced::Color),
                        ),
                        None => Space::new().into(),
                    },
                    text(name)
                        .size(size::CAPTION)
                        .wrapping(text::Wrapping::None),
                ]
                .spacing(6)
                .align_y(Alignment::Center);
                let where_: Element<'a, Msg> = match peer {
                    Some(u) => ui::tip(
                        button(who)
                            .padding([2, 4])
                            .style(btn::ghost)
                            .on_press(Msg::OpenDm(u)),
                        tr("Open chat", "Chat öffnen"),
                        tooltip::Position::Top,
                    ),
                    None => who.into(),
                };
                (
                    status(tr("In a call", "Im Gespräch")),
                    where_,
                    ui::tip(
                        ui::button_small(tr("Hang up", "Auflegen"))
                            .style(btn::danger)
                            .on_press(Msg::HangUp),
                        tr("End call", "Gespräch beenden"),
                        tooltip::Position::Top,
                    ),
                )
            } else {
                let (ch, sp) = self.voice_label.clone().unwrap_or_else(|| {
                    (tr("Voice channel", "Sprachkanal").to_owned(), String::new())
                });
                let label = if sp.is_empty() {
                    ch
                } else {
                    format!("{ch} · {sp}")
                };
                (
                    status(tr("Voice connected", "Sprache verbunden")),
                    ui::tip(
                        button(
                            text(label)
                                .size(size::CAPTION)
                                .style(ui::tx::muted)
                                .wrapping(text::Wrapping::None),
                        )
                        .padding([2, 4])
                        .style(btn::ghost)
                        .on_press(Msg::Select(Some(cur.0))),
                        tr("Go to space", "Zum Space"),
                        tooltip::Position::Top,
                    ),
                    ui::tip(
                        ui::button_small(tr("Leave", "Verlassen"))
                            .style(btn::danger)
                            .on_press(Msg::LeaveVoice),
                        tr("Leave voice channel", "Sprachkanal verlassen"),
                        tooltip::Position::Top,
                    ),
                )
            };
        let stats_btn = ui::tip(
            ui::button_small("Stats")
                .style(btn::chip(self.voice_stats))
                .on_press(Msg::ToggleVoiceStats),
            tr(
                "Connection values: jitter, loss, buffer, path",
                "Verbindungswerte: Jitter, Verlust, Puffer, Pfad",
            ),
            tooltip::Position::Top,
        );
        let mut dock = column![
            head,
            row![container(where_).width(Length::Fill), stats_btn, leave]
                .spacing(space::S)
                .align_y(Alignment::Center),
        ]
        .spacing(space::XS);
        if self.voice_stats {
            dock = dock.push(Self::voice_stats_view(core, friends));
        }
        let toggle = |on_label: &'static str, off_label: &'static str, off: bool, m: Msg, tip| {
            ui::tip(
                ui::button_small(if off { off_label } else { on_label })
                    .width(Length::Fill)
                    .style(if off { btn::danger } else { btn::secondary })
                    .on_press(m),
                tip,
                tooltip::Position::Top,
            )
        };
        dock = dock.push(
            row![
                toggle(
                    tr("Mic on", "Mikro an"),
                    tr("Mic is off", "Mikro ist aus"),
                    self.muted,
                    Msg::ToggleMute,
                    if self.muted {
                        tr("Turn microphone back on", "Mikrofon wieder einschalten")
                    } else {
                        tr("Mute microphone", "Mikrofon stumm schalten")
                    }
                ),
                toggle(
                    tr("Sound on", "Ton an"),
                    tr("Sound is off", "Ton ist aus"),
                    self.deafened,
                    Msg::ToggleDeaf,
                    if self.deafened {
                        tr("Hear again", "Wieder hören")
                    } else {
                        tr(
                            "Stop hearing everyone (and mute)",
                            "Niemanden mehr hören (und stumm)",
                        )
                    }
                ),
            ]
            .spacing(space::XS),
        );
        dock = dock.push(match &self.share {
            Some(c) => {
                let mut share = column![row![
                    ui::caption(app_core::tr!("Sharing: {}", "Teilt: {}", c.source.title))
                        .wrapping(text::Wrapping::None)
                        .width(Length::Fill),
                    ui::button_small(tr("Change", "Ändern"))
                        .style(btn::ghost)
                        .on_press(Msg::OpenPicker),
                    ui::button_small(tr("Stop", "Beenden"))
                        .style(btn::danger_ghost)
                        .on_press(Msg::StopShare),
                ]
                .spacing(2)
                .align_y(Alignment::Center),]
                .spacing(space::XS);
                if !core.stream.is_streaming() {
                    share = share.push(
                        ui::button_small(tr("Start stream", "Stream starten"))
                            .width(Length::Fill)
                            .style(btn::primary)
                            .on_press(Msg::StartStream),
                    );
                }
                Element::from(share)
            }
            None => Element::from(
                ui::button_small(tr("Share screen …", "Bildschirm teilen …"))
                    .width(Length::Fill)
                    .style(btn::secondary)
                    .on_press(Msg::OpenPicker),
            ),
        });
        Some(
            container(dock)
                .padding(space::M)
                .width(Length::Fill)
                .style(|t: &iced::Theme| iced::widget::container::Style {
                    background: Some(iced::Background::Color(ui::tokens(t).surface)),
                    border: iced::Border::default().width(1).color(ui::tokens(t).border),
                    ..Default::default()
                })
                .into(),
        )
    }

    /// Seitenleiste der Startseite: Freunde und Direktnachrichten.
    fn home_sidebar<'a>(&'a self, core: &'a Core, friends: &'a [Friend]) -> Element<'a, Msg> {
        let requests = friends
            .iter()
            .filter(|f| f.status == FriendStatus::PendingIn)
            .count() as u32;
        let mut friends_row = row![text(tr("Friends", "Freunde"))
            .size(size::BODY)
            .font(ui::SEMIBOLD)
            .width(Length::Fill)]
        .align_y(Alignment::Center);
        if requests > 0 {
            friends_row = friends_row.push(ui::count_badge(requests, true));
        }
        let mut col = column![
            button(friends_row)
                .padding([8, 10])
                .width(Length::Fill)
                .style(btn::nav(self.sel.is_none()))
                .on_press(Msg::Select(None)),
            container(ui::overline(tr("Direct messages", "Direktnachrichten"))).padding(
                iced::Padding::ZERO
                    .top(space::L)
                    .bottom(space::XS)
                    .left(space::S)
            ),
        ]
        .spacing(2)
        .padding([space::S, space::S]);
        // Einladungen erst nach Zustimmung (M-2).
        if !self.invites.is_empty() {
            col = col.push(
                container(ui::overline(tr("Invitations", "Einladungen"))).padding(
                    iced::Padding::ZERO
                        .top(space::L)
                        .bottom(space::XS)
                        .left(space::S),
                ),
            );
        }
        for s in &self.invites {
            let from = s
                .creator
                .map_or_else(String::new, |c| Self::peer_name(friends, &c));
            col = col.push(
                container(
                    column![
                        text(s.name.as_str())
                            .size(size::BODY)
                            .font(ui::SEMIBOLD)
                            .wrapping(text::Wrapping::None),
                        ui::caption(app_core::tr!("from {}", "von {}", from)),
                        row![
                            ui::button_small(tr("Join", "Beitreten"))
                                .style(btn::primary)
                                .on_press(Msg::AcceptInvite(s.space_id)),
                            ui::button_small(tr("Decline", "Ablehnen"))
                                .style(btn::ghost)
                                .on_press(Msg::DeclineInvite(s.space_id)),
                        ]
                        .spacing(space::XS),
                    ]
                    .spacing(4),
                )
                .padding([space::XS, space::S]),
            );
        }
        let mut dms: Vec<(PublicKey, String, SpaceId)> = self
            .dms
            .iter()
            .map(|(u, (s, _))| (*u, Self::peer_name(friends, u), *s))
            .collect();
        dms.sort_by(|a, b| a.1.to_lowercase().cmp(&b.1.to_lowercase()));
        if dms.is_empty() {
            col = col.push(
                container(ui::caption(tr(
                    "No direct messages yet. Write to a friend via “Message”.",
                    "Noch keine Direktnachrichten. Schreib einem Freund über „Nachricht“.",
                )))
                .padding([space::XS, space::S]),
            );
        }
        for (user, name, space) in dms {
            let u = self.unread.1.get(&space).copied().unwrap_or_default();
            let selected = self.sel == Some(space);
            let status = core.presence.visible(&user).map(|p| p.status);
            let mut r = row![
                ui::avatar_status(&name, &user, 30.0, status, ui::on_sidebar),
                text(name)
                    .size(size::BODY)
                    .font(if u.unread > 0 {
                        ui::SEMIBOLD
                    } else {
                        iced::Font::DEFAULT
                    })
                    .wrapping(text::Wrapping::None)
                    .width(Length::Fill),
            ]
            .spacing(space::M - 2.0)
            .align_y(Alignment::Center);
            match core.voice.call_state() {
                app_audio::service::CallState::Active { user: c } if c == user => {
                    r = r.push(ui::tip(
                        ui::tag("●", ui::bg::badge_success),
                        tr("In a call", "Im Gespräch"),
                        tooltip::Position::Right,
                    ));
                }
                app_audio::service::CallState::Incoming { user: c }
                | app_audio::service::CallState::Outgoing { user: c }
                    if c == user =>
                {
                    r = r.push(ui::tag(tr("RINGING", "KLINGELT"), ui::bg::badge_accent));
                }
                _ => {}
            }
            if u.unread > 0 && !selected {
                r = r.push(ui::count_badge(u.unread, true));
            }
            col = col.push(
                button(r)
                    .padding([5, 8])
                    .width(Length::Fill)
                    .style(btn::nav_item(selected, u.unread > 0))
                    .on_press(Msg::OpenDm(user)),
            );
        }
        column![
            ui::header(
                text(tr("Home", "Start"))
                    .size(size::H3)
                    .font(ui::SEMIBOLD)
                    .style(ui::tx::normal),
                Space::new()
            ),
            ui::scroll(col).height(Length::Fill),
        ]
        .height(Length::Fill)
        .into()
    }

    /// Hauptbereich eines Spaces: Chat, Quellen-Auswahl oder Mitglieder und Einladungen.
    pub fn main<'a>(&'a self, core: &'a Core, friends: &'a [Friend]) -> Element<'a, Msg> {
        let name_of = |u: &PublicKey| -> String {
            if *u == core.user {
                return app_core::tr!("{} (you)", "{} (du)", core.name);
            }
            Self::peer_name(friends, u)
        };
        if let Some(p) = &self.picker {
            return p.view().map(Msg::Picker);
        }
        if self.chat.target.is_some() {
            let others: Vec<(PublicKey, String)> = self
                .view
                .members
                .iter()
                .map(|m| (m.user, name_of(&m.user)))
                .collect();
            let peer = self.dm_peer(core, friends);
            let header = match &peer {
                Some((u, n)) => ui::header(
                    row![
                        ui::avatar_status(
                            n,
                            u,
                            28.0,
                            core.presence.visible(u).map(|p| p.status),
                            ui::on_content,
                        ),
                        text(n.clone())
                            .size(size::H3)
                            .font(ui::SEMIBOLD)
                            .style(ui::tx::normal),
                        ui::caption(ui::status_label(core.presence.visible(u).map(|p| p.status))),
                    ]
                    .spacing(space::M - 2.0)
                    .align_y(Alignment::Center),
                    Self::call_controls(core, *u),
                ),
                None => ui::header(
                    row![
                        text("#").size(size::H2).style(ui::tx::faint),
                        text(self.chat.name.as_str())
                            .size(size::H3)
                            .font(ui::SEMIBOLD)
                            .style(ui::tx::normal),
                    ]
                    .spacing(6)
                    .align_y(Alignment::Center),
                    ui::tip(
                        ui::button_small(app_core::tr!(
                            "Members · {}",
                            "Mitglieder · {}",
                            self.view.members.len()
                        ))
                        .style(btn::secondary)
                        .on_press(Msg::ShowMembers),
                        tr("Members and invitations", "Mitglieder und Einladungen"),
                        tooltip::Position::Bottom,
                    ),
                ),
            };
            let chat = self
                .chat
                .view(&core.user, &core.name, &name_of, &others, &core.files, peer)
                .map(Msg::Chat);
            let mut page = column![header].height(Length::Fill);
            if let Some(w) = self.equivocation_warning(&name_of) {
                page = page.push(container(w).padding([space::S, space::L]));
            }
            return page.push(chat).into();
        }
        let admin = self.is_admin(core);
        let mut members = column![].spacing(2);
        for m in &self.view.members {
            let status = if m.user == core.user {
                Some(core.presence.local().status)
            } else {
                core.presence.visible(&m.user).map(|p| p.status)
            };
            let name = name_of(&m.user);
            let mut r = row![
                ui::avatar_status(&name, &m.user, 32.0, status, ui::on_surface),
                column![
                    text(name).size(size::BODY).font(ui::SEMIBOLD),
                    ui::caption(ui::status_label(status)),
                ]
                .spacing(1)
                .width(Length::Fill),
            ]
            .spacing(space::M)
            .align_y(Alignment::Center);
            if self.view.equivocators.contains(&m.user) {
                r = r.push(ui::tip(
                    ui::tag(tr("CONFLICT", "KONFLIKT"), ui::bg::badge_neutral),
                    tr(
                        "Signed two different versions of the same message.",
                        "Hat zwei verschiedene Fassungen derselben Nachricht signiert.",
                    ),
                    tooltip::Position::Left,
                ));
            }
            if m.admin {
                r = r.push(ui::tag("ADMIN", ui::bg::badge_neutral));
            }
            members = members.push(container(r).padding([6, 4]));
        }
        if let Some(w) = self.equivocation_warning(&name_of) {
            members = members.push(w);
        }
        let mut page = column![ui::muted(tr(
            "Who is in this space. Open a channel on the left to write.",
            "Wer in diesem Space ist. Öffne links einen Channel, um zu schreiben."
        ))]
        .spacing(space::L);
        if let Some(i) = &self.info {
            page = page.push(ui::notice(ui::Tone::Info, i.as_str()));
        }
        page = page.push(ui::section(
            app_core::tr!(
                "In the space · {}",
                "Im Space · {}",
                self.view.members.len()
            ),
            None,
            members,
        ));
        if admin {
            let mut inv = column![].spacing(2);
            let mut any = false;
            let has_friends = friends.iter().any(|f| f.status == FriendStatus::Accepted);
            for f in friends
                .iter()
                .filter(|f| f.status == FriendStatus::Accepted)
            {
                if self.view.members.iter().any(|m| m.user == f.user) {
                    continue;
                }
                any = true;
                let status = core.presence.visible(&f.user).map(|p| p.status);
                inv = inv.push(
                    container(
                        row![
                            ui::avatar_status(&f.name, &f.user, 32.0, status, ui::on_surface),
                            text(f.name.as_str())
                                .size(size::BODY)
                                .font(ui::SEMIBOLD)
                                .width(Length::Fill),
                            ui::button_small(tr("Invite", "Einladen"))
                                .style(btn::primary)
                                .on_press(Msg::Invite(f.user)),
                        ]
                        .spacing(space::M)
                        .align_y(Alignment::Center),
                    )
                    .padding([6, 4]),
                );
            }
            if !any {
                inv = inv.push(ui::muted(if has_friends {
                    tr("All your friends are already in.", "Alle deine Freunde sind schon dabei.")
                } else {
                    tr("You don't have any friends yet. Add them under “Home”, then you can invite them here.", "Du hast noch keine Freunde. Füge sie unter „Start“ hinzu, dann kannst du sie hier einladen.")
                }));
            }
            page = page.push(ui::section(
                tr("Invite friends", "Freunde einladen"),
                Some(tr(
                    "Offline friends get the invitation as soon as they are online.",
                    "Offline-Freunde bekommen die Einladung, sobald sie online sind.",
                )),
                inv,
            ));
        }
        column![
            ui::header(
                text(tr("Members", "Mitglieder"))
                    .size(size::H3)
                    .font(ui::SEMIBOLD)
                    .style(ui::tx::normal),
                Space::new(),
            ),
            ui::page(page),
        ]
        .height(Length::Fill)
        .into()
    }

    /// Diagnose-Zeilen fuer die Sprach-Leiste: je Sprecher die Empfangswerte, dazu der eigene
    /// Audio-Thread und verworfene Datagramme.
    fn voice_stats_view<'a>(core: &Core, friends: &[Friend]) -> Element<'a, Msg> {
        use app_net::PathKind;
        let line =
            |s: String| -> Element<'a, Msg> { ui::caption(s).font(iced::Font::MONOSPACE).into() };
        let parts = core.voice.participants();
        let stats = core
            .voice
            .core()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .rx_stats();
        let mut col = iced::widget::Column::new().spacing(2);
        if stats.is_empty() {
            col = col.push(line(
                tr("Nothing received yet", "Noch nichts empfangen").to_owned(),
            ));
        }
        for (dev, r) in &stats {
            let name = parts.iter().find(|p| p.device == *dev).map_or_else(
                || tr("Unknown", "Unbekannt").to_owned(),
                |p| Self::peer_name(friends, &p.user),
            );
            let path = match core.net.path_kind(dev) {
                PathKind::Direct => tr("direct", "direkt"),
                PathKind::Relay => "Relay",
                PathKind::None => tr("no path", "kein Pfad"),
            };
            col = col.push(line(format!("{name} · {path}")));
            col = col.push(line(app_core::tr!(
                "jitter {:.0} ms · buffer {}/{} ms",
                "Jitter {:.0} ms · Puffer {}/{} ms",
                r.jitter_ms,
                r.depth_ms,
                r.target_ms
            )));
            col = col.push(line(app_core::tr!(
                "loss {:.1} % · FEC {} · PLC {} · late {}",
                "Verlust {:.1} % · FEC {} · PLC {} · spät {}",
                r.loss_percent(),
                r.fec,
                r.plc,
                r.late
            )));
            col = col.push(line(app_core::tr!(
                "waited {} · skipped {} · dropouts {} · pauses {}",
                "gewartet {} · übersprungen {} · Aussetzer {} · Pausen {}",
                r.expand,
                r.accel,
                r.stall,
                r.rebuffer - r.stall
            )));
        }
        if let Some(d) = core.audio_diag() {
            col = col.push(line(app_core::tr!(
                "output {} ms · underruns {}",
                "Ausgabe {} ms · Aussetzer {}",
                d.out_ms,
                d.underruns
            )));
            col = col.push(line(app_core::tr!(
                "audio thread late {} · mic burst {}",
                "Audio-Thread spät {} · Mikro-Schub {}",
                d.late_ticks,
                d.capture_burst
            )));
        }
        col = col.push(line(app_core::tr!(
            "log: {}",
            "Protokoll: {}",
            crate::voicelog::dir().display()
        )));
        col = col.push(line(app_core::tr!(
            "datagrams dropped {}",
            "Datagramme verworfen {}",
            core.net.datagrams_dropped()
        )));
        col.into()
    }

    /// Anruf-Knoepfe im Kopf einer Direktnachricht: anrufen, annehmen, auflegen.
    fn call_controls<'a>(core: &Core, u: PublicKey) -> Element<'a, Msg> {
        use app_audio::service::{CallState, CALL_SPACE};
        let online = core.presence.visible(&u).is_some();
        let bar = |items: Vec<Element<'a, Msg>>| -> Element<'a, Msg> {
            iced::widget::Row::with_children(items)
                .spacing(space::S)
                .align_y(Alignment::Center)
                .into()
        };
        match core.voice.call_state() {
            CallState::Active { user } if user == u => {
                let since = core
                    .voice
                    .connected_since()
                    .map(|t| ui::fmt_duration(t.elapsed()))
                    .unwrap_or_default();
                bar(vec![
                    ui::tag(tr("IN CALL", "IM GESPRÄCH"), ui::bg::badge_success),
                    ui::caption(since).into(),
                    ui::button_small(tr("Hang up", "Auflegen"))
                        .style(btn::danger)
                        .on_press(Msg::HangUp)
                        .into(),
                ])
            }
            CallState::Incoming { user } if user == u => bar(vec![
                ui::caption(tr("is calling …", "ruft an …")).into(),
                ui::button_small(tr("Accept", "Annehmen"))
                    .style(btn::success)
                    .on_press(Msg::AcceptCall)
                    .into(),
                ui::button_small(tr("Decline", "Ablehnen"))
                    .style(btn::danger)
                    .on_press(Msg::DeclineCall)
                    .into(),
            ]),
            CallState::Outgoing { user } if user == u => bar(vec![
                ui::caption(tr("Ringing …", "Es klingelt …")).into(),
                ui::button_small(tr("Cancel", "Abbrechen"))
                    .style(btn::danger)
                    .on_press(Msg::CancelCall)
                    .into(),
            ]),
            CallState::Idle => {
                let in_channel = core.voice.current().is_some_and(|c| c.0 != CALL_SPACE);
                ui::tip(
                    ui::button_small(tr("Call", "Anrufen"))
                        .style(btn::success)
                        .on_press_maybe(online.then_some(Msg::CallUser(u))),
                    if !online {
                        tr(
                            "Calling only works when your friend is online",
                            "Anrufen geht nur, wenn dein Freund online ist",
                        )
                    } else if in_channel {
                        tr(
                            "Leaves your voice channel and starts a call",
                            "Verlässt deinen Sprachkanal und ruft an",
                        )
                    } else {
                        tr("Start voice call", "Sprachanruf starten")
                    },
                    tooltip::Position::Bottom,
                )
            }
            _ => ui::tip(
                ui::button_small(tr("Call", "Anrufen")).style(btn::secondary),
                tr("You are already in a call", "Du bist schon in einem Anruf"),
                tooltip::Position::Bottom,
            ),
        }
    }

    /// Mikrofon-Einstellungen: Rauschunterdrückung, Sprachaktivierung, Verstaerkung, Test.
    pub fn audio_panel<'a>(&'a self, core: &Core) -> Element<'a, Msg> {
        use iced::widget::progress_bar;
        let s = core.input();
        let (level, open) = core.input_level();
        let db = 20.0 * (level + 1e-9).log10();
        let meter = ((db + 80.0) / 80.0).clamp(0.0, 1.0);
        let mut col = column![
            ui::setting(
                tr("Noise suppression", "Rauschunterdrückung"),
                Some(
                    tr(
                        "Filters fans, keyboard and other background noise.",
                        "Filtert Lüfter, Tastatur und andere Hintergrundgeräusche."
                    )
                    .to_owned()
                ),
                ui::toggle(s.denoise).on_toggle(Msg::Denoise),
            ),
            ui::setting(
                tr("Voice activation", "Sprachaktivierung"),
                Some(
                    tr(
                        "Only transmits when you speak louder than the threshold.",
                        "Sendet nur, wenn du lauter als die Schwelle sprichst."
                    )
                    .to_owned()
                ),
                ui::toggle(s.voice_activation).on_toggle(Msg::VoiceActivation),
            ),
        ]
        .spacing(space::L);
        if s.voice_activation {
            col = col.push(ui::slider_field(
                tr("Threshold", "Schwelle"),
                format!("{:.0} dB", s.threshold_db),
                ui::slider(-80.0..=-10.0, s.threshold_db, Msg::Threshold),
            ));
        }
        col.push(ui::setting(
            tr("Automatic volume", "Automatische Lautstärke"),
            Some(
                tr(
                    "Evens out quiet and loud speech.",
                    "Gleicht leises und lautes Sprechen aus.",
                )
                .to_owned(),
            ),
            ui::toggle(s.agc).on_toggle(Msg::Agc),
        ))
        .push(ui::slider_field(
            tr("Input gain", "Eingangsverstärkung"),
            format!("{:.0} %", s.gain * 100.0),
            ui::slider(0.0..=2.0, s.gain, Msg::Gain).step(0.05),
        ))
        .push(
            column![
                row![
                    ui::label(tr("Level", "Pegel")).width(Length::Fill),
                    text(if open {
                        tr("transmitting", "sendet")
                    } else {
                        tr("not transmitting", "sendet nicht")
                    })
                    .size(size::CAPTION)
                    .style(if open { ui::tx::success } else { ui::tx::faint }),
                ],
                progress_bar(0.0..=1.0, meter)
                    .girth(8)
                    .style(ui::meter_style),
            ]
            .spacing(6),
        )
        .push(ui::setting(
            tr("Hear myself", "Mich selbst hören"),
            Some(
                tr(
                    "For testing – headphones only, otherwise it will squeal.",
                    "Zum Testen – nur mit Kopfhörern, sonst pfeift es.",
                )
                .to_owned(),
            ),
            ui::toggle(core.monitoring()).on_toggle(Msg::Monitor),
        ))
        .into()
    }

    /// Overlay im Spiel: an/aus, Ecke, Groesse, Transparenz.
    pub fn overlay_panel<'a>(&'a self, core: &Core) -> Element<'a, Msg> {
        let o = core.overlay();
        let mut corners = row![].spacing(6);
        for (i, label) in [
            tr("top left", "oben links"),
            tr("top right", "oben rechts"),
            tr("bottom left", "unten links"),
            tr("bottom right", "unten rechts"),
        ]
        .into_iter()
        .enumerate()
        {
            corners = corners.push(
                ui::button_small(label)
                    .style(btn::chip(o.corner as usize == i))
                    .on_press(Msg::Ov(OvChange::Corner(i as u8))),
            );
        }
        column![
            ui::setting(
                tr("Show overlay", "Overlay anzeigen"),
                Some(tr("Shows who is speaking in the channel while in a game.", "Zeigt im Spiel, wer im Channel spricht.").to_owned()),
                ui::toggle(o.enabled).on_toggle(|v| Msg::Ov(OvChange::Enabled(v))),
            ),
            ui::setting(
                tr("Show only speakers", "Nur Sprechende zeigen"),
                None,
                ui::toggle(o.only_speaking).on_toggle(|v| Msg::Ov(OvChange::OnlySpeaking(v))),
            ),
            ui::field("Position", corners.wrap(), None),
            ui::slider_field(
                tr("Size", "Größe"),
                format!("{:.0} %", o.scale * 100.0),
                ui::slider(0.7..=2.0, o.scale, |v| Msg::Ov(OvChange::Scale(v)))
                    .step(0.1),
            ),
            ui::slider_field(
                tr("Opacity", "Deckkraft"),
                format!("{:.0} %", o.opacity * 100.0),
                ui::slider(0.1..=1.0, o.opacity, |v| Msg::Ov(OvChange::Opacity(v)))
                    .step(0.05),
            ),
            ui::notice(
                ui::Tone::Info,
                tr("Only works if the game runs in windowed or borderless mode, not in exclusive fullscreen.", "Funktioniert nur, wenn das Spiel im Fenster- oder randlosen Modus läuft, nicht im exklusiven Vollbild."),
            ),
        ]
        .spacing(space::L)
        .into()
    }

    /// Push-to-Talk, Stumm- und Taub-Taste belegen.
    pub fn hotkey_panel<'a>(&'a self, core: &Core) -> Element<'a, Msg> {
        let s = core.hotkeys.settings();
        let capturing = core.hotkeys.capturing();
        let mut col = column![].spacing(space::L);
        if !core.hotkeys.available() {
            col = col.push(ui::notice(
                ui::Tone::Danger,
                tr(
                    "Global keys are not available.",
                    "Globale Tasten sind nicht verfügbar.",
                ),
            ));
        }
        col = col.push(ui::setting(
            "Push-to-Talk",
            Some(
                tr(
                    "Only transmit while the key is held.",
                    "Nur senden, solange die Taste gehalten wird.",
                )
                .to_owned(),
            ),
            ui::toggle(s.ptt_enabled).on_toggle(Msg::HkPtt),
        ));
        let mut keys = column![].spacing(space::S);
        for (a, label) in [
            (Action::PushToTalk, "Push-to-Talk"),
            (Action::ToggleMute, tr("Toggle mute", "Stumm schalten")),
            (Action::ToggleDeafen, tr("Toggle deafen", "Taub schalten")),
            (
                Action::ToggleOverlay,
                tr("Toggle overlay", "Overlay an/aus"),
            ),
            (Action::ToggleStream, tr("Toggle stream", "Stream an/aus")),
        ] {
            let listening = capturing == Some(a);
            let bound = s.binding(a);
            let shown = if listening {
                tr("Press a key …", "Taste drücken …").to_owned()
            } else {
                bound.map_or_else(|| tr("not bound", "nicht belegt").to_owned(), binding_name)
            };
            let mut r =
                row![
                    text(label).size(size::BODY).width(Length::Fill),
                    ui::tip(
                        button(text(shown).size(size::SMALL).font(
                            if bound.is_some() || listening {
                                ui::SEMIBOLD
                            } else {
                                iced::Font::DEFAULT
                            }
                        ))
                        .padding([5, 12])
                        .width(Length::Fixed(160.0))
                        .style(btn::chip(listening))
                        .on_press(if listening {
                            Msg::HkCancel
                        } else {
                            Msg::HkCapture(a)
                        }),
                        if listening {
                            tr("Click to cancel", "Klicken zum Abbrechen")
                        } else {
                            tr(
                                "Click, then press the desired key",
                                "Klicken und dann die gewünschte Taste drücken",
                            )
                        },
                        tooltip::Position::Top,
                    ),
                ]
                .spacing(space::S)
                .align_y(Alignment::Center);
            r = r.push(if bound.is_some() {
                mini(
                    "✕",
                    tr("Remove binding", "Belegung entfernen"),
                    Msg::HkClear(a),
                    btn::ghost,
                )
            } else {
                Space::new().width(26).into()
            });
            keys = keys.push(r);
        }
        col = col.push(keys).push(ui::slider_field(
            tr("Release delay", "Nachlauf nach dem Loslassen"),
            format!("{} ms", s.ptt_release_ms),
            ui::slider(0.0..=1000.0, s.ptt_release_ms as f32, Msg::HkRelease).step(50.0),
        ));
        // Kein "Als Administrator neu starten" mehr (M-9): Die Exe liegt in einem fuer den
        // Nutzer beschreibbaren Ordner, Schadsoftware koennte sie vorher austauschen; und die
        // ganze netzwerkoffene App liefe dann mit Adminrechten.
        if !app_hotkeys::is_elevated() {
            col = col.push(ui::banner(
                ui::Tone::Info,
                text(tr(
                    "If a game runs as administrator, Windows does not pass your keys to Campfire while the game is in front. Campfire does not offer to run as administrator, for your safety.",
                    "Läuft ein Spiel als Administrator, gibt Windows deine Tasten nicht an Campfire weiter, solange das Spiel im Vordergrund ist. Als Administrator zu starten bietet Campfire zu deiner Sicherheit nicht an.",
                ))
                .size(size::SMALL),
            ));
        }
        col.into()
    }

    /// Eingabe fuer einen neuen Space (Startseite).
    pub fn create_form(&self) -> Element<'_, Msg> {
        let mut col = column![row![
            ui::input(
                tr("Name, e.g. “Game night”", "Name, z. B. „Zockerabend“"),
                &self.new_space
            )
            .id(NEW_SPACE_INPUT)
            .on_input(Msg::NewSpaceName)
            .on_submit(Msg::CreateSpace)
            .width(Length::Fill),
            ui::button_text(tr("Create", "Anlegen"))
                .style(btn::primary)
                .on_press_maybe((!self.new_space.trim().is_empty()).then_some(Msg::CreateSpace)),
        ]
        .spacing(space::S)
        .align_y(Alignment::Center)]
        .spacing(space::M);
        if let Some(i) = &self.info {
            col = col.push(ui::notice(ui::Tone::Danger, i.as_str()));
        }
        ui::section(
            tr("Create new space", "Neuen Space anlegen"),
            Some(tr("A space is a group with its own text and voice channels, for example for your team or your gaming group.", "Ein Space ist eine Gruppe mit eigenen Text- und Sprachkanälen, zum Beispiel für dein Team oder deine Spielrunde.")),
            col,
        )
    }
}

/// Symbol in der Space-Leiste mit Auswahl-Markierung links und optionaler Plakette.
fn rail_item<'a>(
    label: Element<'a, Msg>,
    selected: bool,
    unread: bool,
    badge: Option<Element<'a, Msg>>,
    tip: String,
    on_press: Msg,
) -> Element<'a, Msg> {
    let pill_height = if selected {
        36.0
    } else if unread {
        8.0
    } else {
        0.0
    };
    let pill = container(container(Space::new()).width(4).height(pill_height).style(
        |t: &iced::Theme| iced::widget::container::Style {
            background: Some(iced::Background::Color(ui::tokens(t).text)),
            border: iced::Border::default().rounded(iced::border::Radius {
                top_left: 0.0,
                top_right: 3.0,
                bottom_right: 3.0,
                bottom_left: 0.0,
            }),
            ..Default::default()
        },
    ))
    .width(12)
    .height(48)
    .align_y(Alignment::Center);
    let b = button(container(label).center(Length::Fixed(48.0)))
        .padding(0)
        .width(48)
        .height(48)
        .style(btn::rail(selected))
        .on_press(on_press);
    let icon: Element<'a, Msg> = match badge {
        Some(badge) => stack![
            b,
            container(badge)
                .align_right(Length::Fill)
                .align_bottom(Length::Fill)
        ]
        .into(),
        None => b.into(),
    };
    row![pill, ui::tip(icon, tip, tooltip::Position::Right)]
        .align_y(Alignment::Center)
        .into()
}

/// Kleiner Symbolknopf mit Tooltip (Channel-Verwaltung).
fn mini<'a>(
    glyph: &'a str,
    label: &'a str,
    m: Msg,
    style: fn(&iced::Theme, button::Status) -> button::Style,
) -> Element<'a, Msg> {
    ui::tip(
        button(text(glyph).size(size::SMALL).line_height(1.0))
            .padding([4, 7])
            .style(style)
            .on_press(m),
        label,
        tooltip::Position::Top,
    )
}
