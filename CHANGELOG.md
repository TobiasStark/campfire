# Änderungen

## 0.6.1 (2026-11-09)

### Oberfläche
- Neue Einstellungsseite „Farben“: Jede Farbe der Oberfläche (Hintergründe, Linien, Text, Akzent, Erfolg, Warnung, Gefahr, Video, Avatare, Overlay, Schalter und Regler) lässt sich einzeln ändern – per Regler für Rot, Grün, Blau und Deckkraft oder als Hex-Wert – und einzeln oder gesamt zurücksetzen. Dunkel und hell haben getrennte Farben.
- Ändert man Akzent, Erfolg, Warnung oder Gefahr, passen sich Hover-, Text- und sanfte Varianten sowie die Schriftfarbe darauf automatisch an, solange sie nicht selbst festgelegt wurden.
- Voreinstellungen (Ozean, Wald, Rose, Bernstein, Violett, Mitternacht/OLED), Suche, Export und Import über die Zwischenablage.

## 0.6.0 (2026-11-08)

### Oberfläche
- Überarbeitete Oberfläche mit einheitlichen Farben, Abständen, Schriftgrößen und Rundungen in hellem und dunklem Design. Alle Texte erfüllen die Kontrastvorgaben (WCAG AA, per Test geprüft).
- Leiste links: Markierung für den gewählten Space, Punkt bei Ungelesenem, Zähler für Erwähnungen. „+“ springt direkt zum Formular für einen neuen Space und setzt den Cursor ins Namensfeld.
- Channel-Liste mit Abschnitten, Ungelesen-Zählern und Sprechanzeige im Voice-Channel. Profilbilder zeigen den Status (online, abwesend, nicht stören).
- Chat: Nachrichten derselben Person werden zusammengefasst, Datumstrenner und „Neu“-Linie, Erwähnungen hervorgehoben. Beim Überfahren erscheinen „Antworten“ und „Bearbeiten“ sowie die Uhrzeit. „Neueste Nachrichten“ bringt dich zurück ans Ende. Das Eingabefeld wächst mit dem Text.
- Anhänge als Karten mit Fortschritt, Startseite (Freunde) und Einstellungen mit Navigation, Abschnitten und Schaltern statt Häkchen. Leere Zustände erklären, was als Nächstes zu tun ist.
- Löschen eines Channels und Entfernen eines Freundes fragen jetzt erst nach.
- Hinweis auf eine fehlende Sicherung führt direkt zu „Konto“ in den Einstellungen.

### Intern
- Neues Designsystem in `apps/desktop/src/ui.rs` (Farbwerte, Abstände, Bausteine für Knöpfe, Felder, Abschnitte, Banner, Profilbilder).
- Nur Debug-Builds: `CAMPFIRE_MULTI_INSTANCE=1` erlaubt eine zweite Instanz (mit eigenem `APPDATA`) für Screenshots und Tests mit zwei Geräten.

## 0.5.1 (2026-11-08)

### Sicherheit
- Dateien, deren Name auf `.`, Leerzeichen oder unsichtbare Zeichen endet (z. B. `rechnung.exe.`), werden jetzt wie ausführbare Dateien behandelt: „Öffnen“ fragt erst nach. Vorher ließ sich die Warnung so umgehen.
- Direktnachrichten: Ein Dritter kann keinen „DM“-Space mehr unterschieben, um mitzulesen. Als Direktnachricht zählt nur noch ein Space mit genau dir und dem Freund.
- Blockieren und Entfernen trennen eine bestehende Verbindung sofort. Vorher bekam der Blockierte bis zum nächsten Verbindungsabbruch weiter Voice, Chat und Dateien. Die App wählt blockierte oder entfernte Freunde auch nicht mehr von sich aus an.
- Spaces, in die dich niemand eingeladen hat, tauchen nicht mehr auf, auch nicht in Ungelesen-Zählern und Benachrichtigungen. Dateien gibt die App nur an Mitglieder eines Spaces weiter, in dem auch du Mitglied bist.
- Nachrichten zählen nur noch von Leuten, die der Ersteller des Spaces wirklich hinzugefügt hat. Nach dem Entfernen nimmt die App keine neuen Nachrichten der Person mehr an.
- Ein Mitglied kann die Uhren der anderen nur noch um höchstens 3 Stunden vorstellen (vorher 24).
- Freunde sehen deinen Voice-Channel und ob du streamst nur noch, wenn sie Mitglied des Spaces sind. Andere sehen nur deinen Status.
- Downloads brechen ab, wenn eine Datei größer ist als angegeben. Vorschaubilder sind auf 256 KB begrenzt. Bilder mit absurden Abmessungen (Speicher-Bomben) werden nicht mehr dekodiert, sehr große Bilder werden für die Anzeige verkleinert.
- „Öffnen“ fragt jetzt bei allen Dateitypen nach, die nicht als harmlos bekannt sind (vorher nur bei einer Liste gefährlicher Endungen, der z. B. `.msc`, `.chm`, `.iso`, `.docm` oder `.html` fehlten). Gespeicherte und geöffnete Dateien sind wie Browser-Downloads als „aus dem Internet“ markiert, damit SmartScreen und die geschützte Ansicht in Office greifen.
- Geöffnete Anhänge werden nach 24 Stunden aus dem Temp-Ordner entfernt (beim nächsten Start).
- Backups überschreiben nie ein vorhandenes Backup. Liegt schon eines vor, schlägt die Ersteinrichtung den Import vor.
- **Backup-Passwörter brauchen jetzt mindestens 12 Zeichen** (vorher 8) und werden genau so übernommen, wie du sie tippst (Leerzeichen am Rand zählen mit). Ältere Backups lassen sich weiter öffnen.
- Fremde können die App nicht mehr mit vielen halb offenen Verbindungen oder übergroßen Anfragen ausbremsen. Wer blockiert ist, erfährt das nicht mehr aus der Antwort auf eine Freundschaftsanfrage.
- Installer: ffmpeg kommt aus einem festen, per Prüfsumme geprüften Build. Der Release wird erst nach bestandenen Tests gebaut. Die Lizenztexte von ffmpeg (LGPL) liegen bei (`THIRD-PARTY-NOTICES.txt`).

### Intern
- Mitgliedschaften liegen im Speicher. Voice und Presence warten nicht mehr auf die Datenbank, wenn gerade ein großer Abgleich läuft.
- Video-Empfang belegt Speicher erst, wenn die Daten ankommen. Netzwerkteile laufen nach einem Absturz in einem anderen Task weiter (kein Mutex-Poisoning).

### Bekannte Lücken
- Ein entferntes Mitglied kann noch Nachrichten mit einem Zeitstempel vor seiner Entfernung nachreichen (braucht eine Protokolländerung).
- Die lokale Datenbank ist nicht verschlüsselt; geschützt ist sie nur über dein Windows-Konto (z. B. mit BitLocker).

## 0.5.0 (2026-11-08)

### Überarbeitete Oberfläche
- Untere Leiste aufgeräumt: nur noch Name, Status (Auswahlliste) und Einstellungen. Autostart und Design sind in den Einstellungen; der Arbeitsspeicher steht als Hinweis am Namen.
- Space-Leiste mit Hinweisen beim Daraufzeigen (Name des Spaces); Start-Knopf heißt „Start“.
- Channel-Liste: Textkanäle und Sprachkanäle getrennt, ungelesen als Zähler-Plakette. Verwaltung (Umbenennen, Reihenfolge, Löschen, Meldungen) erst nach „Bearbeiten“, damit die Liste ruhig bleibt.
- Sprach-Leiste unten in der Channel-Liste, solange man verbunden ist: „Mikro an/aus“, „Ton an/aus“, Verlassen, Bildschirm teilen.
- Ein Space öffnet beim Betreten gleich den ersten Textkanal.
- Startseite: Freunde zuerst mit Erklärung, „Neuen Space anlegen“ darunter. Hinweise für leere Listen.
- Ersteinrichtung: ein Schritt (Name, optionale Sicherung); „Ich habe schon ein Konto“ klappt die Wiederherstellung auf.
- Texte mit richtigen Umlauten und einfacheren Wörtern („Sicherung“ statt „Backup“).

### Neu: Dateien teilen
- Dateien an Nachrichten anhängen: Auswahl-Dialog (📎), Drag&Drop oder Strg+V (auch kopierte Dateien aus dem Explorer). Bis 4 GB je Datei.
- Die Dateien liegen nur auf den Geräten der Mitglieder und werden direkt zwischen ihnen übertragen. Wer eine Datei hat, gibt sie an andere weiter, auch wenn der Absender nicht mehr online ist. Angefangene Downloads setzen fort.
- Einstellungen → Allgemein: „Dateien automatisch laden bis X MB“ (Standard 100 MB, 0 = nie).
- Vorschauen: Bilder erscheinen im Chat, ein Klick öffnet die Großansicht. Vorschaubilder (höchstens 100 KB) laden immer automatisch.
- „Speichern“, „Speichern unter …“ und „Anpinnen“ an jedem Anhang. Gefährliche Dateitypen (z. B. .exe, .bat) öffnen erst nach einer Warnung.
- Einstellungen → Allgemein: Größe des Datei-Speichers (Standard 10 GB, 0 = unbegrenzt). Wird er voll, werden die ältesten, nicht angepinnten Dateien entfernt. Selbst geteilte Dateien sind automatisch angepinnt. Der belegte Platz wird angezeigt.
- Bei Dateien, die noch nicht da sind, steht „verfügbar bei N Personen“ bzw. „gerade bei niemandem online verfügbar“. Ein gescheiterter Download startet von selbst, sobald jemand online kommt.

### Neu: Bildschirm-Streams
- Bildschirm oder Fenster im Voice-Channel streamen (LIVE-Knopf, Hotkey), mit Quellen-Auswahl, Hardware-Encoder-Erkennung (NVENC/AMF/QSV, sonst Software), automatischer Bitrate und zwei Qualitätsstufen (Simulcast). Zuschauer sehen den Stream im Fenster, im Vollbild oder als eigenes Fenster, mit Statistik-Overlay.
- Spielsound wird mitgeschickt (Windows 11; unter Windows 10 weist die App darauf hin).
- Der Streamer sieht, wer zuschaut; ist der Upload erschöpft, werden weitere Zuschauer abgelehnt.
- Videodateien bekommen im Chat eine Vorschau (erster Frame).
- Installer enthält jetzt ffmpeg (LGPL); er ist deshalb größer (Grenze 120 MB statt 80 MB).

### Intern
- Neue Crates `app-blobs` (Dateispeicher auf iroh-blobs, Zugriffsprüfung je Space) und `app-video`.
- Neues Netzwerk-Protokoll für Dateien (ALPN `/iroh-bytes/4`); siehe `docs/protocol.md`.
- Der Ereignis-Verlauf kennt `FileShare` mit Vorschaubild.

### Bekannte Lücken
- Nicht von Hand getestet (nur automatisch): Streams mit echtem Spiel, Ton-Synchronität und zwei echte PCs, Oberfläche für Dateien (Drag&Drop, Auswahl, Zwischenablage, Fortschritt, Großansicht, Speichern unter, Anpinnen), Cache-Aufräumen im echten Betrieb, Verfügbarkeitsanzeige.
- Ein beim Beenden abgebrochener Download bleibt nur liegen, wenn er nach dem Neustart wieder gestartet wird, bevor der erste Aufräumlauf (nach höchstens einer Minute) kommt.
- Der Installer ist nicht signiert (SmartScreen-Hinweis, siehe `docs/installation.md`).
