# Protokoll (Version 1)

Quelle der Wahrheit ist der Code in `crates/app-proto`; dieses Dokument beschreibt ihn.

## Grundregeln

- Wire-Format ist **postcard** (D12), deterministisch. Ausnahme: Voice-Header (feste Struktur).
- Streams tragen genau eine Nachricht pro Richtung und Anfrage, Obergrenze `MAX_MESSAGE_LEN` = 1 MiB. Groessere oder mit Restbytes versehene Nachrichten werden abgelehnt (`decode`).
- Version steckt im ALPN. Inkompatible Aenderung = neues ALPN (`/2`), der Client spricht uebergangsweise beide. Innerhalb von `/1` nur anhaengen (neue Enum-Varianten am Ende), nie umsortieren, da postcard Varianten per Index kodiert.
- Schluessel (`PublicKey`, 32 Byte) und `Signature` (64 Byte) sind rohe Bytes; Pruefung passiert in `app-core`.

## ALPNs

| ALPN | Konstante | Zweck |
|---|---|---|
| `app/peer/1` | `alpn::PEER` | Hauptverbindung, nur bekannte Geraete |
| `app/hello/1` | `alpn::HELLO` | Freundschaftsanfragen, offen fuer Fremde (Rate-Limit) |

iroh-gossip und iroh-blobs nutzen ihre eigenen ALPNs. Dateien: ALPN `/iroh-bytes/4` (iroh-blobs 0.104), eingehende Verbindungen kommen als `NetEvent::Blobs` und gehen an `app-blobs`; ausgeliefert wird nur, was der `Authorizer` für das anfragende Gerät erlaubt, Push ist aus.

## Kanaele auf `app/peer/1`

| Kanal | Transport | Typ |
|---|---|---|
| Control | dauerhafter bidirektionaler Stream | `control::Handshake` / `HandshakeReply`, danach `ControlMessage` |
| Sync | Stream pro Lauf | `sync::SyncMessage` |
| Voice | QUIC-Datagramme | `voice::VoiceHeader` + Opus |
| Video | ein uni-Stream pro Frame | `video::FrameHeader` + kodierte Nutzdaten; Steuerung ueber `ControlMessage::Video(VideoMsg)` |

### Streams und Rahmen

- Der Anrufer oeffnet den ersten bidirektionalen Stream = Control. Nachrichten darauf sind `[len u32 BE][postcard]`.
- Jeder weitere bidirektionale Stream beginnt mit einem Kanal-Byte: `1` = Sync (danach Rahmen wie oben). Unbekannte Kanaele werden ignoriert.
- Unidirektionale Streams sind Video (kein Kanal-Byte).
- Haben beide Seiten gleichzeitig verbunden, bleibt die Verbindung, die der Knoten mit der kleineren Device-ID aufgebaut hat; beide Seiten entscheiden gleich.
- QUIC-Keep-Alive 2 s, Idle-Timeout 6 s.

### Handshake

1. Client sendet `Handshake{versions, cert, display_name, avatar_hash}`.
2. Server prueft, dass `cert.device` der Device-ID der Verbindung entspricht, das Zertifikat (`DeviceCert`: User-Signatur ueber `"app/device-cert/1" | user | device | created_at(BE)`) und die Allowlist, antwortet mit `HandshakeReply::Accept{version, handshake}` oder `Reject{reason}`.
3. `version` ist die hoechste gemeinsame Version. Der Client prueft die Antwort des Servers genauso.
4. Bei `Reject` wird die Verbindung geschlossen.

### Voice-Paket

`[flags u8][stream_id u8][seq u32 BE][ts u32 BE][opus...]`, 20-ms-Frames, 10 Byte Header.

**Stream-Ton (M3-07):** gleicher Header mit `stream_id` 1, Opus Stereo (20 ms, 160 kbit/s, FEC). `ts` ist hier Millisekunden auf der Medienuhr des Streams (wie `FrameHeader.ts`), nicht 48-kHz-Samples. Der Zuschauer nimmt ihn nur vom Sender an, den er ansieht; `Announce.audio` zeigt, ob Ton läuft.

Beitritt: `ControlMessage::Voice(VoiceSignal::Join{space_id, channel, stream_id})` an jedes verbundene Gerät eines Space-Mitglieds, danach `State{muted, deafened}`; `Leave` beim Verlassen. Datagramme gehen nur an Geräte, die selbst `Join` für denselben Channel gesendet haben und deren Nutzer Mitglied des Spaces ist; empfangen wird nur von diesen. `seq` zählt Frames (nicht gesendete Pakete), damit DTX-Pausen als Lücke erkennbar sind.

Direktanruf: `VoiceSignal::CallInvite{call_id}` an alle verbundenen Geräte des Freundes, Antwort `CallAccept{call_id}` / `CallDecline{call_id, busy}`, Abbruch `CallCancel{call_id}`. Nach Annahme treten beide dem Voice-Channel `(space_id = 0, channel = call_id)` bei; dort sind nur die beiden Gesprächspartner zugelassen. Klingeldauer 30 s.

## Video

Steuerung auf dem Control-Stream als `ControlMessage::Video(VideoMsg)` (am Ende der Enum angehaengt; `Subscribe`/`Unsubscribe`/`RequestKeyframe` direkt in `ControlMessage` sind reserviert und ungenutzt).

| Nachricht | Richtung | Inhalt |
|---|---|---|
| `Announce{stream_id, layers, audio}` | Sender -> Zuschauer | Stufen (`LayerInfo`: layer, codec, Groesse, fps, kbps). Erneut gesendet, wenn sich etwas aendert (z. B. Quellenwechsel). |
| `Stop{stream_id}` | Sender -> Zuschauer | Stream beendet |
| `Subscribe{stream_id, layer, decoders}` | Zuschauer -> Sender | Abo einer Stufe; `decoders` = Codecs, die der Zuschauer dekodieren kann |
| `Unsubscribe{stream_id}` | Zuschauer -> Sender | Abo beenden |
| `KeyframeRequest{stream_id}` | Zuschauer -> Sender | Einstieg oder Erholung nach Verlust |
| `Report(ReceiverReport)` | Zuschauer -> Sender | je Sekunde: received, lost, late, jitter_ms, kbps |
| `Refused{stream_id, reason}` | Sender -> Zuschauer | `NoSuchStream`, `Capacity`, `NoCommonCodec`, `NotAllowed` |

**Frame:** ein unidirektionaler Stream pro Frame, `[stream_id u8][layer u8][flags u8][codec u8][frame_no u32 BE][ts u32 BE][len u32 BE][Nutzdaten]` (16 Byte Kopf). `flags` Bit 0 = Keyframe; `codec` 1 = H.264, 2 = HEVC, 3 = AV1; `len` hoechstens 8 MiB. `frame_no` zaehlt pro Stream, mit Umlauf. `ts` = Millisekunden auf der Medienuhr des Senders; der Stream-Ton (`stream_id` 1) trägt dieselbe Uhr in `ts`, damit lassen sich Bild und Ton abgleichen.

**Regeln:**
- Ein neues Abo loest sofort einen Keyframe aus; der Zuschauer beginnt erst mit einem Keyframe. Zusaetzlich kommt mindestens jede Sekunde einer.
- Der Sender haelt je Zuschauer hoechstens 4 Frames gleichzeitig offen und setzt Streams zurueck, die nach 500 ms nicht bestaetigt sind. Danach bekommt dieser Zuschauer keine Delta-Frames mehr, bis ein Keyframe kommt. Andere Zuschauer sind nicht betroffen, weil jeder Frame je Zuschauer in einer eigenen Task gesendet wird.
- Keyframe-Anforderungen des Senders an den Encoder haben mindestens 300 ms Abstand.
- Empfaenger verwerfen Frames mit kleinerer `frame_no` als erwartet (zu spaet). Eine Luecke fordert einen Keyframe an.

### Upload-Budget und Bitrate (M3-04)

- Verplant werden 80 % des Uploads (Einstellung oder Messung, Standard 10 Mbit/s). Davon gehen je Voice-Partner 64 kbit/s ab (Voice hat Vorrang), der Rest wird durch die Zahl der Zuschauer geteilt (jeder bekommt den Stream einzeln gesendet).
- Liegt der Anteil je Zuschauer unter 1 Mbit/s, antwortet der Sender auf `Subscribe` mit `Refused{Capacity}`; die UI zeigt dann einen Hinweis statt eines kaputten Streams.
- Anpassung einmal pro Sekunde aus den `Report`s: bei >= 3 % verlorenen oder verspaeteten Frames sofort auf 90 % des tatsaechlich Angekommenen (mindestens ein Schritt von 15 % nach unten), nach 3 sauberen Berichten +8 %. Der schlechteste Zuschauer **seiner Stufe** bestimmt die Bitrate dieser Stufe (ohne Simulcast gibt es nur Stufe 0). Der Encoder startet bei einer Aenderung ab 10 % neu und beginnt mit einem Keyframe.

### Simulcast (M3-05)

- Optional (Einstellung, Standard aus) kodiert der Sender zusaetzlich Stufe 1 = 720p30 mit hoechstens 3 Mbit/s (Ziel 2,5 Mbit/s). Jede Stufe hat einen eigenen Encoder, eigene Bitrate und eigene Keyframes; `Announce.layers` nennt die Stufen. Entfaellt die Stufe (Simulcast aus, Quelle ohnehin 720p30), gibt es nur Stufe 0.
- Der Zuschauer abonniert Stufe 0; der Sender setzt ihn automatisch um: 2 schlechte Berichte in Folge (>= 3 % Verlust/Verspaetung) -> Stufe 1; nach 15 sauberen Berichten ein Aufstiegsversuch, bei Misserfolg (Rueckfall binnen 5 Berichten) verdoppelt sich die Wartezeit bis 120 Berichte. Nach jedem Wechsel beginnt der Zuschauer mit einem Keyframe der neuen Stufe. Ein Wechsel aendert die Stufe im `FrameHeader.layer`; eine eigene Nachricht ist nicht noetig.
- Budget: Jeder Zuschauer kostet seine Stufe einzeln (`n0 * r0 + n1 * r1`); Stufe 1 bekommt hoechstens ihre 2,5 Mbit/s, Stufe 0 den Rest.

### Ablauf in der App (M3-06)

- `Announce` geht alle 2 s an die anderen Teilnehmer des eigenen Voice-Channels; Ankündigungen und Abos von anderen werden ignoriert bzw. mit `Refused{NotAllowed}` beantwortet. Eine Ankündigung gilt 8 s. Ein Absender hat höchstens einen Stream (`stream_id` 1).
- Der Zuschauer sendet `Subscribe` mit den Codecs, die sein ffmpeg dekodieren kann, danach jede Sekunde `Report`; `KeyframeRequest` höchstens alle 300 ms.
- `FrameHeader.frame_no` zählt je Stufe lückenlos. Wechselt die Stufe, wertet der Empfänger sie erst ab dem nächsten Keyframe.

## Events

`EventBody{space_id, author, seq, hlc, kind}` wird mit dem User-Key des Autors signiert (Bytes = postcard der Body). `event_id` = blake3 der Body-Bytes, die Signatur ist nicht Teil der ID. `seq` ist pro Autor und Space lueckenlos ab 1. Anzeigereihenfolge: `Hlc` (wall_ms, counter), bei Gleichstand `event_id`.

Arten: `SpaceCreate`, `MemberAdd`, `MemberRemove`, `ChannelCreate`, `ChannelUpdate`, `ChannelDelete`, `Message`, `MessageEdit`, `MessageDelete`, `Reaction`, `ReactionRemove`, `FileShare`. `RoleGrant`/`RoleRevoke` folgen spaeter (am Ende anhaengen).

## Sync

`Heads` austauschen, fehlendes mit `Want{space_id, author, from_seq, to_seq}` anfordern, Antwort als `Events`-Batches, `Done` beendet den Lauf.

- Ablauf (beide Seiten symmetrisch): Initiator sendet `Heads`; der Empfaenger antwortet mit eigenen `Heads` und einem `Want` (auch leer); jede Seite beantwortet das `Want` der anderen mit `Events`* und `Done` und beendet dann ihre Senderichtung. Initiator ist bei `PeerUp` die Seite mit der kleineren Geraete-ID; zusaetzlich gleicht jeder Knoten alle 60 s mit allen Peers ab.
- Push: Neue Events gehen als eigener Stream (`Events`* + `Done`, ohne `Heads`) an verbundene Mitglieder. Wer bis zu 50 neue Events annimmt, reicht sie an seine anderen Peers weiter (Flooding mit Duplikat-Erkennung). Eine Luecke loest einen vollen Abgleich aus.
- `Heads` und `Events` enthalten nur Spaces, in denen der Gegenueber Mitglied ist; `Want` fuer andere Spaces wird leer beantwortet. Batches bleiben unter ca. 400 KiB (Rahmenlimit 1 MiB).
- Annahme eines Events: Signatur (strict) -> Groessen/Zeit (HLC hoechstens 3 h in der Zukunft) -> `seq` muss genau `head + 1` sein (kleiner = Equivocation, groesser = Luecke) -> Autor ist zum Zeitpunkt (HLC) des Events gueltiges Mitglied: Ersteller (`SpaceCreate`) oder vom Ersteller per `MemberAdd` hinzugefuegt und nicht danach entfernt (`MemberRemove` durch Ersteller oder sich selbst); sonst `NotMember` bzw. `Unauthorized`. `SpaceCreate` ist nur gueltig, wenn `space_id = blake3("app/space/1" | author | hlc)[..16]` (kein Vorab-Belegen fremder Spaces) und der Space neu ist.
- Projektion: Events werden in Anzeigereihenfolge angewandt; die Berechtigung (Admin fuer Mitglieder/Channels, Autor fuer Bearbeiten, Admin oder Autor fuers Loeschen) wird dort geprueft. Nicht erlaubte Events bleiben im Log, wirken aber nirgends. Kommt ein Event vor bereits bekannten (HLC-kleiner), wird der Space aus dem Log neu aufgebaut; dadurch ist das Ergebnis unabhaengig von der Ankunftsreihenfolge.
- Bekannte Grenze: Ein entfernter Nutzer erhaelt keine Events mehr (auch nicht die eigene Entfernung); Rollen ausser `admin`/`member` gibt es noch nicht.

## Hello (`app/hello/1`)

Ein bidirektionaler Stream pro Verbindung, ein Rahmen je Richtung: `HelloMessage::{Request, Accept, Decline}` -> `HelloReply::{Received, RateLimited, Blocked, Invalid}`. Namen max. 64, Notiz max. 256 Zeichen (`is_sane`).

- `Request` und `Accept` tragen ein `DeviceCert`; `cert.device` muss der Node-ID der Verbindung entsprechen, sonst `Invalid`. `Decline` hat keinen Inhalt; der Empfaenger ordnet es ueber die Node-ID einer ausgehenden Anfrage zu.
- Ein `Request` eines Blockierten wird wie ein zugestellter mit `Received` beantwortet (verraet die Blockierung nicht) und nicht gespeichert. `Blocked` bleibt im Protokoll, wird aber nicht mehr gesendet. Hello-Rahmen hoechstens 4 KiB. Sind beide gleichzeitig ausstehend, werden beide Freunde.
- `RateLimited`: Token-Bucket pro Node-ID (3 Burst, 1 je 10 s) und global (20 Burst, 5/s); mehr als 100 unbeantwortete Anfragen werden ebenfalls so beantwortet. Der Absender versucht es spaeter erneut (Backoff 5 s bis 5 min).
- Zustellung ist at-least-once ueber eine persistente Outbox; alle Nachrichten sind idempotent.

## Presence

`ControlMessage::Presence{status, voice, streaming}` wird beim Verbinden und bei jeder Aenderung gesendet. Der Empfaenger behandelt `Invisible` wie "offline"; ohne bisher empfangene Presence gilt der Peer ebenfalls als offline. Faellt die Verbindung weg (auch hart, Idle-Timeout 6 s), ist der Peer offline. Bei mehreren Geraeten eines Nutzers gilt der zuletzt gemeldete sichtbare Zustand. Auto-Abwesend (Standard 10 min ohne Eingabe) aendert nur `Online` zu `Away`.

## Freundescode

Binaer `[version u8][flags u8][user 32][node_id 32][relay_len u8, relay]?[check 4]`, base32 ohne Padding, Grossbuchstaben. `check` = erste 4 Bytes von blake3 ueber alles davor (Abweichung von PLAN 6.2: Tippfehler werden erkannt). Flag 1 = Relay-URL vorhanden. Eingabe ist unempfindlich gegen Gross-/Kleinschreibung, Leerraum und `-`. Link: `app://add/<code>`. Enthaelt keine geheimen Daten.
