//! Campfire Desktop: Einstiegspunkt, Einzelinstanz, Tray.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod chat_ui;
mod colors_ui;
mod core;
mod diag;
mod files;
mod hotkeys;
mod markdown;
mod overlay;
mod platform;
mod settings_ui;
mod signals;
mod sounds;
mod source_picker;
mod spaces;
#[cfg_attr(not(feature = "video"), allow(dead_code))]
mod stream;
mod stream_ui;
mod toast;
mod ui;
mod updater;
mod video;
mod video_view;

use signals::{send, Signal};
use std::time::Instant;
use tray_icon::menu::{Menu, MenuEvent, MenuItem};
use tray_icon::{Icon, MouseButton, TrayIcon, TrayIconBuilder, TrayIconEvent};

/// Einfaches Icon: abgerundetes blaues Quadrat mit hellem Punkt.
fn icon(badge: bool) -> Icon {
    const N: u32 = 32;
    let mut px = Vec::with_capacity((N * N * 4) as usize);
    for y in 0..N {
        for x in 0..N {
            let (dx, dy) = (x as f32 - 15.5, y as f32 - 15.5);
            let inside = dx.abs() < 14.0 && dy.abs() < 14.0;
            let dot = dx * dx + dy * dy < 36.0;
            // Roter Punkt oben rechts, wenn Ungelesenes da ist.
            let (bx, by) = (x as f32 - 24.0, y as f32 - 8.0);
            let c = match (inside, dot) {
                _ if badge && bx * bx + by * by < 49.0 => [237, 66, 69, 255],
                (false, _) => [0, 0, 0, 0],
                (true, true) => [255, 255, 255, 255],
                (true, false) => [88, 101, 242, 255],
            };
            px.extend_from_slice(&c);
        }
    }
    Icon::from_rgba(px, N, N).expect("gueltiges Icon")
}

thread_local! {
    /// Das Tray-Icon gehoert dem Hauptthread; die iced-Schleife laeuft ebenfalls dort.
    static TRAY: std::cell::RefCell<Option<TrayIcon>> = const { std::cell::RefCell::new(None) };
    /// Menu entries ("Open", "Quit"), kept to re-translate them when the language changes.
    static TRAY_ITEMS: std::cell::RefCell<Option<(MenuItem, MenuItem)>> = const { std::cell::RefCell::new(None) };
}

/// Re-applies the current UI language to the tray menu.
pub fn refresh_tray_language() {
    TRAY_ITEMS.with(|i| {
        if let Some((open, quit)) = i.borrow().as_ref() {
            open.set_text(app_core::i18n::tr("Open", "Öffnen"));
            quit.set_text(app_core::i18n::tr("Quit", "Beenden"));
        }
    });
}

/// Zeigt Ungelesenes am Tray-Icon (roter Punkt, Zahl im Tooltip).
pub fn set_tray_badge(unread: u32, mentions: u32) {
    TRAY.with(|t| {
        if let Some(t) = t.borrow().as_ref() {
            let tip = match (unread, mentions) {
                (0, _) => "Campfire".to_owned(),
                (n, 0) => app_core::tr!("Campfire: {n} unread", "Campfire: {n} ungelesen"),
                (n, m) => app_core::tr!(
                    "Campfire: {n} unread, {m} mentions",
                    "Campfire: {n} ungelesen, {m} Erwähnungen"
                ),
            };
            let _ = t.set_tooltip(Some(tip));
            let _ = t.set_icon(Some(icon(unread > 0)));
        }
    });
}

fn make_tray() -> Option<TrayIcon> {
    let menu = Menu::new();
    let open = MenuItem::with_id("open", "Open", true, None);
    let quit = MenuItem::with_id("quit", "Quit", true, None);
    menu.append_items(&[&open, &quit]).ok()?;
    TRAY_ITEMS.with(|i| *i.borrow_mut() = Some((open.clone(), quit.clone())));
    MenuEvent::set_event_handler(Some(|e: MenuEvent| match e.id.as_ref() {
        "open" => send(Signal::Show),
        "quit" => send(Signal::Quit),
        _ => {}
    }));
    TrayIconEvent::set_event_handler(Some(|e: TrayIconEvent| {
        if let TrayIconEvent::Click {
            button: MouseButton::Left,
            button_state: tray_icon::MouseButtonState::Up,
            ..
        } = e
        {
            send(Signal::Show);
        }
    }));
    TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_menu_on_left_click(false)
        .with_tooltip("Campfire")
        .with_icon(icon(false))
        .build()
        .ok()
}

fn main() -> iced::Result {
    // Decoder-Prozess fuer fremde Videostreams (M-10): vor allem anderen, ohne Logs, DB oder UI.
    #[cfg(feature = "video")]
    if std::env::args().nth(1).as_deref() == Some(app_video::sandbox::CHILD_ARG) {
        app_video::sandbox::child_main();
    }
    // Installer-/Update-Aufrufe von Velopack zuerst behandeln (kann das Programm beenden).
    updater::init();
    diag::init();
    let start = Instant::now();
    let background = std::env::args().any(|a| a == "--background");

    // Nach einem Neustart (Relay, Update) kurz warten, bis die alte Instanz beendet ist.
    let restarted = std::env::args().any(|a| a == "--restarted");
    let mut single = platform::acquire_single_instance();
    for _ in 0..if restarted { 50 } else { 0 } {
        if single.is_some() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
        single = platform::acquire_single_instance();
    }
    // Nur Entwicklung: weitere Instanz mit eigenem LOCALAPPDATA (Screenshots, Zwei-Geraete-Tests).
    let multi = cfg!(debug_assertions) && std::env::var_os("CAMPFIRE_MULTI_INSTANCE").is_some();
    if single.is_none() && !multi {
        platform::signal_existing_instance();
        return Ok(());
    }
    let _single = single;

    // D14: DX12 gibt den GPU-Speicher beim Schliessen des Fensters frei.
    if std::env::var_os("WGPU_BACKEND").is_none() {
        std::env::set_var("WGPU_BACKEND", "dx12");
    }

    updater::spawn_check();
    signals::init();
    platform::spawn_show_listener(|| send(Signal::Show));
    TRAY.with(|t| *t.borrow_mut() = make_tray());

    // Tokio-Laufzeit fuer den Kern (iced bringt eigene mit; der Kern nutzt die von iced).
    iced::daemon(
        move || app::App::boot(start, background),
        app::App::update,
        app::App::view,
    )
    .title(app::App::title)
    .theme(app::App::theme)
    .subscription(app::App::subscription)
    .run()
}
