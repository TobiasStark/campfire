//! M0-08: UI-Prototyp – Seitenleiste, lange Liste (10.000 Einträge), Eingabe mit Emoji/Umlauten
//! und ein transparentes, klick-durchlässiges Overlay-Fenster (kein Injection).
//!
//!   ui_proto [--naive] [--autoscroll] [--no-overlay] [--secs 20]
//!
//! --naive        alle 10.000 Zeilen als Widgets (Vergleich), sonst virtualisiert
//! --autoscroll   Liste scrollt automatisch (Messung der Flüssigkeit)

use std::{
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant},
};

use iced::{
    widget::{button, column, container, row, scrollable, text, text_input, Space},
    window, Color, Element, Length, Point, Size, Subscription, Task, Theme,
};
use windows::Win32::{
    Foundation::{FILETIME, HWND},
    System::{
        ProcessStatus::{
            GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX,
        },
        Threading::{GetCurrentProcess, GetProcessTimes},
    },
    UI::WindowsAndMessaging::{
        GetWindowLongPtrW, SetWindowLongPtrW, GWL_EXSTYLE, WS_EX_LAYERED, WS_EX_NOACTIVATE,
        WS_EX_TOOLWINDOW, WS_EX_TRANSPARENT,
    },
};

const ROW_H: f32 = 28.0;
const N: usize = 10_000;

static VIEW_US: AtomicU64 = AtomicU64::new(0);
static VIEW_N: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone)]
enum Msg {
    MainOpened(window::Id),
    OverlayOpened(window::Id),
    OverlayHandle(u64),
    Scrolled(f32, f32),
    Input(String),
    Select(usize),
    Tick,
    AutoScroll,
    Closed,
}

struct App {
    main: Option<window::Id>,
    overlay: Option<window::Id>,
    items: Vec<String>,
    input: String,
    selected: usize,
    offset: f32,
    viewport_h: f32,
    naive: bool,
    autoscroll: bool,
    speaker: usize,
    start: Instant,
    secs: u64,
    // Statistik
    last: Instant,
    cpu_last: u64,
    ticks: u64,
    ticks_last: u64,
    cpu_sum: f64,
    reports: u32,
    ram_max: f64,
    scroll_pos: f32,
}

fn list_id() -> iced::widget::Id {
    iced::widget::Id::new("list")
}

fn cpu_time_100ns() -> u64 {
    let (mut c, mut e, mut k, mut u) = (
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
    );
    // SAFETY: Eigener Prozess, gültige Ausgabeparameter.
    unsafe {
        let _ = GetProcessTimes(GetCurrentProcess(), &mut c, &mut e, &mut k, &mut u);
    }
    let t = |f: FILETIME| ((f.dwHighDateTime as u64) << 32) | f.dwLowDateTime as u64;
    t(k) + t(u)
}

fn private_mb() -> f64 {
    let mut m = PROCESS_MEMORY_COUNTERS_EX {
        cb: std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32,
        ..Default::default()
    };
    // SAFETY: PROCESS_MEMORY_COUNTERS_EX beginnt mit PROCESS_MEMORY_COUNTERS.
    unsafe {
        let _ = GetProcessMemoryInfo(
            GetCurrentProcess(),
            &mut m as *mut _ as *mut PROCESS_MEMORY_COUNTERS,
            m.cb,
        );
    }
    m.PrivateUsage as f64 / 1024.0 / 1024.0
}

fn boot(naive: bool, autoscroll: bool, secs: u64, overlay: bool) -> (App, Task<Msg>) {
    let items = (0..N)
        .map(|i| {
            format!(
                "#{i}  Anna: Hallo zusammen, Grüße aus Köln! Übermorgen zocken wir 🎮🎉 – äöüß 👍🏽 😀"
            )
        })
        .collect();
    let (main_id, open_main) = window::open(window::Settings {
        size: Size::new(1000.0, 700.0),
        ..Default::default()
    });
    let mut tasks = vec![open_main.map(Msg::MainOpened)];
    if overlay {
        let (oid, open_overlay) = window::open(window::Settings {
            size: Size::new(280.0, 170.0),
            position: window::Position::Specific(Point::new(30.0, 30.0)),
            decorations: false,
            transparent: true,
            resizable: false,
            level: window::Level::AlwaysOnTop,
            visible: true,
            ..Default::default()
        });
        let _ = oid;
        tasks.push(open_overlay.map(Msg::OverlayOpened));
    }
    let _ = main_id;
    (
        App {
            main: None,
            overlay: None,
            items,
            input: "Grüße 🎉 äöüß 👍🏽 😀 Tschüss".into(),
            selected: 0,
            offset: 0.0,
            viewport_h: 600.0,
            naive,
            autoscroll,
            speaker: 0,
            start: Instant::now(),
            secs,
            last: Instant::now(),
            cpu_last: cpu_time_100ns(),
            ticks: 0,
            ticks_last: 0,
            cpu_sum: 0.0,
            reports: 0,
            ram_max: 0.0,
            scroll_pos: 0.0,
        },
        Task::batch(tasks),
    )
}

impl App {
    fn update(&mut self, msg: Msg) -> Task<Msg> {
        match msg {
            Msg::MainOpened(id) => self.main = Some(id),
            Msg::OverlayOpened(id) => {
                self.overlay = Some(id);
                // Klick-durchlässig + ohne Fokus: HWND holen und Erweiterte Stile setzen.
                return Task::batch([
                    window::enable_mouse_passthrough::<Msg>(id),
                    window::raw_id::<Msg>(id).map(Msg::OverlayHandle),
                ]);
            }
            Msg::OverlayHandle(raw) => {
                // SAFETY: raw ist das HWND unseres eigenen Overlay-Fensters.
                unsafe {
                    let hwnd = HWND(raw as *mut _);
                    let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
                    let add =
                        (WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW).0
                            as isize;
                    SetWindowLongPtrW(hwnd, GWL_EXSTYLE, ex | add);
                }
            }
            Msg::Scrolled(y, h) => {
                self.offset = y;
                self.viewport_h = h;
            }
            Msg::Input(s) => self.input = s,
            Msg::Select(i) => self.selected = i,
            Msg::AutoScroll => {
                if self.autoscroll {
                    self.scroll_pos = (self.scroll_pos + 25.0) % (N as f32 * ROW_H - 700.0);
                    return iced::widget::operation::scroll_to(
                        list_id(),
                        scrollable::AbsoluteOffset {
                            x: 0.0,
                            y: self.scroll_pos,
                        },
                    );
                }
            }
            Msg::Closed => return iced::exit(),
            Msg::Tick => {
                self.ticks += 1;
                self.speaker = (self.ticks as usize / 36) % 4;
                let dt = self.last.elapsed().as_secs_f64();
                if dt >= 1.0 {
                    let cpu = cpu_time_100ns();
                    let cpu_pct = (cpu - self.cpu_last) as f64 / 1e7 / dt * 100.0;
                    let ram = private_mb();
                    let vn = VIEW_N.swap(0, Ordering::Relaxed).max(1);
                    let vus = VIEW_US.swap(0, Ordering::Relaxed);
                    println!(
                        "{} | Takt {:.0}/s | view() {:.3} ms/Aufruf | CPU {:.1}% | RAM {:.0} MB",
                        if self.naive { "naiv" } else { "virtualisiert" },
                        (self.ticks - self.ticks_last) as f64 / dt,
                        vus as f64 / vn as f64 / 1000.0,
                        cpu_pct,
                        ram
                    );
                    if self.start.elapsed().as_secs() >= 3 {
                        self.cpu_sum += cpu_pct;
                        self.reports += 1;
                        self.ram_max = self.ram_max.max(ram);
                    }
                    self.last = Instant::now();
                    self.cpu_last = cpu;
                    self.ticks_last = self.ticks;
                }
                if self.start.elapsed().as_secs() >= self.secs {
                    println!(
                        "\n=== ZUSAMMENFASSUNG ===\nØ CPU {:.1}% (100% = 1 Kern), RAM max {:.0} MB",
                        self.cpu_sum / self.reports.max(1) as f64,
                        self.ram_max
                    );
                    return iced::exit();
                }
            }
        }
        Task::none()
    }

    fn view(&self, id: window::Id) -> Element<'_, Msg> {
        let t0 = Instant::now();
        let el = if Some(id) == self.overlay {
            self.overlay_view()
        } else {
            self.main_view()
        };
        VIEW_US.fetch_add(t0.elapsed().as_micros() as u64, Ordering::Relaxed);
        VIEW_N.fetch_add(1, Ordering::Relaxed);
        el
    }

    fn overlay_view(&self) -> Element<'_, Msg> {
        let names = ["Anna", "Ben", "Chris", "Dana"];
        let mut col = column![].spacing(6);
        for (i, n) in names.iter().enumerate() {
            let speaking = i == self.speaker;
            let dot = if speaking { "🟢" } else { "⚫" };
            col = col.push(text(format!("{dot} {n}")).size(22).color(if speaking {
                Color::WHITE
            } else {
                Color::from_rgb(0.7, 0.7, 0.7)
            }));
        }
        container(col)
            .padding(14)
            .style(|_| container::Style {
                background: Some(Color::from_rgba(0.05, 0.05, 0.08, 0.6).into()),
                border: iced::Border {
                    radius: 10.0.into(),
                    ..Default::default()
                },
                ..Default::default()
            })
            .into()
    }

    fn main_view(&self) -> Element<'_, Msg> {
        let mut side = column![text("Freunde").size(18)].spacing(6).padding(10);
        for (i, n) in [
            "Anna",
            "Ben",
            "Chris",
            "Dana",
            "Space: Zocken",
            "Space: Familie",
        ]
        .iter()
        .enumerate()
        {
            side = side.push(
                button(text(*n))
                    .width(Length::Fill)
                    .on_press(Msg::Select(i)),
            );
        }
        let side = container(side).width(200).height(Length::Fill);

        let list: Element<Msg> = if self.naive {
            let mut col = column![];
            for s in &self.items {
                col = col.push(container(text(s.as_str()).size(14)).height(ROW_H));
            }
            scrollable(col)
                .id(list_id())
                .on_scroll(|v| Msg::Scrolled(v.absolute_offset().y, v.bounds().height))
                .height(Length::Fill)
                .into()
        } else {
            let first = ((self.offset / ROW_H) as usize).saturating_sub(2).min(N);
            let count = (self.viewport_h / ROW_H).ceil() as usize + 6;
            let last = (first + count).min(N);
            let mut col = column![Space::new().height(Length::Fixed(first as f32 * ROW_H))];
            for s in &self.items[first..last] {
                col = col.push(container(text(s.as_str()).size(14)).height(ROW_H));
            }
            col = col.push(Space::new().height(Length::Fixed((N - last) as f32 * ROW_H)));
            scrollable(col)
                .id(list_id())
                .on_scroll(|v| Msg::Scrolled(v.absolute_offset().y, v.bounds().height))
                .height(Length::Fill)
                .into()
        };

        let main = column![
            text(format!(
                "Channel #allgemein – {} Nachrichten – Auswahl {}",
                N, self.selected
            ))
            .size(18),
            list,
            text_input("Nachricht schreiben …", &self.input)
                .on_input(Msg::Input)
                .padding(10)
                .size(16),
        ]
        .spacing(8)
        .padding(10);
        container(row![side, main])
            .style(|t: &Theme| container::Style {
                background: Some(t.palette().background.into()),
                ..Default::default()
            })
            .into()
    }

    fn subscription(&self) -> Subscription<Msg> {
        Subscription::batch([
            iced::time::every(Duration::from_micros(16_667)).map(|_| Msg::Tick),
            iced::time::every(Duration::from_millis(16)).map(|_| Msg::AutoScroll),
            window::close_events().map(|_| Msg::Closed),
        ])
    }
}

fn main() -> iced::Result {
    let args: Vec<String> = std::env::args().collect();
    let naive = args.iter().any(|a| a == "--naive");
    let autoscroll = args.iter().any(|a| a == "--autoscroll");
    let overlay = !args.iter().any(|a| a == "--no-overlay");
    let secs: u64 = args
        .iter()
        .position(|a| a == "--secs")
        .and_then(|i| args.get(i + 1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(20);
    iced::daemon(
        move || boot(naive, autoscroll, secs, overlay),
        App::update,
        App::view,
    )
    .title(|_: &App, _| "Campfire UI-Prototyp".to_string())
    .subscription(App::subscription)
    .theme(|_: &App, _| Theme::Dark)
    .style(|_: &App, theme: &Theme| iced::theme::Style {
        background_color: Color::TRANSPARENT,
        text_color: theme.palette().text,
    })
    .run()
}
