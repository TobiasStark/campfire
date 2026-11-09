# Campfire

Campfire ist eine Voice-, Stream- und Chat-App für Freundesgruppen unter Windows. Sie läuft komplett Peer-to-Peer: kein eigener Server, keine Cloud, kein Konto bei einem Anbieter. Nachrichten, Dateien und Streams gehen direkt zwischen den Geräten der Freunde hin und her.

## Funktionen

- **Voice-Channels** mit Opus, Push-to-Talk (globaler Hotkey, auch Maustasten) und Overlay über dem Spiel
- **Bildschirm-Streams** in Voice-Channels mit Hardware-Encoding (NVENC/AMF/QSV), Spielsound (Windows 11) und zwei Qualitätsstufen
- **Text-Chat** in Spaces und Direktnachrichten, mit einfachem Markdown, Antworten, Bearbeiten, Erwähnungen und Ungelesen-Zählern
- **Dateien teilen** bis 4 GB, direkt zwischen den Mitgliedern, mit Vorschauen und Fortsetzen abgebrochener Downloads
- **Freunde** per Freundescode, Status (online, abwesend, nicht stören), Blockieren
- **Sicherung** des Kontos als passwortgeschützte Datei; die Schlüssel liegen lokal per DPAPI geschützt
- Helles und dunkles Design, jede Farbe einstellbar
- Auto-Updates über Velopack

## Installieren

Den Installer `Campfire-win-Setup.exe` von der Seite [Releases](https://github.com/TobiasStark/campfire/releases) laden. Details und Hinweise zur SmartScreen-Warnung: [docs/installation.md](docs/installation.md).

## Technik

Rust, [iroh](https://github.com/n0-computer/iroh) (QUIC, Hole-Punching, Relay-Fallback), [iced](https://github.com/iced-rs/iced) für die Oberfläche, SQLite für die lokale Datenhaltung, ffmpeg (LGPL) für Video.

| Verzeichnis | Inhalt |
|---|---|
| `apps/desktop` | Die Windows-App (`campfire.exe`) |
| `apps/node` | Platzhalter für einen späteren Headless-Knoten |
| `crates/app-proto` | Wire-Protokoll (siehe [docs/protocol.md](docs/protocol.md)) |
| `crates/app-net` | Netzwerk auf iroh |
| `crates/app-core` | Identität, Freunde, Presence, Event-Log, Speicher |
| `crates/app-audio` | Voice: Aufnahme, Wiedergabe, Opus, Jitter-Buffer |
| `crates/app-video` | Capture, Encoding, Decoding |
| `crates/app-blobs` | Dateispeicher und -übertragung (iroh-blobs) |
| `crates/app-hotkeys` | Globale Hotkeys / Push-to-Talk |
| `crates/app-testkit` | Hilfen für Netzwerk-Tests |
| `tools/nettest` | Verbindungstest zwischen zwei Rechnern |

Bauen: [docs/building.md](docs/building.md). Änderungen: [CHANGELOG.md](CHANGELOG.md).

## Lizenz

Lizenziert nach deiner Wahl unter einer der folgenden Lizenzen:

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT-Lizenz ([LICENSE-MIT](LICENSE-MIT))

Beiträge, die du zur Aufnahme in dieses Projekt einreichst, stehen ohne ausdrückliche gegenteilige Angabe unter derselben Doppellizenz.

Der Installer enthält ffmpeg-Bibliotheken unter der LGPL, siehe [packaging/THIRD-PARTY-NOTICES.txt](packaging/THIRD-PARTY-NOTICES.txt).
