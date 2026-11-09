//! PTT-Demo: zeigt Drücken/Loslassen von F8, Mouse4 und Mouse5 global an.
//! Start: cargo run -p app-hotkeys --example ptt_demo --release
//! Beendet sich nach 120 s selbst (oder Strg+C).

use app_hotkeys::{is_elevated, max_callback_micros, HotkeyService};
use std::time::{Duration, Instant};

fn main() {
    println!("PTT-Demo (M0-04)");
    println!(
        "Dieser Prozess läuft {} Administratorrechten.",
        if is_elevated() { "MIT" } else { "OHNE" }
    );
    println!("Zeigt alle Tasten und Seitentasten. Läuft 120 s, dann Statistik.\n");

    let (svc, rx) = match HotkeyService::start() {
        Ok(v) => v,
        Err(e) => {
            println!("FEHLER: {e}");
            return;
        }
    };
    let start = Instant::now();
    let mut count = 0u32;
    while start.elapsed() < Duration::from_secs(120) {
        if let Ok(ev) = rx.recv_timeout(Duration::from_millis(200)) {
            count += 1;
            println!(
                "[{:>6.2}s] {:?} {}",
                start.elapsed().as_secs_f32(),
                ev.trigger,
                if ev.pressed {
                    "GEDRÜCKT  -> PTT AN"
                } else {
                    "LOSGELASSEN -> PTT AUS"
                }
            );
        }
    }
    println!(
        "\n{count} Ereignisse, längste Callback-Laufzeit: {} µs (Ziel < 1000 µs)",
        max_callback_micros()
    );
    drop(svc);
}
