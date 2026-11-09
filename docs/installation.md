# Campfire installieren

1. Auf der Seite **Releases** des Projekts die Datei `Campfire-win-Setup.exe` herunterladen.
2. Doppelklick. Campfire installiert sich für deinen Benutzer (keine Administratorrechte nötig) und startet.
3. Fertig. Updates holt die App selbst im Hintergrund; sie werden beim nächsten Start eingespielt.

## Windows-Warnung „Der Computer wurde durch Windows geschützt" (SmartScreen)

Der Installer ist (noch) nicht mit einem bezahlten Zertifikat signiert. Windows warnt deshalb bei jeder neuen,
wenig verbreiteten Datei. Das ist kein Hinweis auf einen Virus.

1. Auf **Weitere Informationen** klicken.
2. Auf **Trotzdem ausführen** klicken.

Das ist nur bei der Erstinstallation nötig. Updates installiert die App selbst ohne Warnung.

## Kanäle

- `stable`: normale Versionen (Standard).
- `beta`: Vorabversionen (Tags wie `v1.2.0-beta.1`); wer sich den Beta-Installer lädt, bleibt im Kanal beta.

## Für Entwickler: Release erstellen

`git tag v0.1.0 && git push origin v0.1.0` startet `.github/workflows/release.yml`. Der Workflow baut, verpackt mit
Velopack (`vpk`), prüft die Installergröße (≤ 120 MB) und legt das Release an. Der Build enthält Streams und Video-Vorschauen (Feature `video`); dafür lädt der Workflow ffmpeg (LGPL, geteilte Bibliotheken, BtbN-Build) und packt dessen DLLs in den Installer.
Der Update-Check liest `releases/latest/download`; dafür muss das Repository (oder ein separates Release-Repository)
öffentlich sein. Alternativ zeigt die Umgebungsvariable `CAMPFIRE_UPDATE_URL` auf einen eigenen Webspace.
