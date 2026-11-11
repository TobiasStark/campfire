# Changelog

## 0.6.6 (unreleased)

### New
- The settings sidebar now shows the installed version at the bottom.

### Security
- Updated the update framework (Velopack 1.3). It brings a newer ZIP library that fixes several vulnerabilities in extracting update packages.

## 0.6.5 (2026-11-11)

### Fixes
- Voice calls sounded choppy on connections with uneven delays. Late packets are now waited for instead of being skipped, so the delay adapts to the real network jitter.
- After a pause in speaking (voice activation), playback no longer resumes with an empty buffer, which caused stuttering on the first words.

### New
- A "Stats" toggle in the voice dock shows per-speaker call quality (jitter, buffer, loss, dropouts) and whether the connection is direct or via relay.

### Changes
- Voice data that cannot be sent in time is dropped instead of queued for seconds when the connection is congested.

## 0.6.4 (2026-11-10)

### Fixes
- Messages stayed at "⏳ waiting" for the sender although the recipient already had them. The delivery confirmation was stored, but the chat was only refreshed when another new message arrived. The chat now updates as soon as the confirmation comes in.
- When a friend catches up on missed messages after coming online, the sender now sees "delivered" right away instead of after the next sync up to a minute later.
- The "New messages since your last visit" banner now disappears after clicking it, clicking a message, jumping down or sending.

### Compatibility
- **This version cannot talk to 0.6.3 and earlier.** The network protocol changed for the security fixes below (friend codes, friend requests, handshake, sync, how messages are signed). Everyone needs to update.
- **Spaces and direct messages from 0.6 are deleted once** when you update, because messages are now signed differently; files received in them are removed at the next cleanup (pinned files stay). Your account, identity and friends are kept; spaces have to be created again.
- Old friend codes no longer work. Share your new code from Settings → Account.

### Security
Files and downloads
- Nothing is downloaded automatically any more. Under Settings → General you choose what may load on its own (previews, images, videos, audio, documents, archives, up to a size you set); programs never do, and everything else waits for a click.
- Friends can no longer fetch files from spaces you share with others: a file is only handed out to members of a space it was shared in, in which you are a member too and through which it reached you. Only devices with an authenticated connection may open file connections, and only a few each.
- Opening programs, scripts, archives, Office documents and PDFs from others asks first; downloaded files get the "from the internet" mark (Mark of the Web). File names are cleaned more thoroughly (reserved device names, invisible format characters). Temporary copies of opened attachments are deleted when the message is deleted and at the next start.

Updates
- Updates are no longer installed automatically. Campfire tells you about a new version; downloading and installing only happen when you click. In release builds the update source can no longer be changed through an environment variable.

Spaces and messages
- Being added to a space now needs your consent: invitations appear as a request, and you can leave any space. Spaces whose creator you blocked stay hidden, and messages of blocked users are neither shown nor passed on.
- Removed members can no longer keep writing by backdating messages, and the creation of a space is bound to its creator.
- Friends can no longer push spaces onto you that you were never invited to, nor flood you: events are only accepted for spaces you belong to or were invited to, unaccepted spaces take at most 5000 events, each author at most 10,000 per hour. Data of spaces you left is deleted once your leave has arrived.
- Large batches of events no longer freeze the app (bounded batches, ordered processing, delayed rebuilds).
- A friend can no longer lock up your database by flooding you with messages: your own actions always go first, signatures are checked outside the database, and a friend who sends forged or unrequested messages is cut off at once and ignored for 5 minutes. While Campfire runs, no other program can lock its database.
- If someone signs two different messages with the same number (to show different people different things), Campfire now detects it, keeps the proof and shows a warning in the space naming that member.
- Clocks: messages may be at most one hour in the future (was three hours), and a manipulated counter can no longer break the order.
- A space only counts as a direct message when it has exactly two members; a group named like a DM can no longer pose as one or notify like one.
- Removing a reaction only works within its own space; file sizes from others are checked.

Friends and identity
- Friend codes contain an invitation secret: a request only counts with your current code, and "New code" in Settings → Account makes all earlier codes useless. Unanswered requests are capped (the oldest drop out) and slow senders are cut off after 2 s, so request spam can no longer lock out real requests.
- Your IP address is no longer revealed to people who only have your friend code: friend requests go through a separate, relay-only endpoint in both directions. Friends you accepted can still see your IP unless you enable "Relay only" (Settings → Network).
- You can give friends a nickname, and Campfire warns about names that look alike. Every friend shows a key fingerprint you can compare outside the app and mark as verified; renaming no longer changes how a friend appears to you without notice.
- New: recovery key (Settings → Account). It never stays on your devices. If a device or backup is stolen, it moves your account to a new key; your friends switch over automatically and block the old one. Set it up now: friends only remember the first recovery key they see.
- Backups: the password must be strong (at least 12 characters, no simple patterns, with a strength indicator and a generator), you choose where the file goes, and Campfire warns when the folder is synced to the cloud.

Calls and network
- Calls: after a declined or missed call, the same friend can't ring again for 60 s; "Do not disturb" silences calls; an incoming call flashes the taskbar instead of stealing focus.
- Per-friend limits on control messages, sync and video streams and voice packets, so one friend flooding the connection no longer slows down everyone else. Repeated sync requests are bundled.
- Someone who only knows your friend code can no longer block friends' connections by holding open unauthenticated handshakes: each source (IP or relay node ID) may keep at most 4 open.
- Relay addresses must be public `https://` servers; links in messages are only opened when they are plain `http(s)` addresses without hidden user names, and look-alike (IDN) domains are shown.

Video
- Streams from others are decoded in a separate, locked-down process (no privileges, low integrity, no other processes, memory limit). A bug in ffmpeg or the graphics driver no longer gives an attacker your account, database or files. A decoder that hangs is stopped.
- Frames above 4K are rejected, and the number of incoming video frames at the same time is limited.
- Campfire no longer offers to restart as administrator. When it runs as administrator anyway, streams of others are not played.
- Video thumbnails only open common containers (MP4/MOV, Matroska/WebM, raw H.264) and codecs (H.264, HEVC, AV1, VP8, VP9); this keeps the rarely used ffmpeg parsers (AVI, FLIC, PSD, MagicYUV, ...) out of reach.

Local data
- Storage no longer grows without limit. Messages have a storage limit (Settings → General, default 1 GB): when it is full, the oldest messages of whoever uses the most space are removed first, your own last, so spam pushes out the spammer's messages instead of your history. Removed messages leave a tiny signed placeholder, so syncing keeps working for everyone. Files are removed by last use (opened, saved, viewed or downloaded) instead of message age, and files whose message is gone are removed; pinned files stay. Removed files can be downloaded again from friends who still have them.
- The database moved from the roaming profile to `%LOCALAPPDATA%\CampfireData` (once, automatically), so it no longer travels to other computers with a roaming profile.
- The diagnostics export hides IP addresses, opens for review before you share it, and old crash files are cleaned up.
- The keyboard hook only runs while a push-to-talk or mute key is set. Another program can no longer block Campfire from starting by taking its single-instance lock.
- Internal: locks no longer cascade panics; file authorization no longer blocks the network thread; integer overflows from peer values are handled.

Build and release
- Releases are built with read-only tokens; GitHub Actions are pinned to commits, dependencies are checked for advisories (`cargo deny`), and Dependabot proposes updates weekly. A weekly job reports new ffmpeg security releases (see `docs/building.md`).

### Known issues
- ffmpeg 9.0.2 (included) is affected by CVEs published on 2026-11-09 (e.g. CVE-2026-125241 in the FLIC decoder, CVE-2026-125239 in DXVA2 HEVC); no fixed ffmpeg release exists yet. Thumbnails never open FLIC files, and streams from others are decoded in the locked-down decoder process. An update follows once ffmpeg publishes a fix.

## 0.6.3 (2026-11-09)

### Audio
- Choppy voice fixed: after every short pause (silence detection or voice activation) the jitter buffer threw away the already buffered beginning of the next sentence (about 120 ms each time). Buffered audio is now kept, send pauses are skipped cleanly, and short utterances are played even before the buffer is full.
- The jitter buffer starts at 60 ms and grows on its own when packets arrive late or the connection fluctuates (up to 240 ms); it shrinks again only after a calm while. Catching up after a jitter spike happens in quiet moments, so it is not audible.
- The playback buffer grows automatically when the sound card runs dry (crackling, dropouts) and shrinks again after 30 s without dropouts.

### Calls and voice
- The voice bar at the bottom of the sidebar is now visible on every page (home, direct messages, other spaces) – for voice channels and for direct calls. It shows how long you have been connected, with whom or in which channel, and lets you jump there.
- Direct messages have a "Call" button in the header. During a call it shows "In call", the duration and "Hang up"; while ringing, "Accept"/"Decline" or "Cancel".
- Calling a friend while you are in a voice channel leaves the channel and calls (instead of failing with "already in a call").
- The call bar shows the duration, whether the other person is speaking (ring around the avatar), muted or deafened, and the mic/sound buttons now show their state.
- Direct message list and friends list mark who you are in a call with or who is ringing; the friends list offers "Hang up"/"Accept" right there.
- The window title shows an ongoing or ringing call and the voice channel.
- Errors when calling (e.g. friend offline) now appear in the call bar on every page, not only on the home page.

### Fixes
- Call notes ("No answer", "Declined", "Busy", "Missed call") are now translated.

## 0.6.2 (2026-11-09)

### Language
- All texts are now English by default. German can be selected under Settings → General → Language; the choice applies immediately and is remembered.
- Technical error messages are English only.
- README, documentation, release workflows and third-party notices are now in English.

### Fixes
- Direct calls: when both sides joined at nearly the same moment, one side could miss the other's join message and end up without audio. A participant who appears for the first time is now answered with our own join again.

## 0.6.1 (2026-11-09)

### Interface
- New "Colors" settings page: every color of the interface (backgrounds, lines, text, accent, success, warning, danger, video, avatars, overlay, switches and sliders) can be changed individually – with sliders for red, green, blue and opacity or as a hex value – and reset individually or all at once. Dark and light have separate colors.
- When you change accent, success, warning or danger, the hover, text and soft variants and the foreground color adapt automatically as long as they have not been set by hand.
- Presets (Ocean, Forest, Rose, Amber, Violet, Midnight/OLED), search, and export and import via the clipboard.

## 0.6.0 (2026-11-08)

### Interface
- Reworked interface with consistent colors, spacing, font sizes and corner radii in light and dark themes. All text meets the contrast requirements (WCAG AA, checked by a test).
- Left rail: marker for the selected space, dot for unread messages, counter for mentions. "+" jumps straight to the form for a new space and puts the cursor in the name field.
- Channel list with sections, unread counters and a speaking indicator in the voice channel. Avatars show presence (online, away, do not disturb).
- Chat: consecutive messages from the same person are grouped, date dividers and a "New" line, highlighted mentions. Hovering shows "Reply" and "Edit" as well as the time. "Latest messages" takes you back to the end. The input field grows with the text.
- Attachments as cards with progress, a home page (friends) and settings with navigation, sections and switches instead of checkboxes. Empty states explain what to do next.
- Deleting a channel and removing a friend now ask for confirmation first.
- The hint about a missing backup leads directly to "Account" in the settings.

### Internal
- New design system in `apps/desktop/src/ui.rs` (color values, spacing, building blocks for buttons, fields, sections, banners, avatars).
- Debug builds only: `CAMPFIRE_MULTI_INSTANCE=1` allows a second instance (with its own `APPDATA`) for screenshots and tests with two devices.

## 0.5.1 (2026-11-08)

### Security
- Files whose name ends in `.`, a space or invisible characters (e.g. `invoice.exe.`) are now treated like executable files: "Open" asks first. Previously the warning could be bypassed this way.
- Direct messages: a third party can no longer slip in a "DM" space to read along. Only a space with exactly you and the friend counts as a direct message.
- Blocking and removing now cut an existing connection immediately. Previously the blocked person kept receiving voice, chat and files until the next disconnect. The app also no longer dials blocked or removed friends on its own.
- Spaces you were not invited to no longer show up, not even in unread counters and notifications. The app only passes files on to members of a space you are also a member of.
- Messages only count from people the space creator actually added. After removal, the app no longer accepts new messages from that person.
- A member can now move other members' clocks forward by at most 3 hours (previously 24).
- Friends only see your voice channel and whether you are streaming if they are members of the space. Others only see your presence.
- Downloads abort if a file is larger than announced. Thumbnails are limited to 256 KB. Images with absurd dimensions (memory bombs) are no longer decoded, very large images are downscaled for display.
- "Open" now asks for every file type not known to be harmless (previously only for a list of dangerous extensions that lacked e.g. `.msc`, `.chm`, `.iso`, `.docm` or `.html`). Saved and opened files are marked as "from the internet" like browser downloads, so SmartScreen and Office Protected View apply.
- Opened attachments are removed from the temp folder after 24 hours (on the next start).
- Backups never overwrite an existing backup. If one already exists, first-run setup suggests importing it.
- **Backup passwords now need at least 12 characters** (previously 8) and are taken exactly as typed (leading and trailing spaces count). Older backups can still be opened.
- Strangers can no longer slow the app down with many half-open connections or oversized requests. Being blocked is no longer revealed by the response to a friend request.
- Installer: ffmpeg comes from a fixed, checksum-verified build. The release is only built after tests pass. The ffmpeg (LGPL) license texts are included (`THIRD-PARTY-NOTICES.txt`).

### Internal
- Memberships are kept in memory. Voice and presence no longer wait for the database while a large sync is running.
- Video reception only allocates memory when data arrives. Network parts continue in a different task after a crash (no mutex poisoning).

### Known gaps
- A removed member can still submit messages with a timestamp from before their removal (needs a protocol change).
- The local database is not encrypted; it is only protected through your Windows account (e.g. with BitLocker).

## 0.5.0 (2026-11-08)

### Reworked interface
- Bottom bar tidied up: only name, status (dropdown) and settings. Autostart and theme are in the settings; memory usage is shown as a hint on the name.
- Space bar with tooltips on hover (name of the space); the home button is called "Home".
- Channel list: text channels and voice channels separated, unread shown as a counter badge. Management (rename, reorder, delete, notifications) only after "Edit", to keep the list calm.
- Voice bar at the bottom of the channel list while connected: "Mic on/off", "Sound on/off", Leave, Share screen.
- A space opens its first text channel when entered.
- Home page: friends first with an explanation, "Create new space" below. Hints for empty lists.
- First-run setup: one step (name, optional backup); "I already have an account" expands the restore form.
- Texts with proper umlauts and simpler wording ("Backup" instead of technical terms).

### New: file sharing
- Attach files to messages: file picker (📎), drag and drop or Ctrl+V (including files copied from Explorer). Up to 4 GB per file.
- Files only live on the members' devices and are transferred directly between them. Whoever has a file passes it on to others, even when the sender is no longer online. Interrupted downloads resume.
- Settings → General: "Download files automatically up to X MB" (default 100 MB, 0 = never).
- Previews: images appear in the chat, a click opens the large view. Thumbnails (at most 100 KB) are always loaded automatically.
- "Save", "Save as …" and "Pin" on every attachment. Dangerous file types (e.g. .exe, .bat) open only after a warning.
- Settings → General: size of the file storage (default 10 GB, 0 = unlimited). When it is full, the oldest unpinned files are removed. Files you share yourself are pinned automatically. The used space is shown.
- For files that are not there yet, it says "available from N people" or "not available from anyone online right now". A failed download starts again by itself as soon as someone comes online.

### New: screen streaming
- Stream a screen or window in the voice channel (LIVE button, hotkey), with source picker, hardware encoder detection (NVENC/AMF/QSV, otherwise software), automatic bitrate and two quality tiers (simulcast). Viewers watch the stream in the window, fullscreen or in its own window, with a statistics overlay.
- Game audio is sent along (Windows 11; on Windows 10 the app points this out).
- The streamer sees who is watching; when upload is exhausted, further viewers are refused.
- Video files get a preview in the chat (first frame).
- The installer now includes ffmpeg (LGPL); it is therefore larger (limit 120 MB instead of 80 MB).

### Internal
- New crates `app-blobs` (file storage on iroh-blobs, per-space access check) and `app-video`.
- New network protocol for files (ALPN `/iroh-bytes/4`); see `docs/protocol.md`.
- The event log knows `FileShare` with a thumbnail.

### Known gaps
- Not tested by hand (only automatically): streams with a real game, audio sync and two real PCs, the file UI (drag and drop, picker, clipboard, progress, large view, save as, pin), cache cleanup in real operation, availability display.
- A download aborted at shutdown is only kept if it is restarted after the restart, before the first cleanup run (after at most one minute).
- The installer is not signed (SmartScreen notice, see `docs/installation.md`).
