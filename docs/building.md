# Building Campfire

## Standard build (without streaming)

Requirements: Windows 10/11, Rust (the version is pinned by `rust-toolchain.toml`; `rustup` installs it automatically).

```
cargo build --release -p app-desktop --bin campfire
cargo test --workspace
```

Without the `video` feature, screen streaming and video previews are missing; everything else works.

## With streaming (feature `video` / `ffmpeg`)

1. Extract an LGPL shared build of ffmpeg 9.0, e.g. from [BtbN/FFmpeg-Builds](https://github.com/BtbN/FFmpeg-Builds/releases) (`…-win64-lgpl-shared-9.0.zip`). The build used by CI is named by `FFMPEG_URL` in `.github/workflows/release.yml`.
2. Install LLVM for bindgen: `winget install LLVM.LLVM`.
3. Set environment variables, most conveniently in a local, uncommitted `.cargo/config.toml`:

   ```toml
   [env]
   FFMPEG_DIR = "C:\\path\\to\\ffmpeg"
   LIBCLANG_PATH = "C:\\Program Files\\LLVM\\bin"
   # Include paths of Clang, MSVC and the Windows SDK, otherwise bindgen reports "stdint.h not found".
   BINDGEN_EXTRA_CLANG_ARGS = "-I\"…\" -I\"…\""
   ```

   Alternatively, build in a "Developer PowerShell for VS" and generate `BINDGEN_EXTRA_CLANG_ARGS` from `%INCLUDE%`, as `.github/workflows/ci.yml` does.
4. Build:

   ```
   cargo build --release -p app-desktop --bin campfire --features video
   ```

5. To run, the DLLs from `ffmpeg\bin` must be next to `campfire.exe` or on the `PATH`.

## Useful environment variables

| Variable | Effect |
|---|---|
| `CAMPFIRE_UPDATE_URL` | Custom update source instead of GitHub releases |
| `CAMPFIRE_MULTI_INSTANCE=1` | Debug builds only: allows a second instance (with its own `APPDATA`) |
| `CAMPFIRE_TRACE` | Logs timings during startup |
| `CAMPFIRE_DEV_NAME=<name>` | Creates an identity during first-run setup without the UI (tests) |
