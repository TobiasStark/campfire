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
| `app/hello/1` | `alpn::HELLO` | Friend requests, open to strangers (rate-limited) |

iroh-gossip and iroh-blobs use their own ALPNs. Files: ALPN `/iroh-bytes/4` (iroh-blobs 0.104); incoming connections arrive as `NetEvent::Blobs` and go to `app-blobs`; only what the `Authorizer` allows for the requesting device is served, push is off.

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

### Handshake

1. The client sends `Handshake{versions, cert, display_name, avatar_hash}`.
2. The server checks that `cert.device` matches the device ID of the connection, the certificate (`DeviceCert`: user signature over `"app/device-cert/1" | user | device | created_at(BE)`) and the allowlist, and replies with `HandshakeReply::Accept{version, handshake}` or `Reject{reason}`.
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

`EventBody{space_id, author, seq, hlc, kind}` is signed with the author's user key (bytes = postcard of the body). `event_id` = blake3 of the body bytes; the signature is not part of the ID. `seq` is gap-free per author and space, starting at 1. Display order: `Hlc` (wall_ms, counter), ties broken by `event_id`.

Kinds: `SpaceCreate`, `MemberAdd`, `MemberRemove`, `ChannelCreate`, `ChannelUpdate`, `ChannelDelete`, `Message`, `MessageEdit`, `MessageDelete`, `Reaction`, `ReactionRemove`, `FileShare`. `RoleGrant`/`RoleRevoke` follow later (append at the end).

## Sync

Exchange `Heads`, request what is missing with `Want{space_id, author, from_seq, to_seq}`, answer in `Events` batches, `Done` ends the run.

- Flow (both sides symmetric): the initiator sends `Heads`; the receiver answers with its own `Heads` and a `Want` (possibly empty); each side answers the other's `Want` with `Events`* and `Done` and then ends its sending direction. The initiator on `PeerUp` is the side with the smaller device ID; in addition every node reconciles with all peers every 60 s.
- Push: new events go as their own stream (`Events`* + `Done`, without `Heads`) to connected members. Whoever accepts up to 50 new events passes them on to its other peers (flooding with duplicate detection). A gap triggers a full reconciliation.
- `Heads` and `Events` only contain spaces the other side is a member of; a `Want` for other spaces is answered empty. Batches stay below about 400 KiB (frame limit 1 MiB).
- Accepting an event: signature (strict) -> size/time (HLC at most 3 h in the future) -> `seq` must be exactly `head + 1` (smaller = equivocation, larger = gap) -> the author is a valid member at the time (HLC) of the event: the creator (`SpaceCreate`) or added by the creator via `MemberAdd` and not removed afterwards (`MemberRemove` by the creator or by themselves); otherwise `NotMember` or `Unauthorized`. `SpaceCreate` is only valid if `space_id = blake3("app/space/1" | author | hlc)[..16]` (no pre-claiming other people's spaces) and the space is new.
- Projection: events are applied in display order; permissions (admin for members/channels, author for editing, admin or author for deleting) are checked there. Events that are not allowed stay in the log but have no effect anywhere. If an event arrives before already known ones (smaller HLC), the space is rebuilt from the log; the result is therefore independent of arrival order.
- Known limit: a removed user no longer receives events (not even their own removal); roles other than `admin`/`member` do not exist yet.

## Hello (`app/hello/1`)

One bidirectional stream per connection, one frame per direction: `HelloMessage::{Request, Accept, Decline}` -> `HelloReply::{Received, RateLimited, Blocked, Invalid}`. Names at most 64 and notes at most 256 characters (`is_sane`).

- `Request` and `Accept` carry a `DeviceCert`; `cert.device` must match the node ID of the connection, otherwise `Invalid`. `Decline` has no content; the receiver matches it to an outgoing request via the node ID.
- A `Request` from a blocked user is answered with `Received` like a delivered one (does not reveal the block) and is not stored. `Blocked` remains in the protocol but is no longer sent. Hello frames are at most 4 KiB. If both are pending at the same time, both become friends.
- `RateLimited`: token bucket per node ID (burst of 3, 1 per 10 s) and globally (burst of 20, 5/s); more than 100 unanswered requests are answered the same way. The sender retries later (backoff 5 s to 5 min).
- Delivery is at-least-once via a persistent outbox; all messages are idempotent.

## Presence

`ControlMessage::Presence{status, voice, streaming}` is sent on connect and on every change. The receiver treats `Invisible` like "offline"; without any presence received so far, the peer also counts as offline. If the connection drops (even abruptly, idle timeout 6 s), the peer is offline. With several devices of one user, the most recently reported visible state applies. Auto-away (default 10 min without input) only changes `Online` to `Away`.

## Friend code

Binary `[version u8][flags u8][user 32][node_id 32][relay_len u8, relay]?[check 4]`, base32 without padding, uppercase. `check` = first 4 bytes of blake3 over everything before it (deviation from PLAN 6.2: typos are detected). Flag 1 = relay URL present. Input is insensitive to case, whitespace and `-`. Link: `app://add/<code>`. Contains no secret data.
