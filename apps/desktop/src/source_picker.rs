//! Auswahl der Bildquelle fuer einen Stream (M3-01): Fenster und Monitore mit Vorschaubild.

use crate::ui::{self, btn, size, space};
use app_core::i18n::tr;
use app_video::capture::{list_sources, thumbnail, Source, SourceId};
use iced::widget::{button, column, container, image, row, text, Space};
use iced::{Element, Length, Task};
use std::collections::HashMap;
use std::time::Duration;

/// So viele Vorschaubilder werden hoechstens aufgenommen.
const MAX_THUMBS: usize = 24;

#[derive(Debug, Clone)]
pub enum Msg {
    Loaded(Vec<Source>),
    Thumb(SourceId, Option<(u32, u32, Vec<u8>)>),
    Select(SourceId),
    Cursor(bool),
    Refresh,
    Confirm,
    Close,
}

/// Was der Nutzer ausgewaehlt hat.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Choice {
    pub source: Source,
    pub cursor: bool,
}

pub enum Outcome {
    None,
    Chosen(Choice),
    Closed,
}

pub struct State {
    sources: Vec<Source>,
    thumbs: HashMap<SourceId, image::Handle>,
    failed: Vec<SourceId>,
    selected: Option<SourceId>,
    cursor: bool,
    loading: bool,
}

impl State {
    pub fn open(current: Option<&Choice>) -> (Self, Task<Msg>) {
        let s = Self {
            sources: Vec::new(),
            thumbs: HashMap::new(),
            failed: Vec::new(),
            selected: current.map(|c| c.source.id),
            cursor: current.is_none_or(|c| c.cursor),
            loading: true,
        };
        (s, Self::load())
    }

    fn load() -> Task<Msg> {
        Task::perform(
            async {
                tokio::task::spawn_blocking(|| list_sources(Some(std::process::id())))
                    .await
                    .unwrap_or_default()
            },
            Msg::Loaded,
        )
    }

    pub fn update(&mut self, msg: Msg) -> (Task<Msg>, Outcome) {
        match msg {
            Msg::Loaded(list) => {
                self.loading = false;
                let todo: Vec<SourceId> = list.iter().take(MAX_THUMBS).map(|s| s.id).collect();
                self.sources = list;
                let tasks = todo.into_iter().map(|id| {
                    Task::perform(
                        async move {
                            tokio::task::spawn_blocking(move || {
                                thumbnail(id, 240, Duration::from_millis(1500))
                                    .map(|t| (t.width, t.height, t.rgba))
                            })
                            .await
                            .unwrap_or(None)
                        },
                        move |t| Msg::Thumb(id, t),
                    )
                });
                return (Task::batch(tasks), Outcome::None);
            }
            Msg::Thumb(id, Some((w, h, rgba))) => {
                self.thumbs.insert(id, image::Handle::from_rgba(w, h, rgba));
            }
            Msg::Thumb(id, None) => self.failed.push(id),
            Msg::Select(id) => self.selected = Some(id),
            Msg::Cursor(c) => self.cursor = c,
            Msg::Refresh => {
                self.loading = true;
                self.thumbs.clear();
                self.failed.clear();
                return (Self::load(), Outcome::None);
            }
            Msg::Confirm => {
                if let Some(s) = self
                    .sources
                    .iter()
                    .find(|s| Some(s.id) == self.selected)
                    .cloned()
                {
                    return (
                        Task::none(),
                        Outcome::Chosen(Choice {
                            source: s,
                            cursor: self.cursor,
                        }),
                    );
                }
            }
            Msg::Close => return (Task::none(), Outcome::Closed),
        }
        (Task::none(), Outcome::None)
    }

    pub fn view(&self) -> Element<'_, Msg> {
        const W: f32 = 220.0;
        const H: f32 = 124.0;
        let mut grid = row![].spacing(space::M);
        for s in &self.sources {
            let selected = self.selected == Some(s.id);
            let preview: Element<'_, Msg> = match self.thumbs.get(&s.id) {
                Some(h) => container(
                    image(h.clone())
                        .width(Length::Fill)
                        .height(Length::Fill)
                        .border_radius(ui::radius::M),
                )
                .width(W)
                .height(H)
                .style(ui::bg::video)
                .into(),
                None => container(ui::caption(if self.failed.contains(&s.id) {
                    tr("no preview", "keine Vorschau")
                } else {
                    tr("loading …", "lädt …")
                }))
                .center_x(Length::Fixed(W))
                .center_y(Length::Fixed(H))
                .style(ui::bg::well)
                .into(),
            };
            grid = grid.push(
                button(
                    column![
                        preview,
                        text(s.title.clone())
                            .size(size::SMALL)
                            .font(ui::SEMIBOLD)
                            .wrapping(text::Wrapping::None),
                        ui::caption(s.detail.clone()).wrapping(text::Wrapping::None),
                    ]
                    .spacing(6)
                    .width(Length::Fixed(W)),
                )
                .padding(space::S)
                .style(btn::card(selected))
                .on_press(Msg::Select(s.id)),
            );
        }
        let body: Element<'_, Msg> = if self.sources.is_empty() {
            ui::empty_state(
                "▢",
                if self.loading {
                    tr(
                        "Looking for windows and screens …",
                        "Suche Fenster und Bildschirme …",
                    )
                } else {
                    tr("No sources found", "Keine Quellen gefunden")
                },
                if self.loading {
                    tr(
                        "One moment, the previews are being captured.",
                        "Einen Moment, die Vorschaubilder werden aufgenommen.",
                    )
                } else {
                    tr("Open the window you want to share and click “Refresh”.", "Öffne das Fenster, das du teilen möchtest, und klicke auf „Aktualisieren“.")
                },
                None,
            )
        } else {
            ui::scroll(container(grid.wrap()).padding([space::L, space::XL]))
                .height(Length::Fill)
                .into()
        };
        column![
            ui::header(
                column![
                    text(tr("What do you want to share?", "Was möchtest du teilen?"))
                        .size(size::H3)
                        .font(ui::SEMIBOLD)
                        .style(ui::tx::normal),
                ],
                ui::button_small(tr("Refresh", "Aktualisieren"))
                    .style(btn::secondary)
                    .on_press(Msg::Refresh),
            ),
            body,
            ui::divider(),
            container(
                row![
                    ui::check(self.cursor, tr("Show mouse cursor", "Mauszeiger zeigen"))
                        .on_toggle(Msg::Cursor),
                    Space::new().width(Length::Fill),
                    ui::button_text(tr("Cancel", "Abbrechen"))
                        .style(btn::ghost)
                        .on_press(Msg::Close),
                    ui::button_text(tr("Share", "Teilen"))
                        .style(btn::primary)
                        .on_press_maybe(self.selected.map(|_| Msg::Confirm)),
                ]
                .spacing(space::S)
                .align_y(iced::Alignment::Center),
            )
            .padding([space::M, space::L]),
        ]
        .height(Length::Fill)
        .into()
    }
}
