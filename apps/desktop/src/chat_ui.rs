//! Chat-Ansicht und Eingabefeld eines Text-Channels (M2-02, M2-03).
//!
//! Es wird immer nur ein Fenster der Nachrichten gehalten (neueste Seite, aelteres per Knopf
//! nachgeladen); die Datenbank liefert eine Seite in Millisekunden, auch bei 100.000 Nachrichten.

use crate::core::Core;
use crate::files::{FileService, FileState};
use crate::markdown::{parse, Span};
use crate::ui::{self, btn, radius, size, space, tx, Tone};
use app_core::eventlog::chat::{page, Attachment, ChatMessage, Cursor, Delivery};
use app_core::eventlog::unread;
use app_core::eventlog::MAX_BODY_BYTES;
use app_core::files::{format_size, is_risky};
use app_core::i18n::tr;
use app_proto::event::{ChannelId, EventId, EventKind, SpaceId};
use app_proto::PublicKey;
use iced::font::Style;
use iced::keyboard::key::Named;
use iced::keyboard::Key;
use iced::widget::text_editor::{Action, Binding, Content, Edit, Motion, Status};
use iced::widget::{
    button, column, container, mouse_area, rich_text, row, scrollable, span, stack, text,
    text_editor, tooltip, Space,
};
use iced::{Alignment, Background, Border, Color, Element, Font, Length, Task, Theme};
use std::collections::HashMap;
use std::sync::Arc;

const PAGE: usize = 50;
/// Hoechstens so viele Dateien auf einmal.
const MAX_PENDING: usize = 20;
/// Mehr Nachrichten werden im Speicher nie gehalten (aelteste fallen beim Aktualisieren weg).
const MAX_KEPT: usize = 1000;
/// Nachrichten desselben Autors innerhalb dieser Zeit werden zusammengefasst.
const GROUP_MS: i64 = 5 * 60 * 1000;
const EMOJI: &[&str] = &[
    "😀", "😂", "🙂", "😉", "😍", "😎", "🤔", "😅", "😭", "😡", "👍", "👎", "👏", "🙏", "💪", "🎉",
    "🔥", "❤️", "💯", "✅", "❌", "👀", "🎮", "🍕",
];

fn list_id() -> iced::widget::Id {
    iced::widget::Id::new("chat-list")
}

#[derive(Debug, Clone)]
pub enum Msg {
    Latest(u64, Vec<ChatMessage>),
    Older(u64, Vec<ChatMessage>),
    LoadOlder,
    Link(String),
    Jump,
    Scrolled(scrollable::Viewport),
    Edit(Action),
    Send,
    Sent(Result<(), String>),
    EditLast,
    CancelCompose,
    Menu(Option<EventId>),
    /// Maus ueber einer Nachricht (zeigt deren Aktionen).
    Hover(EventId),
    Unhover(EventId),
    Reply(EventId, String),
    StartEdit(EventId),
    PickMention(String),
    ToggleEmoji,
    Emoji(&'static str),
    /// (Generation, Lesestand beim Oeffnen, Anzahl Ungelesener)
    Opened(u64, i64, u32),
    /// Lesestand wurde gespeichert.
    Marked,
    JumpUnread,
    /// Dateiauswahl oeffnen.
    PickFiles,
    Picked(Vec<std::path::PathBuf>),
    /// Datei per Drag & Drop auf das Fenster gezogen.
    Dropped(std::path::PathBuf),
    /// Strg+V: Dateien aus der Zwischenablage anhaengen, sonst Text einfuegen.
    Paste,
    RemovePending(usize),
    Download(app_proto::Hash, u64),
    /// Vorschaubild laden (nur per Klick, wenn automatische Vorschauen aus sind).
    LoadPreview(app_proto::Hash),
    /// Datei oeffnen (bei riskanten Typen erst nach Rueckfrage).
    OpenFile(app_proto::Hash, String),
    ConfirmOpen,
    CancelOpen,
    /// In "Downloads" speichern.
    SaveFile(app_proto::Hash, String),
    /// Bild gross anzeigen (Hash, Name).
    ShowImage(app_proto::Hash, String),
    ImageLoaded(Option<iced::widget::image::Handle>),
    CloseImage,
    /// tr("Save as …", "Speichern unter …") (Hash, Name).
    SaveAs(app_proto::Hash, String),
    SaveAsPicked(app_proto::Hash, Option<std::path::PathBuf>),
    TogglePin(app_proto::Hash),
    FileDone(Result<(std::path::PathBuf, bool), String>),
}

#[derive(Default)]
pub struct Chat {
    pub target: Option<(SpaceId, ChannelId)>,
    pub name: String,
    messages: Vec<ChatMessage>,
    has_older: bool,
    loading: bool,
    at_bottom: bool,
    generation: u64,
    last_limit: usize,
    input: Content,
    /// Entwuerfe je Channel, bleiben beim Wechsel erhalten.
    drafts: HashMap<ChannelId, String>,
    editing: Option<EventId>,
    reply: Option<(EventId, String)>,
    menu: Option<EventId>,
    hovered: Option<EventId>,
    emoji_open: bool,
    /// Dateien, die mit dem naechsten Senden geteilt werden.
    pending: Vec<std::path::PathBuf>,
    /// Riskante Datei wartet auf Bestaetigung zum Oeffnen.
    confirm_open: Option<(app_proto::Hash, String)>,
    /// Hinweis nach Speichern/Oeffnen.
    file_note: Option<String>,
    /// Grossansicht eines Bildes.
    viewer: Option<iced::widget::image::Handle>,
    error: Option<String>,
    /// Fenster hat den Fokus; nur dann zaehlt eine Nachricht als gelesen.
    focused: bool,
    /// Lesestand beim Oeffnen: Nachrichten danach stehen unter der Linie "Neue Nachrichten".
    marker: i64,
    marked_wall: i64,
    /// Banner "Neue Nachrichten" wurde bedient (Klick/Interaktion) und bleibt weg.
    banner_off: bool,
}

fn content_with(text: &str) -> Content {
    let mut c = Content::with_text(text);
    c.perform(Action::Move(Motion::DocumentEnd));
    c
}

/// Das letzte Wort des Textes, wenn es eine begonnene Erwaehnung ist (`@ab`).
pub fn mention_prefix(text: &str) -> Option<&str> {
    if text.ends_with(char::is_whitespace) {
        return None;
    }
    let last = text.rsplit(char::is_whitespace).next()?;
    last.strip_prefix('@')
}

/// Ersetzt die begonnene Erwaehnung am Ende durch `@name `.
pub fn complete_mention(text: &str, name: &str) -> String {
    match mention_prefix(text) {
        Some(p) => format!("{}@{name} ", &text[..text.len() - p.len() - 1]),
        None => text.to_owned(),
    }
}

impl Chat {
    fn stash_draft(&mut self) {
        if let Some((_, ch)) = self.target {
            let t = self.input.text();
            let t = t.trim_end_matches('\n').to_owned();
            if t.trim().is_empty() || self.editing.is_some() {
                self.drafts.remove(&ch);
            } else {
                self.drafts.insert(ch, t);
            }
        }
    }

    fn reset_compose(&mut self) {
        self.editing = None;
        self.reply = None;
        self.menu = None;
        self.emoji_open = false;
        self.error = None;
    }

    pub fn open(
        &mut self,
        space: SpaceId,
        channel: ChannelId,
        name: String,
        core: &Core,
    ) -> Task<Msg> {
        self.stash_draft();
        self.reset_compose();
        self.target = Some((space, channel));
        self.name = name;
        self.messages.clear();
        self.has_older = false;
        self.loading = false;
        self.at_bottom = true;
        self.generation += 1;
        self.last_limit = PAGE;
        self.input = content_with(self.drafts.get(&channel).map_or("", String::as_str));
        self.marker = 0;
        self.banner_off = false;
        self.marked_wall = 0;
        let (db, me, g, my_name) = (
            core.db.clone(),
            core.user,
            self.generation,
            core.name.clone(),
        );
        Task::perform(
            async move {
                tokio::task::spawn_blocking(move || {
                    db.call(move |c| {
                        let m = unread::last_read(c, &channel)?;
                        let n = unread::channel_unread(c, &space, &me, &my_name)?
                            .get(&channel)
                            .map_or(0, |u| u.unread);
                        Ok((m, n))
                    })
                    .unwrap_or_default()
                })
                .await
                .unwrap_or_default()
            },
            move |(m, n)| Msg::Opened(g, m, n),
        )
    }

    /// Fenster hat Fokus bekommen oder verloren.
    pub fn focused(&self) -> bool {
        self.focused
    }

    pub fn set_focus(&mut self, focused: bool, core: &Core) -> Task<Msg> {
        self.focused = focused;
        self.mark_read(core)
    }

    /// Speichert den Lesestand, wenn der Channel sichtbar und bis unten gelesen ist.
    fn mark_read(&mut self, core: &Core) -> Task<Msg> {
        let (Some((_, channel)), true, true) = (self.target, self.focused, self.at_bottom) else {
            return Task::none();
        };
        let Some(wall) = self.messages.last().map(|m| m.cursor.wall_ms) else {
            return Task::none();
        };
        if wall <= self.marked_wall {
            return Task::none();
        }
        self.marked_wall = wall;
        let db = core.db.clone();
        Task::perform(
            async move {
                let _ = tokio::task::spawn_blocking(move || {
                    db.call(move |c| unread::mark_read(c, &channel, wall))
                })
                .await;
            },
            |()| Msg::Marked,
        )
    }

    /// Index der ersten Nachricht unter der Linie "Neue Nachrichten".
    fn first_new(&self, me: &PublicKey) -> Option<usize> {
        if self.marker == 0 {
            return None;
        }
        self.messages
            .iter()
            .position(|m| m.cursor.wall_ms > self.marker && m.author != *me && !m.deleted)
    }

    pub fn close(&mut self) {
        self.stash_draft();
        self.reset_compose();
        self.target = None;
        self.hovered = None;
        self.messages.clear();
        self.pending.clear();
        self.generation += 1;
    }

    /// Fragt den Speicher, welche angehaengten Dateien schon lokal liegen.
    fn probe_files(&self, core: &Core) {
        let hashes: Vec<_> = self
            .messages
            .iter()
            .flat_map(|m| m.attachments.iter().map(|a| a.hash))
            .collect();
        let thumbs: Vec<_> = self
            .messages
            .iter()
            .flat_map(|m| m.attachments.iter().filter_map(|a| a.thumb))
            .collect();
        if !hashes.is_empty() {
            core.files.probe(hashes);
        }
        let Some((space, _)) = self.target else {
            return;
        };
        if !thumbs.is_empty() {
            core.files.want_thumbs(thumbs, space);
        }
        let remote: Vec<_> = self
            .messages
            .iter()
            .flat_map(|m| m.attachments.iter().map(|a| (a.hash, space)))
            .collect();
        core.files.check_availability(remote, false);
    }

    fn add_pending(&mut self, paths: impl IntoIterator<Item = std::path::PathBuf>) {
        if self.target.is_none() || self.editing.is_some() {
            return;
        }
        for p in paths {
            if !self.pending.contains(&p) && self.pending.len() < MAX_PENDING {
                self.pending.push(p);
            }
        }
    }

    /// Neue Nachrichten holen (nach Sync/Aenderung); behaelt die geladene Laenge bei.
    pub fn refresh(&mut self, core: &Core) -> Task<Msg> {
        if self.target.is_none() {
            return Task::none();
        }
        let n = self.messages.len().clamp(PAGE, MAX_KEPT);
        self.last_limit = n;
        self.load(core, None, n, false)
    }

    fn load(&self, core: &Core, before: Option<Cursor>, limit: usize, older: bool) -> Task<Msg> {
        let Some((space, channel)) = self.target else {
            return Task::none();
        };
        let (db, me, g) = (core.db.clone(), core.user, self.generation);
        Task::perform(
            async move {
                tokio::task::spawn_blocking(move || {
                    db.call(move |c| page(c, &space, &channel, before.as_ref(), limit, &me))
                        .unwrap_or_default()
                })
                .await
                .unwrap_or_default()
            },
            move |m| {
                if older {
                    Msg::Older(g, m)
                } else {
                    Msg::Latest(g, m)
                }
            },
        )
    }

    fn last_own(&self, me: &PublicKey) -> Option<&ChatMessage> {
        self.messages
            .iter()
            .rev()
            .find(|m| m.author == *me && !m.deleted)
    }

    fn start_edit(&mut self, id: EventId) {
        if let Some(m) = self.messages.iter().find(|m| m.id == id) {
            self.input = content_with(&m.body);
            self.editing = Some(id);
            self.reply = None;
            self.menu = None;
        }
    }

    fn send(&mut self, core: &Core) -> Task<Msg> {
        let Some((space, channel)) = self.target else {
            return Task::none();
        };
        let body = self.input.text().trim().to_owned();
        if self.editing.is_none() && !self.pending.is_empty() {
            let paths = std::mem::take(&mut self.pending);
            core.files
                .share(space, channel, (!body.is_empty()).then_some(body), paths);
            self.input = Content::new();
            self.reset_compose();
            self.drafts.remove(&channel);
            return Task::none();
        }
        if body.is_empty() {
            return Task::none();
        }
        if body.len() > MAX_BODY_BYTES {
            self.error = Some(app_core::tr!(
                "The message is too long ({} of {MAX_BODY_BYTES} characters).",
                "Die Nachricht ist zu lang ({} von {MAX_BODY_BYTES} Zeichen).",
                body.len()
            ));
            return Task::none();
        }
        let kind = match self.editing {
            Some(target) => EventKind::MessageEdit { target, body },
            None => EventKind::Message {
                channel,
                body,
                reply_to: self.reply.as_ref().map(|(id, _)| *id),
            },
        };
        let sync = core.sync.clone();
        // Eingabe sofort leeren; schlaegt das Senden fehl, steht der Fehler unter dem Feld.
        self.input = Content::new();
        self.reset_compose();
        self.drafts.remove(&channel);
        Task::perform(
            async move {
                sync.publish(Some(space), kind)
                    .await
                    .map(|_| ())
                    .map_err(|e| e.to_string())
            },
            Msg::Sent,
        )
    }

    /// Datei aus dem Speicher nach "Downloads" (Speichern) bzw. in den Temp-Ordner (Oeffnen) schreiben.
    fn export_and_open(core: &Core, hash: app_proto::Hash, name: String, open: bool) -> Task<Msg> {
        let files = core.files.clone();
        let dir = if open {
            crate::files::opened_dir_for(&hash)
        } else {
            downloads_dir()
        };
        Task::perform(
            async move { files.export_to(&hash, &name, &dir).await.map(|p| (p, open)) },
            Msg::FileDone,
        )
    }

    pub fn update(&mut self, msg: Msg, core: &Core) -> Task<Msg> {
        match msg {
            Msg::Latest(g, msgs) if g == self.generation => {
                self.has_older = msgs.len() >= self.last_limit;
                let stay = self.at_bottom;
                self.messages = msgs;
                self.probe_files(core);
                let mark = self.mark_read(core);
                if stay {
                    return Task::batch([iced::widget::operation::snap_to_end(list_id()), mark]);
                }
                return mark;
            }
            Msg::Opened(g, marker, n) if g == self.generation => {
                self.marker = marker;
                self.marked_wall = marker;
                self.last_limit = (n as usize + 5).clamp(PAGE, MAX_KEPT);
                return self.load(core, None, self.last_limit, false);
            }
            Msg::Opened(..) => {}
            Msg::PickFiles => {
                return Task::perform(
                    async {
                        rfd::AsyncFileDialog::new()
                            .set_title(tr("Share files", "Dateien teilen"))
                            .pick_files()
                            .await
                            .map(|v| v.into_iter().map(|f| f.path().to_path_buf()).collect())
                            .unwrap_or_default()
                    },
                    Msg::Picked,
                );
            }
            Msg::Picked(paths) => self.add_pending(paths),
            Msg::Dropped(p) => self.add_pending([p]),
            Msg::Paste => {
                let files = crate::platform::clipboard_files();
                if files.is_empty() {
                    return iced::clipboard::read().map(|t| {
                        Msg::Edit(Action::Edit(Edit::Paste(Arc::new(t.unwrap_or_default()))))
                    });
                }
                self.add_pending(files);
            }
            Msg::RemovePending(i) => {
                if i < self.pending.len() {
                    self.pending.remove(i);
                }
            }
            Msg::Download(h, size) => {
                if let Some((space, _)) = self.target {
                    core.files.download(h, size, space);
                }
            }
            Msg::LoadPreview(h) => {
                if let Some((space, _)) = self.target {
                    core.files.load_thumb(h, space);
                }
            }
            Msg::OpenFile(h, name) => {
                if is_risky(&name) {
                    self.confirm_open = Some((h, name));
                } else {
                    return Self::export_and_open(core, h, name, true);
                }
            }
            Msg::ConfirmOpen => {
                if let Some((h, name)) = self.confirm_open.take() {
                    return Self::export_and_open(core, h, name, true);
                }
            }
            Msg::CancelOpen => self.confirm_open = None,
            Msg::ShowImage(h, name) => {
                let files = core.files.clone();
                return Task::perform(async move { files.load_image(&h).await }, move |i| {
                    if i.is_none() {
                        Msg::OpenFile(h, name.clone())
                    } else {
                        Msg::ImageLoaded(i)
                    }
                });
            }
            Msg::ImageLoaded(h) => self.viewer = h,
            Msg::SaveAs(h, name) => {
                return Task::perform(
                    async move {
                        let f = rfd::AsyncFileDialog::new()
                            .set_title(tr("Save as", "Speichern unter"))
                            .set_file_name(app_core::files::sanitize_file_name(&name))
                            .save_file()
                            .await;
                        (h, f.map(|f| f.path().to_path_buf()))
                    },
                    |(h, p)| Msg::SaveAsPicked(h, p),
                );
            }
            Msg::SaveAsPicked(h, Some(path)) => {
                let files = core.files.clone();
                return Task::perform(
                    async move { files.export_path(&h, &path).await.map(|p| (p, false)) },
                    Msg::FileDone,
                );
            }
            Msg::SaveAsPicked(_, None) => {}
            Msg::TogglePin(h) => core.files.toggle_pin(h),
            Msg::CloseImage => self.viewer = None,
            Msg::SaveFile(h, name) => return Self::export_and_open(core, h, name, false),
            Msg::FileDone(r) => match r {
                Ok((path, open)) => {
                    if open {
                        crate::platform::open_path(&path);
                        self.file_note = None;
                    } else {
                        crate::platform::reveal_path(&path);
                        self.file_note = Some(app_core::tr!(
                            "Saved: {}",
                            "Gespeichert: {}",
                            path.display()
                        ));
                    }
                }
                Err(e) => self.file_note = Some(e),
            },
            Msg::Marked => {}
            Msg::JumpUnread => {
                self.banner_off = true;
                if let Some(i) = self.first_new(&core.user) {
                    self.at_bottom = false;
                    let y = i as f32 / self.messages.len().max(1) as f32;
                    return iced::widget::operation::snap_to(
                        list_id(),
                        iced::widget::scrollable::RelativeOffset {
                            x: None,
                            y: Some(y),
                        },
                    );
                }
            }
            Msg::Older(g, msgs) if g == self.generation => {
                self.loading = false;
                self.has_older = msgs.len() >= PAGE;
                let mut all = msgs;
                all.append(&mut self.messages);
                self.messages = all;
                self.messages.truncate(MAX_KEPT);
                self.probe_files(core);
            }
            Msg::LoadOlder => {
                if self.loading || !self.has_older {
                    return Task::none();
                }
                if let Some(first) = self.messages.first() {
                    self.loading = true;
                    let before = first.cursor.clone();
                    return self.load(core, Some(before), PAGE, true);
                }
            }
            Msg::Link(url) => {
                // Nur normalisierte http(s)-Links ohne Userinfo (N-6).
                if let Some(u) = app_proto::urls::link(&url) {
                    crate::platform::open_url(&u);
                }
            }
            Msg::Jump => {
                self.banner_off = true;
                self.at_bottom = true;
                return Task::batch([
                    iced::widget::operation::snap_to_end(list_id()),
                    self.mark_read(core),
                ]);
            }
            Msg::Scrolled(v) => {
                let fits = v.content_bounds().height <= v.bounds().height + 1.0;
                self.at_bottom = fits || v.relative_offset().y >= 0.98;
                return self.mark_read(core);
            }
            Msg::Edit(a) => {
                self.error = None;
                self.input.perform(a);
            }
            Msg::Send => {
                self.banner_off = true;
                self.at_bottom = true;
                return self.send(core);
            }
            Msg::Sent(r) => {
                if let Err(e) = r {
                    self.error = Some(app_core::tr!(
                        "Sending failed: {e}",
                        "Senden fehlgeschlagen: {e}"
                    ));
                }
                return self.refresh(core);
            }
            Msg::EditLast => {
                if let Some(id) = self.last_own(&core.user).map(|m| m.id) {
                    self.start_edit(id);
                }
            }
            Msg::CancelCompose => {
                if self.editing.is_some() {
                    self.input = Content::new();
                }
                self.reset_compose();
            }
            Msg::Menu(id) => {
                self.banner_off = true;
                self.menu = if self.menu == id { None } else { id };
            }
            Msg::Hover(id) => self.hovered = Some(id),
            Msg::Unhover(id) => {
                if self.hovered == Some(id) {
                    self.hovered = None;
                }
            }
            Msg::Reply(id, preview) => {
                self.editing = None;
                self.reply = Some((id, preview));
                self.menu = None;
            }
            Msg::StartEdit(id) => self.start_edit(id),
            Msg::PickMention(name) => {
                let t = complete_mention(self.input.text().trim_end_matches('\n'), &name);
                self.input = content_with(&t);
            }
            Msg::ToggleEmoji => self.emoji_open = !self.emoji_open,
            Msg::Emoji(e) => {
                self.input
                    .perform(Action::Edit(Edit::Paste(Arc::new(e.to_owned()))));
                self.emoji_open = false;
            }
            Msg::Latest(..) | Msg::Older(..) => {}
        }
        Task::none()
    }

    /// Chat eines Channels. `dm_peer` ist das Gegenueber einer Direktnachricht.
    #[allow(clippy::too_many_arguments)]
    pub fn view<'a>(
        &'a self,
        me: &PublicKey,
        my_name: &str,
        name_of: &dyn Fn(&PublicKey) -> String,
        members: &[(PublicKey, String)],
        files: &FileService,
        dm_peer: Option<(PublicKey, String)>,
    ) -> Element<'a, Msg> {
        if let Some(h) = &self.viewer {
            return image_viewer(h);
        }
        let mut col = column![].padding([space::S, space::S]);
        if self.has_older {
            col = col.push(
                container(
                    ui::button_small(if self.loading {
                        tr("Loading …", "Lade …")
                    } else {
                        tr("Load older messages", "Ältere Nachrichten laden")
                    })
                    .style(btn::secondary)
                    .on_press_maybe((!self.loading).then_some(Msg::LoadOlder)),
                )
                .center_x(Length::Fill)
                .padding(space::S),
            );
        } else {
            col = col.push(self.welcome(dm_peer.as_ref()));
        }
        let newest_own = self.last_own(me).map(|m| m.id);
        let first_new = self.first_new(me);
        let mut prev: Option<&ChatMessage> = None;
        for (idx, m) in self.messages.iter().enumerate() {
            let new_day = prev.is_none_or(|p| day(p.cursor.wall_ms) != day(m.cursor.wall_ms));
            if new_day {
                col = col.push(day_divider(date_label(m.cursor.wall_ms)));
            }
            if first_new == Some(idx) {
                col = col.push(new_divider());
            }
            let grouped = !new_day
                && first_new != Some(idx)
                && m.reply.is_none()
                && prev.is_some_and(|p| {
                    p.author == m.author && m.cursor.wall_ms - p.cursor.wall_ms <= GROUP_MS
                });
            let hovered = self.hovered == Some(m.id);
            let author = name_of(&m.author);

            let mut content = column![].spacing(space::XS);
            if !grouped {
                content = content.push(
                    row![
                        text(author.clone())
                            .size(size::BODY)
                            .font(ui::SEMIBOLD)
                            .style(tx::normal),
                        ui::caption(time_label(m.cursor.wall_ms)),
                    ]
                    .spacing(space::S)
                    .align_y(Alignment::Center),
                );
            }
            if !m.body.is_empty() || m.attachments.is_empty() {
                content = content.push(body_view(m));
            }
            for a in &m.attachments {
                content = content.push(attachment_view(
                    a,
                    files.state(&a.hash),
                    a.thumb.and_then(|t| files.thumb(&t)),
                    files.is_pinned(&a.hash),
                    files.availability(&a.hash),
                    self.confirm_open.as_ref(),
                ));
            }
            if !m.reactions.is_empty() {
                let mut r = row![].spacing(space::XS);
                for x in &m.reactions {
                    let mine = x.mine;
                    r = r.push(
                        container(text(format!("{} {}", x.emoji, x.count)).size(size::CAPTION))
                            .padding([2, 8])
                            .style(move |t: &Theme| {
                                let k = ui::tokens(t);
                                container::Style {
                                    text_color: Some(if mine { k.accent_fg } else { k.text }),
                                    background: Some(Background::Color(if mine {
                                        k.accent_soft
                                    } else {
                                        k.raised
                                    })),
                                    border: Border::default()
                                        .rounded(radius::FULL)
                                        .width(1)
                                        .color(if mine { k.accent } else { Color::TRANSPARENT }),
                                    ..container::Style::default()
                                }
                            }),
                    );
                }
                content = content.push(r.wrap());
            }
            if let Some(d) = m.delivery.filter(|_| !m.deleted) {
                let newest = newest_own == Some(m.id);
                if let Some((label, tip)) = delivery_text(d, newest) {
                    content =
                        content.push(ui::tip(ui::caption(label), tip, tooltip::Position::Bottom));
                }
            }

            let gutter: Element<'a, Msg> = if grouped {
                let t: Element<'a, Msg> = if hovered {
                    text(time_label(m.cursor.wall_ms))
                        .size(10)
                        .style(tx::faint)
                        .into()
                } else {
                    Space::new().into()
                };
                container(t)
                    .width(AVATAR)
                    .padding(iced::Padding::ZERO.top(3))
                    .center_x(AVATAR)
                    .into()
            } else {
                ui::avatar(&author, &m.author, AVATAR)
            };
            let mut block = column![].spacing(space::XS);
            if let Some(r) = &m.reply {
                let who = name_of(&r.author);
                block = block.push(
                    row![
                        text("↪").size(size::CAPTION).style(tx::faint),
                        ui::avatar(&who, &r.author, 16.0),
                        text(who)
                            .size(size::CAPTION)
                            .font(ui::SEMIBOLD)
                            .style(tx::muted),
                        text(if r.deleted {
                            tr("(deleted)", "(gelöscht)").to_owned()
                        } else {
                            r.preview.clone()
                        })
                        .size(size::CAPTION)
                        .style(tx::faint)
                        .wrapping(text::Wrapping::None),
                    ]
                    .spacing(6)
                    .align_y(Alignment::Center)
                    .padding(iced::Padding::ZERO.left(AVATAR + space::M - 2.0)),
                );
            }
            block = block.push(
                row![gutter, content.width(Length::Fill)]
                    .spacing(space::M)
                    .align_y(Alignment::Start),
            );
            let mention = m.author != *me && !m.deleted && unread::mentions(&m.body, my_name);
            let active = hovered || self.menu == Some(m.id);
            let shell = container(block)
                .width(Length::Fill)
                .padding([if grouped { 3.0 } else { space::S - 2.0 }, space::M])
                .style(move |t: &Theme| {
                    if mention {
                        ui::bg::mention(t)
                    } else if active {
                        ui::bg::hover(t)
                    } else {
                        container::Style::default()
                    }
                });
            let item: Element<'a, Msg> = if active && !m.deleted {
                let preview: String = m.body.chars().take(60).collect();
                let mut actions = row![msg_action(
                    tr("Reply", "Antworten"),
                    Msg::Reply(m.id, preview)
                )]
                .spacing(2);
                if m.author == *me {
                    actions =
                        actions.push(msg_action(tr("Edit", "Bearbeiten"), Msg::StartEdit(m.id)));
                }
                stack![
                    shell,
                    container(container(actions).padding(2).style(ui::bg::popover))
                        .align_right(Length::Fill)
                        .padding([0.0, space::M]),
                ]
                .into()
            } else {
                shell.into()
            };
            col = col.push(
                mouse_area(item)
                    .on_press(Msg::Menu(Some(m.id)))
                    .on_enter(Msg::Hover(m.id))
                    .on_exit(Msg::Unhover(m.id)),
            );
            prev = Some(m);
        }
        let list = ui::scroll(col.push(Space::new().height(space::S)))
            .id(list_id())
            .on_scroll(Msg::Scrolled)
            .width(Length::Fill)
            .height(Length::Fill);

        let mut layers = stack![list].width(Length::Fill).height(Length::Fill);
        if first_new.is_some() && !self.banner_off {
            layers = layers.push(
                container(
                    button(
                        row![
                            text(tr(
                                "New messages since your last visit",
                                "Neue Nachrichten seit deinem letzten Besuch"
                            ))
                            .size(size::SMALL)
                            .width(Length::Fill),
                            text(tr("To first unread ↑", "Zur ersten ungelesenen ↑"))
                                .size(size::SMALL)
                                .font(ui::SEMIBOLD),
                        ]
                        .spacing(space::S),
                    )
                    .width(Length::Fill)
                    .padding([6, 12])
                    .style(btn::primary)
                    .on_press(Msg::JumpUnread),
                )
                .padding([space::S, space::L]),
            );
        }
        if !self.at_bottom {
            layers = layers.push(
                container(
                    button(
                        text(tr("↓  Latest messages", "↓  Neueste Nachrichten"))
                            .size(size::SMALL)
                            .font(ui::SEMIBOLD),
                    )
                    .padding([7, 14])
                    .style(btn::floating)
                    .on_press(Msg::Jump),
                )
                .align_right(Length::Fill)
                .align_bottom(Length::Fill)
                .padding(space::L),
            );
        }
        let placeholder = match &dm_peer {
            Some((_, n)) => app_core::tr!("Message {n}", "Nachricht an {n}"),
            None => app_core::tr!("Message #{}", "Nachricht an #{}", self.name),
        };
        column![layers, self.composer(me, members, files, placeholder)].into()
    }

    /// Kopf am Anfang des Verlaufs.
    fn welcome<'a>(&self, dm_peer: Option<&(PublicKey, String)>) -> Element<'a, Msg> {
        let (mark, title, desc): (Element<'a, Msg>, String, String) = match dm_peer {
            Some((u, n)) => (
                ui::avatar(n, u, 64.0),
                n.clone(),
                app_core::tr!(
                    "This is the start of your direct message with {n}. Only the two of you can see this history.",
                    "Das ist der Anfang deiner Direktnachricht mit {n}. Nur ihr zwei seht diesen Verlauf."
                ),
            ),
            None => (
                container(text("#").size(32).font(ui::BOLD).line_height(1.0))
                    .center_x(Length::Fixed(64.0))
                    .center_y(Length::Fixed(64.0))
                    .style(|t: &Theme| container::Style {
                        background: Some(Background::Color(ui::tokens(t).raised)),
                        border: Border::default().rounded(radius::FULL),
                        ..container::Style::default()
                    })
                    .into(),
                app_core::tr!("Welcome to #{}", "Willkommen in #{}", self.name),
                tr("This is the start of the channel.", "Das ist der Anfang des Channels.").to_owned(),
            ),
        };
        container(
            column![
                mark,
                text(title).size(size::H1).font(ui::BOLD).style(tx::normal),
                ui::muted(desc),
            ]
            .spacing(space::S),
        )
        .padding([space::XL, space::M])
        .into()
    }

    fn composer<'a>(
        &'a self,
        me: &PublicKey,
        members: &[(PublicKey, String)],
        files: &FileService,
        placeholder: String,
    ) -> Element<'a, Msg> {
        let mut col = column![].spacing(space::S);
        if let Some(n) = &self.file_note {
            col = col.push(ui::caption(n.as_str()));
        }
        if let Some(s) = files.sharing() {
            col = col.push(ui::caption(app_core::tr!("Sharing … {s}", "Teile … {s}")));
        }
        if let Some(e) = files.share_error() {
            col = col.push(text(e).size(size::CAPTION).style(tx::danger));
        }
        // Erwaehnungs-Vorschlaege (gehen davon aus, dass der Cursor am Ende steht)
        let full = self.input.text();
        if let Some(prefix) = mention_prefix(full.trim_end_matches('\n')) {
            let p = prefix.to_lowercase();
            let mut sug = column![ui::overline(tr("Members", "Mitglieder"))].spacing(2);
            let mut any = false;
            for (u, n) in members
                .iter()
                .filter(|(u, n)| u != me && n.to_lowercase().starts_with(&p))
                .take(5)
            {
                any = true;
                sug = sug.push(
                    button(
                        row![
                            ui::avatar(n, u, 22.0),
                            text(n.clone()).size(size::SMALL).font(ui::SEMIBOLD),
                        ]
                        .spacing(space::S)
                        .align_y(Alignment::Center),
                    )
                    .width(Length::Fill)
                    .padding([4, 8])
                    .style(btn::nav(false))
                    .on_press(Msg::PickMention(n.clone())),
                );
            }
            if any {
                col = col.push(container(sug).padding(6).style(ui::bg::popover));
            }
        }
        if self.emoji_open {
            let mut r = row![].spacing(2);
            for e in EMOJI {
                r = r.push(
                    button(text(*e).size(18))
                        .padding(4)
                        .style(btn::ghost)
                        .on_press(Msg::Emoji(e)),
                );
            }
            col = col.push(container(r.wrap()).padding(6).style(ui::bg::popover));
        }

        let mut boxed = column![].spacing(space::S);
        if let Some((_, p)) = &self.reply {
            boxed = boxed.push(compose_bar(
                row![
                    text(tr("Replying to", "Antwort auf"))
                        .size(size::CAPTION)
                        .style(tx::faint),
                    text(p.clone())
                        .size(size::CAPTION)
                        .font(ui::SEMIBOLD)
                        .style(tx::muted)
                        .wrapping(text::Wrapping::None),
                ]
                .spacing(6)
                .into(),
            ));
        }
        if self.editing.is_some() {
            boxed = boxed.push(compose_bar(
                row![
                    text(tr("Edit message", "Nachricht bearbeiten"))
                        .size(size::CAPTION)
                        .font(ui::SEMIBOLD)
                        .style(tx::accent),
                    ui::caption(tr(
                        "Enter saves · Esc cancels",
                        "Enter speichert · Esc bricht ab"
                    )),
                ]
                .spacing(space::S)
                .into(),
            ));
        }
        if !self.pending.is_empty() {
            let mut chips = row![].spacing(6);
            for (i, p) in self.pending.iter().enumerate() {
                let name = app_core::files::sanitize_file_name(&p.to_string_lossy());
                let size = std::fs::metadata(p)
                    .map(|m| format_size(m.len()))
                    .unwrap_or_default();
                chips = chips.push(
                    container(
                        row![
                            text(name).size(size::CAPTION).font(ui::SEMIBOLD),
                            ui::caption(size),
                            ui::tip(
                                button(text("✕").size(size::CAPTION))
                                    .padding([0, 4])
                                    .style(btn::ghost)
                                    .on_press(Msg::RemovePending(i)),
                                tr("Don't include", "Nicht mitsenden"),
                                tooltip::Position::Top,
                            ),
                        ]
                        .spacing(6)
                        .align_y(Alignment::Center),
                    )
                    .padding([3, 8])
                    .style(ui::bg::well),
                );
            }
            boxed = boxed.push(chips.wrap());
        }
        let empty = full.trim().is_empty() && self.pending.is_empty();
        let editor = text_editor(&self.input)
            .placeholder(placeholder)
            .on_action(Msg::Edit)
            .size(size::BODY)
            .padding([8, 4])
            .max_height(240)
            .style(ui::editor_plain)
            .key_binding(move |kp| {
                let focused = matches!(kp.status, Status::Focused { .. });
                match (&kp.key, focused) {
                    (Key::Named(Named::Enter), true) if !kp.modifiers.shift() => {
                        Some(Binding::Custom(Msg::Send))
                    }
                    (Key::Named(Named::ArrowUp), true) if empty => {
                        Some(Binding::Custom(Msg::EditLast))
                    }
                    (Key::Named(Named::Escape), true) => Some(Binding::Custom(Msg::CancelCompose)),
                    (Key::Character(c), true) if kp.modifiers.command() && c.as_str() == "v" => {
                        Some(Binding::Custom(Msg::Paste))
                    }
                    _ => Binding::from_key_press(kp),
                }
            });
        boxed = boxed.push(
            row![
                ui::icon_button(
                    "+",
                    tr("Attach files", "Dateien anhängen"),
                    Some(Msg::PickFiles)
                ),
                editor,
                ui::icon_button(
                    "😀",
                    tr("Insert emoji", "Emoji einfügen"),
                    Some(Msg::ToggleEmoji)
                ),
                ui::tip(
                    ui::button_text(if self.editing.is_some() {
                        tr("Save", "Speichern")
                    } else {
                        tr("Send", "Senden")
                    })
                    .style(btn::primary)
                    .on_press_maybe((!empty).then_some(Msg::Send)),
                    tr(
                        "Enter sends, Shift+Enter adds a new line",
                        "Enter sendet, Umschalt+Enter macht eine neue Zeile"
                    ),
                    tooltip::Position::Top,
                ),
            ]
            .spacing(space::XS)
            .align_y(Alignment::End),
        );
        col = col.push(
            container(boxed)
                .padding([6, 6])
                .width(Length::Fill)
                .style(ui::bg::composer),
        );
        if let Some(e) = &self.error {
            col = col.push(text(e.as_str()).size(size::CAPTION).style(tx::danger));
        }
        container(col)
            .padding(
                iced::Padding::ZERO
                    .left(space::L)
                    .right(space::L)
                    .bottom(space::L),
            )
            .into()
    }
}

/// Avatar-Breite im Verlauf.
const AVATAR: f32 = 36.0;

/// Kleine Aktion an einer Nachricht (beim Ueberfahren).
fn msg_action<'a>(label: &'a str, m: Msg) -> Element<'a, Msg> {
    button(text(label).size(size::CAPTION).font(ui::SEMIBOLD))
        .padding([3, 8])
        .style(btn::ghost)
        .on_press(m)
        .into()
}

/// Leiste ueber dem Eingabefeld (Antwort, Bearbeiten) mit Abbrechen.
fn compose_bar(content: Element<'_, Msg>) -> Element<'_, Msg> {
    container(
        row![
            container(content).width(Length::Fill),
            ui::tip(
                button(text("✕").size(size::CAPTION))
                    .padding([2, 6])
                    .style(btn::ghost)
                    .on_press(Msg::CancelCompose),
                tr("Cancel (Esc)", "Abbrechen (Esc)"),
                tooltip::Position::Top,
            ),
        ]
        .align_y(Alignment::Center),
    )
    .padding(iced::Padding::ZERO.left(6))
    .into()
}

/// Trennlinie mit Datum.
fn day_divider<'a>(label: String) -> Element<'a, Msg> {
    row![
        ui::divider(),
        text(label)
            .size(size::CAPTION)
            .font(ui::SEMIBOLD)
            .style(tx::faint),
        ui::divider(),
    ]
    .spacing(space::M)
    .align_y(Alignment::Center)
    .padding([space::M, space::M])
    .into()
}

/// Rote Linie vor der ersten ungelesenen Nachricht.
fn new_divider<'a>() -> Element<'a, Msg> {
    let line = || {
        container(Space::new())
            .width(Length::Fill)
            .height(1)
            .style(|t: &Theme| container::Style {
                background: Some(Background::Color(ui::tokens(t).danger)),
                ..container::Style::default()
            })
    };
    row![line(), ui::tag(tr("NEW", "NEU"), ui::bg::badge_live)]
        .spacing(space::S)
        .align_y(Alignment::Center)
        .padding([space::XS, space::M])
        .into()
}

/// Bild in voller Groesse.
fn image_viewer(h: &iced::widget::image::Handle) -> Element<'_, Msg> {
    container(
        column![
            row![
                Space::new().width(Length::Fill),
                ui::button_text(tr("Close", "Schließen"))
                    .style(btn::secondary)
                    .on_press(Msg::CloseImage),
            ],
            container(
                iced::widget::image(h.clone())
                    .width(Length::Fill)
                    .height(Length::Fill)
            )
            .center(Length::Fill),
        ]
        .spacing(space::M),
    )
    .padding(space::L)
    .width(Length::Fill)
    .height(Length::Fill)
    .style(|_: &Theme| container::Style {
        background: Some(Background::Color(ui::current().media_bg)),
        ..container::Style::default()
    })
    .into()
}

/// Text und Erklaerung zum Zustellstatus; bei vollstaendiger Zustellung nur fuer die neueste.
fn delivery_text(d: Delivery, newest: bool) -> Option<(String, &'static str)> {
    if d.acked == 0 {
        Some((
            tr("⏳ waiting", "⏳ wartet").into(),
            tr("No other member has the message yet. It is safe with you and will be delivered automatically once someone is online.", "Noch hat kein anderes Mitglied die Nachricht. Sie liegt sicher bei dir und wird automatisch zugestellt, sobald jemand online ist."),
        ))
    } else if d.acked < d.total {
        Some((
            app_core::tr!("delivered to {} of {}", "zugestellt an {} von {}", d.acked, d.total),
            tr("The remaining members are offline right now. They will get the message the next time they connect.", "Die übrigen Mitglieder sind gerade offline. Sie bekommen die Nachricht beim nächsten Verbinden."),
        ))
    } else if newest {
        Some((
            tr("✓ delivered", "✓ zugestellt").into(),
            tr(
                "All other members have the message.",
                "Alle anderen Mitglieder haben die Nachricht.",
            ),
        ))
    } else {
        None
    }
}

fn body_view(m: &ChatMessage) -> Element<'_, Msg> {
    let k = ui::current();
    if m.deleted {
        return text(tr("Message deleted", "Nachricht gelöscht"))
            .size(size::SMALL)
            .font(Font {
                style: Style::Italic,
                ..Font::DEFAULT
            })
            .style(tx::faint)
            .into();
    }
    let mut spans: Vec<iced::widget::text::Span<'_, String>> = Vec::new();
    for s in parse(&m.body) {
        spans.push(match s {
            Span::Text(t) => span(t),
            Span::Bold(t) => span(t).font(ui::BOLD),
            Span::Italic(t) => span(t).font(Font {
                style: Style::Italic,
                ..Font::DEFAULT
            }),
            Span::Code(t) => span(t)
                .font(Font::MONOSPACE)
                .padding([1, 4])
                .background(k.raised)
                .border(Border::default().rounded(radius::S)),
            // Angezeigt wird die normalisierte Adresse (Punycode-Host statt Doppelgaenger-
            // Buchstaben); Links mit Userinfo ("bank.example@evil.example") bleiben Text.
            Span::Link(u) => match app_proto::urls::link(&u) {
                Some(n) if app_proto::urls::is_idn(&n) => span(format!("⚠ {n}"))
                    .color(k.danger_fg)
                    .underline(true)
                    .link(n),
                Some(n) => span(n.clone()).color(k.accent_fg).underline(true).link(n),
                None => span(u),
            },
        });
    }
    if m.edited {
        spans.push(
            span(tr(" (edited)", " (bearbeitet)"))
                .size(size::OVERLINE)
                .color(k.text_faint),
        );
    }
    rich_text(spans)
        .on_link_click(Msg::Link)
        .size(size::BODY)
        .line_height(1.4)
        .style(tx::normal)
        .into()
}

/// Ordner "Downloads" des Nutzers (sonst der Temp-Ordner).
fn downloads_dir() -> std::path::PathBuf {
    std::env::var_os("USERPROFILE")
        .map(|p| std::path::PathBuf::from(p).join("Downloads"))
        .filter(|p| p.is_dir())
        .unwrap_or_else(std::env::temp_dir)
}

/// "Verfügbar bei 2 Personen" bzw. "gerade bei niemandem online".
fn avail_label(avail: Option<usize>) -> String {
    match avail {
        None => tr("checking who has it …", "prüfe, wer sie hat …").to_owned(),
        Some(0) => tr(
            "not available from anyone online right now",
            "gerade bei niemandem online verfügbar",
        )
        .to_owned(),
        Some(1) => tr("available from 1 person", "verfügbar bei 1 Person").to_owned(),
        Some(n) => app_core::tr!("available from {n} people", "verfügbar bei {n} Personen"),
    }
}

/// Eine angehaengte Datei: Name, Groesse und je nach Zustand die passenden Knoepfe.
fn attachment_view<'a>(
    a: &Attachment,
    state: FileState,
    thumb: Option<iced::widget::image::Handle>,
    pinned: bool,
    avail: Option<usize>,
    confirm: Option<&(app_proto::Hash, String)>,
) -> Element<'a, Msg> {
    let small =
        |label: &'static str, m: Msg| ui::button_small(label).style(btn::secondary).on_press(m);
    let size_label = format_size(a.size);
    let mut status = column![].spacing(space::XS);
    let mut actions = row![].spacing(6);
    match &state {
        FileState::Local => {
            status = status.push(ui::caption(size_label));
            actions = actions
                .push(
                    ui::button_small(tr("Open", "Öffnen"))
                        .style(btn::primary)
                        .on_press(Msg::OpenFile(a.hash, a.name.clone())),
                )
                .push(small(
                    tr("Save", "Speichern"),
                    Msg::SaveFile(a.hash, a.name.clone()),
                ))
                .push(small(
                    tr("Save as …", "Speichern unter …"),
                    Msg::SaveAs(a.hash, a.name.clone()),
                ))
                .push(ui::tip(
                    ui::button_small(if pinned {
                        tr("📌 Pinned", "📌 Angepinnt")
                    } else {
                        tr("Pin", "Anpinnen")
                    })
                    .style(btn::chip(pinned))
                    .on_press(Msg::TogglePin(a.hash)),
                    tr(
                        "Pinned files are not removed by automatic cleanup",
                        "Angepinnte Dateien löscht die automatische Bereinigung nicht",
                    ),
                    tooltip::Position::Top,
                ));
        }
        FileState::Remote => {
            status = status.push(ui::caption(format!(
                "{size_label} · {}",
                avail_label(avail)
            )));
            actions = actions.push(
                ui::button_small(tr("Download", "Herunterladen"))
                    .style(btn::primary)
                    .on_press(Msg::Download(a.hash, a.size)),
            );
        }
        FileState::Downloading { done, total } => {
            let pct = if *total == 0 { 0 } else { done * 100 / total };
            status = status
                .push(ui::caption(app_core::tr!(
                    "Loading … {pct} % ({} of {})",
                    "Lade … {pct} % ({} von {})",
                    format_size(*done),
                    format_size(*total)
                )))
                .push(
                    iced::widget::progress_bar(0.0..=100.0, pct as f32)
                        .girth(4)
                        .length(Length::Fill),
                );
        }
        FileState::Failed(e) => {
            status = status
                .push(text(e.clone()).size(size::CAPTION).style(tx::danger))
                .push(ui::caption(avail_label(avail)));
            actions = actions.push(small(tr("Retry", "Nochmal"), Msg::Download(a.hash, a.size)));
        }
    }
    let mut col = column![].spacing(space::S);
    if let (None, Some(t)) = (&thumb, a.thumb) {
        // Vorschau gibt es, sie wird aber nicht automatisch geladen.
        actions = actions.push(small(
            tr("Show preview", "Vorschau laden"),
            Msg::LoadPreview(t),
        ));
    }
    if let Some(handle) = thumb {
        let is_image = a.mime.starts_with("image/");
        let press = match &state {
            FileState::Local if is_image => Some(Msg::ShowImage(a.hash, a.name.clone())),
            FileState::Local => Some(Msg::OpenFile(a.hash, a.name.clone())),
            FileState::Remote | FileState::Failed(_) => Some(Msg::Download(a.hash, a.size)),
            FileState::Downloading { .. } => None,
        };
        let pic = iced::widget::image(handle)
            .width(Length::Fixed(320.0))
            .border_radius(radius::M);
        let shown: Element<'a, Msg> = if is_image {
            pic.into()
        } else {
            stack![
                pic,
                container(
                    container(
                        text("▶")
                            .size(20)
                            .color(ui::current().media_fg)
                            .line_height(1.0)
                    )
                    .center_x(Length::Fixed(48.0))
                    .center_y(Length::Fixed(48.0))
                    .style(|_: &Theme| container::Style {
                        background: Some(Background::Color(ui::current().media_scrim)),
                        border: Border::default().rounded(radius::FULL),
                        ..container::Style::default()
                    })
                )
                .center(Length::Fill),
            ]
            .into()
        };
        let tip = if is_image {
            tr("View large", "Groß anzeigen")
        } else {
            tr("Open in default player", "Im Standard-Player öffnen")
        };
        col = col.push(ui::tip(
            button(shown)
                .style(btn::bare)
                .padding(0)
                .on_press_maybe(press),
            tip,
            tooltip::Position::Top,
        ));
    }
    col = col.push(
        row![
            container(text("📄").size(18).line_height(1.0))
                .center_x(Length::Fixed(36.0))
                .center_y(Length::Fixed(36.0))
                .style(ui::bg::well),
            column![
                text(a.name.clone())
                    .size(size::SMALL)
                    .font(ui::SEMIBOLD)
                    .style(tx::normal),
                status,
            ]
            .spacing(2)
            .width(Length::Fill),
        ]
        .spacing(space::M)
        .align_y(Alignment::Center),
    );
    col = col.push(actions.wrap());
    if let Some((h, name)) = confirm {
        if *h == a.hash {
            col = col.push(ui::banner(
                Tone::Danger,
                column![
                    text(app_core::tr!(
                        "“{name}” may be a program and could cause harm when opened. Only open it if you trust the sender.",
                        "„{name}“ kann ein Programm sein und beim Öffnen Schaden anrichten. Nur öffnen, wenn du dem Absender traust."
                    ))
                    .size(size::SMALL),
                    row![
                        ui::button_small(tr("Open anyway", "Trotzdem öffnen"))
                            .style(btn::danger)
                            .on_press(Msg::ConfirmOpen),
                        ui::button_small(tr("Cancel", "Abbrechen"))
                            .style(btn::secondary)
                            .on_press(Msg::CancelOpen),
                    ]
                    .spacing(space::S),
                ]
                .spacing(space::S),
            ));
        }
    }
    container(col)
        .padding(space::M)
        .max_width(440)
        .style(ui::bg::card)
        .into()
}

// ---------- Zeit (lokale Zeit ohne Zusatzbibliothek) ----------

fn local_secs(ms: i64) -> i64 {
    ms.div_euclid(1000) + crate::platform::utc_offset_minutes() as i64 * 60
}

fn day(ms: i64) -> i64 {
    local_secs(ms).div_euclid(86_400)
}

/// Datum aus Tagen seit 1970-01-01 (proleptisch gregorianisch).
fn civil(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn date_label(ms: i64) -> String {
    let (y, m, d) = civil(day(ms));
    format!("{d}.{m}.{y}")
}

fn time_label(ms: i64) -> String {
    let s = local_secs(ms).rem_euclid(86_400);
    format!("{:02}:{:02}", s / 3600, s % 3600 / 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delivery_text_states() {
        let d = |acked, total| Delivery { acked, total };
        assert!(delivery_text(d(0, 2), false).unwrap().0.contains("waiting"));
        assert_eq!(
            delivery_text(d(1, 2), false).unwrap().0,
            "delivered to 1 of 2"
        );
        assert!(delivery_text(d(2, 2), false).is_none());
        assert!(delivery_text(d(2, 2), true).is_some());
    }

    #[test]
    fn civil_dates() {
        assert_eq!(civil(0), (1970, 1, 1));
        assert_eq!(civil(19_723), (2024, 1, 1));
        assert_eq!(civil(11_016), (2000, 2, 29));
        assert_eq!(civil(-1), (1969, 12, 31));
    }

    #[test]
    fn mentions_complete_only_the_last_started_word() {
        assert_eq!(mention_prefix("hallo @to"), Some("to"));
        assert_eq!(mention_prefix("hallo @"), Some(""));
        assert_eq!(mention_prefix("hallo @to "), None);
        assert_eq!(mention_prefix("mail a@b"), None);
        assert_eq!(complete_mention("hi @to", "Tobi"), "hi @Tobi ");
        assert_eq!(complete_mention("@x", "Max"), "@Max ");
        assert_eq!(complete_mention("kein", "Max"), "kein");
    }
}
