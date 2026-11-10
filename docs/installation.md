# Installing Campfire

1. On the project's **Releases** page, download `Campfire-win-Setup.exe`.
2. Double-click it. Campfire installs for your user (no administrator rights needed) and starts.
3. Done. When a new version is available, Campfire tells you; it only downloads and installs it after you confirm.

## Windows warning "Windows protected your PC" (SmartScreen)

The installer is not (yet) signed with a paid certificate. Windows therefore warns about every new, rarely downloaded
file. This does not indicate a virus.

1. Click **More info**.
2. Click **Run anyway**.

This is only needed for the first installation. Updates installed from within the app don't trigger the warning.

## Channels

- `stable`: regular releases (default).
- `beta`: pre-releases (tags like `v1.2.0-beta.1`); if you download the beta installer, you stay on the beta channel.

## For developers: creating a release

`git tag v0.1.0 && git push origin v0.1.0` starts `.github/workflows/release.yml`. The workflow builds, packages with
Velopack (`vpk`), checks the installer size (≤ 120 MB) and creates the release. The build includes streaming and video previews (feature `video`); for that the workflow downloads ffmpeg (LGPL, shared libraries, BtbN build) and packs its DLLs into the installer.
The update check reads `releases/latest/download`; for that the repository (or a separate release repository)
must be public. Alternatively, the environment variable `CAMPFIRE_UPDATE_URL` can point to your own web space.
