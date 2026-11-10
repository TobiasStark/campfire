# Protocol (version 1)

The source of truth is the code in `crates/app-proto`; this document describes it.

## Ground rules

- The wire format is **postcard** (D12), deterministic. Exception: the voice header (fixed structure).
- Streams carry exactly one message per direction and request, with an upper limit `MAX_MESSAGE_LEN` = 1 MiB. Larger messages, or messages with trailing bytes, are rejected (`decode`).
- The version lives in the ALPN. An incompatible change means a new ALPN (`/2`); the client speaks both during a transition. Within `/1`, only append (new enum variants at the end) and never reorder, because postcard encodes variants by index.
- Keys (`PublicKey`, 32 bytes) and `Signature` (64 bytes) are raw bytes; verification happens in `app-core`.

## ALPNs

| ALPN | Constant | Purpose |
|---|---|---|
| `app/peer/1` | `alpn::PEER` | Main connection, known devices only |
| `app/hello/1` | `alpn::HELLO` | Friend requests, open to strangers (rate-limited); only on the rendezvous endpoint |

iroh-gossip and iroh-blobs use their own ALPNs. Files: ALPN `/iroh-bytes/4` (iroh-blobs 0.104); incoming connections arrive as `NetEvent::Blobs` and go to `app-blobs`. Only devices with an authenticated peer connection may connect (at most a few connections per device), and a file is only served to a user who is a member of a space in which it was shared, in which I am a member too and through which it reached me. Push is off.

### Rendezvous endpoint

Every device runs a second iroh endpoint that only speaks `app/hello/1`. Its key is derived from the device key (`hello::rendezvous_secret`), and in Internet mode it only uses the relay, never direct paths. The friend code names this endpoint, not the device, so someone who only knows the code never reaches the main endpoint and never learns an IP address. Outgoing hello connections also leave from the rendezvous endpoint. `Request` and `Accept` carry `binding` = signature of `cert.device` over `"app/rendezvous/1" | device | rendezvous`, which ties the endpoint to the device; the receiver checks it against the node ID of the hello connection. The main endpoint does not accept `app/hello/1`.

## Channels on `app/peer/1`

| Channel | Transport | Type |
|---|---|---|
| Control | persistent bidirectional stream | `control::Handshake` / `HandshakeReply`, then `ControlMessage` |
| Sync | one stream per run | `sync::SyncMessage` |
| Voice | QUIC datagrams | `voice::VoiceHeader` + Opus |
| Video | one uni stream per frame | `video::FrameHeader` + encoded payload; control via `ControlMessage::Video(VideoMsg)` |

### Streams and framing

- The caller opens the first bidirectional stream = Control. Messages on it are `[len u32 BE][postcard]`.
- Every further bidirectional stream starts with a channel byte: `1` = Sync (followed by frames as above). Unknown channels are ignored.
- Unidirectional streams are video (no channel byte).
- If both sides connected at the same time, the connection initiated by the node with the smaller device ID is kept; both sides decide the same way.
- QUIC keep-alive 2 s, idle timeout 6 s.
- Per peer token buckets: control messages 100/s (burst 400), new sync streams 20/s (burst 100), new video streams (one per frame) 240/s (burst 480), datagrams 400/s (burst 800). An empty bucket slows down reading from that peer only (QUIC then throttles the sender); excess datagrams are dropped.

### Handshake

1. The client sends `Handshake{versions, cert, display_name, avatar_hash, recovery}`.
2. The server checks that `cert.device` matches the device ID of the connection, the certificate (`DeviceCert`: user signature over `"app/device-cert/1" | user | device | created_at(BE)`; `created_at` at most one day in the future) and the allowlist, and replies with `HandshakeReply::Accept{version, handshake}` or `Reject{reason}`.
   `recovery` is the commitment of the user's recovery key (see "Key rotation"); a friend stores the first one it sees and never replaces it.
3. `version` is the highest common version. The client checks the server's reply in the same way.
4. On `Reject` the connection is closed.

### Voice packet

`[flags u8][stream_id u8][seq u32 BE][ts u32 BE][opus...]`, 20 ms frames, 10-byte header.

**Stream audio (M3-07):** same header with `stream_id` 1, Opus stereo (20 ms, 160 kbit/s, FEC). Here `ts` is milliseconds on the stream's media clock (like `FrameHeader.ts`), not 48 kHz samples. The viewer only accepts it from the sender they are watching; `Announce.audio` shows whether audio is running.

Joining: `ControlMessage::Voice(VoiceSignal::Join{space_id, channel, stream_id})` to every connected device of a space member, followed by `State{muted, deafened}`; `Leave` when leaving. Datagrams only go to devices that themselves sent `Join` for the same channel and whose user is a member of the space; receiving only happens from those. `seq` counts frames (not sent packets), so DTX pauses show up as a gap.

Direct call: `VoiceSignal::CallInvite{call_id}` to all connected devices of the friend, answered by `CallAccept{call_id}` / `CallDecline{call_id, busy}`, canceled by `CallCancel{call_id}`. After acceptance both join the voice channel `(space_id = 0, channel = call_id)`; only the two call partners are allowed there. Ring duration 30 s.

## Video

Control runs on the control stream as `ControlMessage::Video(VideoMsg)` (appended at the end of the enum; `Subscribe`/`Unsubscribe`/`RequestKeyframe` directly in `ControlMessage` are reserved and unused).

| Message | Direction | Content |
|---|---|---|
| `Announce{stream_id, layers, audio}` | sender -> viewer | Layers (`LayerInfo`: layer, codec, size, fps, kbps). Sent again when something changes (e.g. source switch). |
| `Stop{stream_id}` | sender -> viewer | Stream ended |
| `Subscribe{stream_id, layer, decoders}` | viewer -> sender | Subscribe to a layer; `decoders` = codecs the viewer can decode |
| `Unsubscribe{stream_id}` | viewer -> sender | End the subscription |
| `KeyframeRequest{stream_id}` | viewer -> sender | Entry point or recovery after loss |
| `Report(ReceiverReport)` | viewer -> sender | every second: received, lost, late, jitter_ms, kbps |
| `Refused{stream_id, reason}` | sender -> viewer | `NoSuchStream`, `Capacity`, `NoCommonCodec`, `NotAllowed` |

**Decoding:** the viewer decodes in a separate process (same executable, `--campfire-video-decoder`) that drops all privileges, lowers itself to Low integrity and runs in a job object (one process, 2 GiB memory, no clipboard or desktop access). Packets and decoded NV12 frames travel over pipes; the app checks every frame (at most 3840 x 2160, sizes match) and kills a decoder that needs more than 5 s for a packet. After 3 failures the viewer gives up.

**Frame:** one unidirectional stream per frame, `[stream_id u8][layer u8][flags u8][codec u8][frame_no u32 BE][ts u32 BE][len u32 BE][payload]` (16-byte header). `flags` bit 0 = keyframe; `codec` 1 = H.264, 2 = HEVC, 3 = AV1; `len` at most 8 MiB. `frame_no` counts per stream and wraps around. `ts` = milliseconds on the sender's media clock; stream audio (`stream_id` 1) carries the same clock in `ts`, so picture and sound can be aligned.

**Rules:**
- A new subscription triggers a keyframe immediately; the viewer only starts at a keyframe. In addition, one arrives at least every second.
- The sender keeps at most 4 frames open per viewer at the same time and resets streams that are not acknowledged after 500 ms. After that this viewer gets no delta frames until a keyframe arrives. Other viewers are not affected, because every frame is sent in its own task per viewer.
- Keyframe requests from the sender to the encoder are at least 300 ms apart.
- Receivers discard frames with a `frame_no` smaller than expected (too late). A gap requests a keyframe.

### Upload budget and bitrate (M3-04)

- 80 % of the upload is budgeted (setting or measurement, default 10 Mbit/s). From that, 64 kbit/s per voice partner is deducted (voice has priority), and the rest is divided by the number of viewers (each gets the stream sent individually).
- If the share per viewer is below 1 Mbit/s, the sender answers `Subscribe` with `Refused{Capacity}`; the UI then shows a hint instead of a broken stream.
- Adjustment once per second from the `Report`s: at >= 3 % lost or late frames, immediately to 90 % of what actually arrived (at least a step of 15 % down); after 3 clean reports, +8 %. The worst viewer **of its layer** determines the bitrate of that layer (without simulcast there is only layer 0). The encoder restarts on a change of 10 % or more and begins with a keyframe.

### Simulcast (M3-05)

- Optionally (setting, off by default) the sender additionally encodes layer 1 = 720p30 with at most 3 Mbit/s (target 2.5 Mbit/s). Each layer has its own encoder, its own bitrate and its own keyframes; `Announce.layers` lists the layers. If the layer does not apply (simulcast off, source already 720p30), there is only layer 0.
- The viewer subscribes to layer 0; the sender switches them automatically: 2 bad reports in a row (>= 3 % loss/lateness) -> layer 1; after 15 clean reports an upgrade attempt, and on failure (fallback within 5 reports) the waiting time doubles up to 120 reports. After every switch the viewer starts with a keyframe of the new layer. A switch changes the layer in `FrameHeader.layer`; no separate message is needed.
- Budget: every viewer costs its layer individually (`n0 * r0 + n1 * r1`); layer 1 gets at most its 2.5 Mbit/s, layer 0 the rest.

### Flow in the app (M3-06)

- `Announce` goes to the other participants of your own voice channel every 2 s; announcements and subscriptions from others are ignored or answered with `Refused{NotAllowed}`. An announcement is valid for 8 s. A sender has at most one stream (`stream_id` 1).
- The viewer sends `Subscribe` with the codecs its ffmpeg can decode, then a `Report` every second; `KeyframeRequest` at most every 300 ms.
- `FrameHeader.frame_no` counts without gaps per layer. When the layer changes, the receiver only evaluates it from the next keyframe.

## Events

`EventBody{space_id, author, seq, hlc, kind}` is signed with the author's user key. The signed bytes contain the content only as a hash: postcard of `{"app/event/2", space_id, author, seq, hlc, tag, content}` with `tag` = fixed number of the kind (`event::tag`) and `content = blake3::derive_key("campfire 2026-11 event content", postcard(kind))`. `event_id` = blake3 of these bytes; the signature is not part of the ID. `seq` is gap-free per author and space, starting at 1. Display order: `Hlc` (wall_ms, counter), ties broken by `event_id`. Events signed by 0.6 (whole body) are not valid any more; the update deletes them once.

Kinds: `SpaceCreate` (0), `MemberAdd` (1), `MemberRemove` (2), `ChannelCreate` (3), `ChannelUpdate` (4), `ChannelDelete` (5), `Message` (6), `MessageEdit` (7), `MessageDelete` (8), `Reaction` (9), `ReactionRemove` (10), `FileShare` (11), `Pruned{tag, content}`. `RoleGrant`/`RoleRevoke` follow later (append at the end).

### Storage limit and stubs

`Pruned{tag, content}` is a stub of an event whose content was removed to stay within the storage limit. It has the same signed bytes, so the same `event_id` and signature as the original, and keeps the author's `seq` chain gap-free for everyone who syncs it later. Stubs are only valid for message kinds (tags 6 to 11); a stub of a member, space or channel event is `Malformed` (otherwise one could hide an invitation or removal). Nobody may create a new event as a stub. The projection ignores stubs.

Each node keeps the stored message data (events plus projection, estimated) below a limit (Settings → General, default 1 GB, at least 100 MB), checked 30 s after start and then every 5 min. Above the limit it prunes down to 90 %: always the oldest events of the (space, author) that can free the most, own events last. Pruning a message or file share also prunes everything that refers to it (edits, deletion, reactions, also from others) and removes it from the view; a `MessageDelete` is never pruned on its own (the deleted message would come back). Stubs are passed on like normal events; a node that has an event only as a stub cannot deliver its content. Files whose message was pruned are removed from the file store at the next cleanup (unless pinned).

## Sync

Exchange `Heads`, request what is missing with `Want{space_id, author, from_seq, to_seq}`, answer in `Events` batches, `Done` ends the run.

`SpaceHeads{space_id, authors: [(author, max_seq, event_id)], invite}`. `event_id` is the ID of the author's event at `max_seq`; if it differs from the receiver's event with that `seq`, the receiver requests exactly that event to obtain proof of equivocation. `invite` (`Invite{create, add}`: the space creation and the receiver's `MemberAdd`, both signed by the creator) is attached for members of the space; it is the receiver's ticket into a space it does not know yet.

- Flow (both sides symmetric): the initiator sends `Heads`; the receiver answers with its own `Heads` and a `Want` (possibly empty); each side answers the other's `Want` with `Events`* and `Done` and then ends its sending direction. The initiator on `PeerUp` is the side with the smaller device ID; in addition every node reconciles with all peers every 60 s.
- Push: new events go as their own stream (`Events`* + `Done`, without `Heads`) to connected members. Whoever accepts up to 50 new events passes them on to its other peers (flooding with duplicate detection). A gap triggers a full reconciliation.
- `Heads` and `Events` only contain spaces the other side is a member of; a `Want` for other spaces is answered empty. Heads also list spaces the other side has left itself, without events, so it learns that its leave arrived and can delete the data. Batches stay below about 400 KiB (frame limit 1 MiB) and at most 5000 events; a sync stream may carry only one `Heads`.
- Admission (before the checks below): events are only accepted for spaces I am a member of, or for which I hold a ticket (`invite`) whose `MemberAdd` is not in the log yet; then only the creator's events up to the invitation. Spaces I have not accepted yet take at most 5000 foreign events; the rest follows after accepting. Per author and space at most 10,000 events per hour of receive time; more is rejected and requested again at the next reconciliation.
- Within a batch, events are applied in `seq` order per author (several passes, so creator -> invitation -> member events work in one batch). Rebuilds of the projection are debounced (2 s).
- Rate: incoming sync runs are limited to 4 at a time with 32 waiting (more are refused); reconciliation with the same device is coalesced and at most once per second.
- Budget: in a stream it opened, a peer may send at most 200 events unasked (push); beyond that only as many as I requested with my `Want`. More events, or a single event with a forged signature, end the stream at once, and the device is ignored for 5 minutes (no sync, no push accepted). Signatures are checked on a worker thread, not on the database thread; database work caused by peers runs behind own actions.
- Accepting an event: signature (strict) -> size/time (HLC at most 1 h in the future; a remote HLC moves the own clock forward at most to that limit, and a full counter carries over into `wall_ms`) -> `seq` must be exactly `head + 1` (larger = gap; smaller with a different event = equivocation: both event IDs are stored as evidence and the space shows a warning naming the author) -> the HLC must be strictly greater than that of the author's previous event (`seq - 1`) -> the author is a valid member at the time (HLC) of the event: the creator (`SpaceCreate`) or added by the creator via `MemberAdd` and not removed afterwards (`MemberRemove` by the creator or by themselves); otherwise `NotMember` or `Unauthorized`.
- `MemberRemove{user, last_seq}`: `last_seq` is the last `seq` of `user` that still counts (the creator sets the highest one it knows; on leaving it is the `seq` of the leave event itself, anything else is `Malformed`). Events of `user` with a larger `seq` are rejected regardless of their HLC, unless the creator added `user` again between the removal and the event. Events with `seq <= last_seq` are stored even if they arrive after the removal (the projection ignores them when their HLC is after it). If a node already holds events above `last_seq` when the removal arrives, it deletes them from the first invalid one on, lowers the head and rebuilds the space. `SpaceCreate` is only valid if `space_id = blake3("app/space/1" | author | hlc)[..16]` (no pre-claiming other people's spaces) and the space is new.
- Projection: events are applied in display order; permissions (admin for members/channels, author for editing, admin or author for deleting) are checked there. Events that are not allowed stay in the log but have no effect anywhere. If an event arrives before already known ones (smaller HLC), the space is rebuilt from the log; the result is therefore independent of arrival order.
- Consent: a `MemberAdd` makes me a member at once (the log needs it), but a space is only shown, notified and loaded automatically after I accept it. Own spaces and direct messages with friends count as accepted. Spaces I am no longer a member of are deleted with their log. Spaces whose creator I blocked stay hidden, and events of blocked users are not passed on.
- Known limits: a removed user no longer receives events (not even their own removal); events the creator had not seen when removing someone are lost; a user who kept writing after being removed cannot be added again with the same key (their `seq` no longer fits); roles other than `admin`/`member` do not exist yet.

## Hello (`app/hello/1`)

One bidirectional stream per connection, one frame per direction: `HelloMessage::{Request, Accept, Decline}` -> `HelloReply::{Received, RateLimited, Blocked, Invalid}`. Names at most 64 and notes at most 256 characters (`is_sane`). The request must arrive within 2 s; at most 32 hello connections are handled at the same time.

- `Request{cert, display_name, note, secret, binding, rotation}` and `Accept{cert, display_name, binding}` carry a `DeviceCert`; `binding` must tie `cert.device` to the node ID of the connection (the sender's rendezvous endpoint), otherwise `Invalid`. `Decline` has no content; the receiver matches it to an outgoing request via the rendezvous endpoint.
- `secret` is the invitation secret from the friend code. A new request from a stranger only counts with the secret of the current code (otherwise `Invalid`); friends (new device) and the answer to my own request do not need it. Generating a new code invalidates the old one. More than 100 unanswered requests push out the oldest.
- A `Request` from a blocked user is answered with `Received` like a delivered one (does not reveal the block) and is not stored. `Blocked` remains in the protocol but is no longer sent. Hello frames are at most 4 KiB. If both are pending at the same time, both become friends.
- `RateLimited`: token bucket per node ID (burst of 3, 1 per 10 s) and globally (burst of 20, 5/s). The sender retries later (backoff 5 s to 5 min).
- Delivery is at-least-once via a persistent outbox; all messages are idempotent.

## Key rotation

The user key is on every device, so a stolen device means a stolen identity. A second key, the **recovery key**, is never stored: the user writes it down (`CFR-…`, base32 with a 4-byte checksum). Only its commitment `blake3::derive_key("campfire 2026-11 recovery commitment", recovery_pub)` is stored and sent in the handshake; friends keep the first commitment they see per user.

If a device is lost, the user signs a `Rotation{old, new, recovery, next, signature}` with the recovery key over `"app/rotation/1" | old | new | next`. `next` is the commitment of the new recovery key, so the chain continues. The rotation travels in `Request.rotation` from the new identity (outbox kind `rotate` to every friend, and with every later request). A friend applies it if `rotation.new == cert.user`, the signature is valid, the stored commitment for `old` equals the commitment of `rotation.recovery`, `old` is a friend and `new` is not blocked. It then blocks `old`, takes over name and nickname for `new` (fingerprint confirmation is reset), stores `next` and answers with `Accept`. Spaces of the old key are gone for the user; they need to be invited again.

Not covered: revoking a single device without replacing the key (every device holds the user key).

## Presence

`ControlMessage::Presence{status, voice, streaming}` is sent on connect and on every change. The receiver treats `Invisible` like "offline"; without any presence received so far, the peer also counts as offline. If the connection drops (even abruptly, idle timeout 6 s), the peer is offline. With several devices of one user, the most recently reported visible state applies. Auto-away (default 10 min without input) only changes `Online` to `Away`.

## Friend code

Version 2. Binary `[version u8][flags u8][user 32][node_id 32][secret 12][relay_len u8, relay]?[check 4]`, base32 without padding, uppercase. `node_id` is the rendezvous endpoint. `secret` = first 12 bytes of `blake3::keyed_hash(invite_key, generation as u64 BE)`, where `invite_key = blake3::derive_key("campfire 2026-11 friend code invite secret", user_secret)` and the generation increases with every new code (`friends::invite_secret`); it is the invitation secret a request must carry. `check` = first 4 bytes of blake3 over everything before it (deviation from PLAN 6.2: typos are detected). Flag 1 = relay URL present (public `https` only). Input is insensitive to case, whitespace and `-`. Link: `app://add/<code>`. Codes of version 1 are rejected.

## Data at rest

The database lives in `%LOCALAPPDATA%\CampfireData\app.db` (moved there once from the roaming profile, so it does not roam to other machines); files in `%LOCALAPPDATA%\Campfire\blobs`. Secret keys are protected with DPAPI (bound to the Windows user); the database itself is not encrypted. Campfire opens the database in exclusive locking mode: while it runs, no other program can lock it or read it through SQLite. Message data stays below the storage limit (see "Storage limit and stubs"); files below the file storage limit (default 10 GB), least recently used first.
