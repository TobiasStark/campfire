# Campfire bauen

## Standard-Build (ohne Streams)

Voraussetzung: Windows 10/11, Rust (die Version legt `rust-toolchain.toml` fest, `rustup` installiert sie automatisch).

```
cargo build --release -p app-desktop --bin campfire
cargo test --workspace
```

Ohne das Feature `video` fehlen Bildschirm-Streams und Video-Vorschauen; alles andere funktioniert.

## Mit Streams (Feature `video` / `ffmpeg`)

1. LGPL-Shared-Build von ffmpeg 9.0 entpacken, z. B. von [BtbN/FFmpeg-Builds](https://github.com/BtbN/FFmpeg-Builds/releases) (`…-win64-lgpl-shared-9.0.zip`). Den Build, den die CI nutzt, nennt `FFMPEG_URL` in `.github/workflows/release.yml`.
2. LLVM für bindgen installieren: `winget install LLVM.LLVM`.
3. Umgebungsvariablen setzen, am bequemsten in einer lokalen, nicht eingecheckten `.cargo/config.toml`:

   ```toml
   [env]
   FFMPEG_DIR = "C:\\pfad\\zu\\ffmpeg"
   LIBCLANG_PATH = "C:\\Program Files\\LLVM\\bin"
   # Include-Pfade von Clang, MSVC und Windows SDK, sonst meldet bindgen „stdint.h not found“.
   BINDGEN_EXTRA_CLANG_ARGS = "-I\"…\" -I\"…\""
   ```

   Alternativ in einer „Developer PowerShell for VS“ bauen und `BINDGEN_EXTRA_CLANG_ARGS` aus `%INCLUDE%` erzeugen, so wie es `.github/workflows/ci.yml` tut.
4. Bauen:

   ```
   cargo build --release -p app-desktop --bin campfire --features video
   ```

5. Zum Ausführen müssen die DLLs aus `ffmpeg\bin` neben der `campfire.exe` liegen oder im `PATH` stehen.

## Nützliche Umgebungsvariablen

| Variable | Wirkung |
|---|---|
| `CAMPFIRE_UPDATE_URL` | Eigene Update-Quelle statt der GitHub-Releases |
| `CAMPFIRE_MULTI_INSTANCE=1` | Nur Debug-Builds: erlaubt eine zweite Instanz (mit eigenem `APPDATA`) |
| `CAMPFIRE_TRACE` | Loggt Zeitmessungen beim Start |
| `CAMPFIRE_DEV_NAME=<Name>` | Legt bei der Ersteinrichtung ohne Oberfläche eine Identität an (Tests) |
