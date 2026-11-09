# Campfire

Campfire is a voice, streaming and chat app for groups of friends on Windows. It is fully peer-to-peer: no server of its own, no cloud, no account with any provider. Messages, files and streams go directly between your friends' devices.

## Features

- **Voice channels** with Opus, push-to-talk (global hotkey, mouse buttons included) and an in-game overlay
- **Screen streaming** in voice channels with hardware encoding (NVENC/AMF/QSV), game audio (Windows 11) and two quality tiers
- **Text chat** in spaces and direct messages, with basic Markdown, replies, editing, mentions and unread counters
- **File sharing** up to 4 GB, directly between members, with previews and resumable downloads
- **Friends** via friend code, presence (online, away, do not disturb) and blocking
- **Backup** of your account as a password-protected file; keys are stored locally, protected by DPAPI
- Light and dark theme, every color customizable
- English by default, German selectable in the settings
- Auto-updates via Velopack

## Installation

Download the installer `Campfire-win-Setup.exe` from the [Releases](https://github.com/TobiasStark/campfire/releases) page. Details and notes on the SmartScreen warning: [docs/installation.md](docs/installation.md).

## Technology

Rust, [iroh](https://github.com/n0-computer/iroh) (QUIC, hole punching, relay fallback), [iced](https://github.com/iced-rs/iced) for the UI, SQLite for local storage, ffmpeg (LGPL) for video.

| Directory | Contents |
|---|---|
| `apps/desktop` | The Windows app (`campfire.exe`) |
| `apps/node` | Placeholder for a future headless node |
| `crates/app-proto` | Wire protocol (see [docs/protocol.md](docs/protocol.md)) |
| `crates/app-net` | Networking on iroh |
| `crates/app-core` | Identity, friends, presence, event log, storage |
| `crates/app-audio` | Voice: capture, playback, Opus, jitter buffer |
| `crates/app-video` | Capture, encoding, decoding |
| `crates/app-blobs` | File storage and transfer (iroh-blobs) |
| `crates/app-hotkeys` | Global hotkeys / push-to-talk |
| `crates/app-testkit` | Helpers for network tests |
| `tools/nettest` | Connectivity test between two machines |

Building: [docs/building.md](docs/building.md). Changes: [CHANGELOG.md](CHANGELOG.md).

## License

Licensed under either of the following, at your option:

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

Unless you explicitly state otherwise, any contribution you submit for inclusion in this project is licensed under the same dual license.

The installer includes ffmpeg libraries under the LGPL, see [packaging/THIRD-PARTY-NOTICES.txt](packaging/THIRD-PARTY-NOTICES.txt).
