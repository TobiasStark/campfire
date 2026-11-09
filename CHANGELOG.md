# Changelog

## Unreleased

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
