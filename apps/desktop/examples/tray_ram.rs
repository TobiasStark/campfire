//! M1-07 Messung: Gibt iced (wgpu) den GPU-Speicher frei, wenn das letzte Fenster schliesst?
//!
//!   WGPU_BACKEND=dx12 tray_ram [--open-secs 4] [--total-secs 14]
//!
//! Oeffnet ein Fenster, schliesst es nach `open-secs`, laesst den Daemon weiterlaufen
//! und gibt jede Sekunde den Private Working Set aus.

use std::time::{Duration, Instant};

use iced::{widget::text, window, Element, Subscription, Task, Theme};
use windows::Win32::System::{
    ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX},
    Threading::GetCurrentProcess,
};

#[derive(Debug, Clone)]
enum Msg {
    Opened,
    Tick,
}

struct App {
    win: Option<window::Id>,
    start: Instant,
    open_secs: u64,
    total_secs: u64,
    closed: bool,
}

fn private_mb() -> f64 {
    let mut c = PROCESS_MEMORY_COUNTERS_EX {
        cb: std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32,
        ..Default::default()
    };
    // SAFETY: c ist gross genug und cb passt zur Struktur.
    unsafe {
        let _ = GetProcessMemoryInfo(
            GetCurrentProcess(),
            &mut c as *mut _ as *mut PROCESS_MEMORY_COUNTERS,
            c.cb,
        );
    }
    c.PrivateUsage as f64 / 1_048_576.0
}

fn arg(name: &str, default: u64) -> u64 {
    let a: Vec<String> = std::env::args().collect();
    a.iter()
        .position(|x| x == name)
        .and_then(|i| a.get(i + 1))
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

impl App {
    fn update(&mut self, msg: Msg) -> Task<Msg> {
        match msg {
            Msg::Opened => Task::none(),
            Msg::Tick => {
                let t = self.start.elapsed().as_secs();
                println!(
                    "t={t:>2}s  fenster={}  private={:.0} MB",
                    if self.closed { "zu  " } else { "offen" },
                    private_mb()
                );
                if t >= self.open_secs && !self.closed {
                    self.closed = true;
                    if let Some(id) = self.win.take() {
                        return window::close(id);
                    }
                }
                if t >= self.total_secs {
                    std::process::exit(0);
                }
                Task::none()
            }
        }
    }

    fn view(&self, _id: window::Id) -> Element<'_, Msg> {
        text("tray_ram").into()
    }

    fn subscription(&self) -> Subscription<Msg> {
        iced::time::every(Duration::from_secs(1)).map(|_| Msg::Tick)
    }
}

fn boot() -> (App, Task<Msg>) {
    let (id, open) = window::open(window::Settings::default());
    (
        App {
            win: Some(id),
            start: Instant::now(),
            open_secs: arg("--open-secs", 4),
            total_secs: arg("--total-secs", 14),
            closed: false,
        },
        open.map(|_| Msg::Opened),
    )
}

fn main() -> iced::Result {
    iced::daemon(boot, App::update, App::view)
        .subscription(App::subscription)
        .theme(|_: &App, _| Theme::Dark)
        .run()
}
