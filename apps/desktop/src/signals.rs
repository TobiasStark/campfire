//! Signale von ausserhalb der iced-Schleife (Tray, zweite Instanz, Kernereignisse).

use iced::futures::{SinkExt, Stream};
use std::sync::{Mutex, OnceLock};
use tokio::sync::mpsc;

#[derive(Debug, Clone)]
pub enum Signal {
    /// Fenster zeigen (Tray-Klick, zweite Instanz).
    Show,
    /// Toast angeklickt: Fenster zeigen und diesen Channel oeffnen.
    OpenChannel(app_proto::event::SpaceId, app_proto::event::ChannelId),
    Quit,
    /// Netz ist heruntergefahren; Prozess beenden.
    Exit,
    /// Gespeichertes Design wurde geladen.
    Theme(app_core::settings::ThemeChoice),
    /// Freundesliste hat sich geaendert; `true` bei neuer eingehender Anfrage.
    FriendsChanged(bool),
    PresenceChanged,
    /// Neue oder geaenderte Space-Daten (Event-Log).
    SpacesChanged,
    /// Teilnehmerliste des Voice-Channels hat sich geaendert.
    VoiceChanged,
    /// Stream-Zustand hat sich geaendert (Angebote, Zuschauer, Bild).
    StreamChanged,
    /// Dateien: Fortschritt oder Zustand hat sich geaendert.
    FilesChanged,
    /// Stumm/Taub per Hotkey.
    Hotkey(app_hotkeys::engine::Action),
    /// Hotkey-Belegung hat sich geaendert.
    HotkeysChanged,
}

static TX: OnceLock<mpsc::UnboundedSender<Signal>> = OnceLock::new();
static RX: Mutex<Option<mpsc::UnboundedReceiver<Signal>>> = Mutex::new(None);

/// Einmal beim Start aufrufen, bevor Signale gesendet werden.
pub fn init() {
    let (tx, rx) = mpsc::unbounded_channel();
    if TX.set(tx).is_ok() {
        *RX.lock().unwrap() = Some(rx);
    }
}

pub fn send(s: Signal) {
    if let Some(tx) = TX.get() {
        let _ = tx.send(s);
    }
}

/// Stream fuer `Subscription::run`; gibt die Signale einmalig an iced weiter.
pub fn stream() -> impl Stream<Item = Signal> {
    iced::stream::channel(32, async |mut out| {
        let rx = RX.lock().unwrap().take();
        let Some(mut rx) = rx else {
            std::future::pending::<()>().await;
            return;
        };
        while let Some(s) = rx.recv().await {
            if out.send(s).await.is_err() {
                break;
            }
        }
    })
}
