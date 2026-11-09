use crate::control::*;
use crate::event::*;
use crate::friend_code::*;
use crate::hello::*;
use crate::sync::*;
use crate::video::{LayerInfo, ReceiverReport, RefuseReason, VideoCodec, VideoMsg};
use crate::voice::*;
use crate::*;
use serde::{de::DeserializeOwned, Serialize};
use std::fmt::Debug;

fn roundtrip<T: Serialize + DeserializeOwned + PartialEq + Debug>(v: T) {
    let bytes = encode(&v).unwrap();
    assert_eq!(decode::<T>(&bytes).unwrap(), v);
    // Deterministisch (wichtig fuer Signaturen).
    assert_eq!(bytes, encode(&v).unwrap());
    // Abgeschnittene Eingabe darf nie panicken.
    for n in 0..bytes.len() {
        let _ = decode::<T>(&bytes[..n]);
    }
}

fn cert() -> DeviceCert {
    DeviceCert {
        user: [1; 32],
        device: [2; 32],
        created_at: 1_700_000_000,
        signature: Signature([9; 64]),
    }
}

fn body(kind: EventKind) -> EventBody {
    EventBody {
        space_id: [3; 16],
        author: [4; 32],
        seq: 7,
        hlc: Hlc {
            wall_ms: 123,
            counter: 2,
        },
        kind,
    }
}

#[test]
fn control_roundtrip() {
    let hs = Handshake {
        versions: vec![1, 2],
        cert: cert(),
        display_name: "Tobi \u{1F525} \u{00E4}".into(),
        avatar_hash: Some([5; 32]),
    };
    roundtrip(hs.clone());
    roundtrip(HandshakeReply::Accept {
        version: 1,
        handshake: Box::new(hs),
    });
    for reason in [
        RejectReason::UnknownUser,
        RejectReason::InvalidCert,
        RejectReason::VersionMismatch,
        RejectReason::Blocked,
    ] {
        roundtrip(HandshakeReply::Reject { reason });
    }
    let msgs = vec![
        ControlMessage::Presence(Presence {
            status: PresenceStatus::Away,
            voice: Some(([1; 16], [2; 16])),
            streaming: true,
        }),
        ControlMessage::Presence(Presence {
            status: PresenceStatus::Invisible,
            voice: None,
            streaming: false,
        }),
        ControlMessage::Ping(42),
        ControlMessage::Pong(u64::MAX),
        ControlMessage::Voice(VoiceSignal::Join {
            space_id: [1; 16],
            channel: [2; 16],
            stream_id: 3,
        }),
        ControlMessage::Voice(VoiceSignal::Leave {
            space_id: [1; 16],
            channel: [2; 16],
        }),
        ControlMessage::Voice(VoiceSignal::State {
            muted: true,
            deafened: false,
        }),
        ControlMessage::Voice(VoiceSignal::CallInvite { call_id: [7; 16] }),
        ControlMessage::Voice(VoiceSignal::CallAccept { call_id: [7; 16] }),
        ControlMessage::Voice(VoiceSignal::CallDecline {
            call_id: [7; 16],
            busy: true,
        }),
        ControlMessage::Voice(VoiceSignal::CallCancel { call_id: [7; 16] }),
        ControlMessage::Subscribe {
            stream_id: 1,
            layer: 0,
        },
        ControlMessage::Unsubscribe { stream_id: 1 },
        ControlMessage::RequestKeyframe { stream_id: 1 },
        ControlMessage::Video(VideoMsg::Announce {
            stream_id: 2,
            layers: vec![LayerInfo {
                layer: 0,
                codec: VideoCodec::Hevc,
                width: 1920,
                height: 1080,
                fps: 60,
                kbps: 12_000,
            }],
            audio: true,
        }),
        ControlMessage::Video(VideoMsg::Stop { stream_id: 2 }),
        ControlMessage::Video(VideoMsg::Subscribe {
            stream_id: 2,
            layer: 0,
            decoders: vec![VideoCodec::H264, VideoCodec::Av1],
        }),
        ControlMessage::Video(VideoMsg::Unsubscribe { stream_id: 2 }),
        ControlMessage::Video(VideoMsg::KeyframeRequest { stream_id: 2 }),
        ControlMessage::Video(VideoMsg::Report(ReceiverReport {
            stream_id: 2,
            interval_ms: 1000,
            received: 58,
            lost: 2,
            late: 1,
            jitter_ms: 4,
            kbps: 11_500,
        })),
        ControlMessage::Video(VideoMsg::Refused {
            stream_id: 2,
            reason: RefuseReason::Capacity,
        }),
    ];
    for m in msgs {
        roundtrip(m);
    }
}

#[test]
fn event_roundtrip_all_kinds() {
    let kinds = vec![
        EventKind::SpaceCreate {
            name: "Zocker".into(),
        },
        EventKind::MemberAdd { user: [6; 32] },
        EventKind::MemberRemove { user: [6; 32] },
        EventKind::ChannelCreate {
            channel: [8; 16],
            kind: ChannelKind::Voice,
            name: "Lobby".into(),
            position: 1,
        },
        EventKind::ChannelUpdate {
            channel: [8; 16],
            name: Some("x".into()),
            position: None,
        },
        EventKind::ChannelDelete { channel: [8; 16] },
        EventKind::Message {
            channel: [8; 16],
            body: "Hallo \u{1F44B}".into(),
            reply_to: Some([1; 32]),
        },
        EventKind::MessageEdit {
            target: [1; 32],
            body: "neu".into(),
        },
        EventKind::MessageDelete { target: [1; 32] },
        EventKind::Reaction {
            target: [1; 32],
            emoji: "\u{1F44D}".into(),
        },
        EventKind::ReactionRemove {
            target: [1; 32],
            emoji: "\u{1F44D}".into(),
        },
        EventKind::FileShare {
            channel: [8; 16],
            blob_hash: [2; 32],
            name: "a.png".into(),
            size: 10,
            mime: "image/png".into(),
            thumb_hash: None,
        },
    ];
    for k in kinds {
        let b = body(k);
        roundtrip(b.clone());
        roundtrip(EventEnvelope {
            body: b,
            signature: Signature([7; 64]),
        });
    }
}

#[test]
fn event_id_depends_on_body_only() {
    let a = body(EventKind::SpaceCreate { name: "a".into() });
    let mut b = a.clone();
    assert_eq!(a.id(), b.id());
    b.seq += 1;
    assert_ne!(a.id(), b.id());
    let e1 = EventEnvelope {
        body: a.clone(),
        signature: Signature([1; 64]),
    };
    let e2 = EventEnvelope {
        body: a,
        signature: Signature([2; 64]),
    };
    assert_eq!(e1.id(), e2.id());
}

#[test]
fn display_order_hlc_then_id() {
    let h1 = Hlc {
        wall_ms: 1,
        counter: 0,
    };
    let h2 = Hlc {
        wall_ms: 1,
        counter: 1,
    };
    assert!(display_order((&h1, &[9; 32]), (&h2, &[0; 32])).is_lt());
    assert!(display_order((&h1, &[1; 32]), (&h1, &[2; 32])).is_lt());
}

#[test]
fn sync_and_hello_roundtrip() {
    roundtrip(SyncMessage::Heads(vec![SpaceHeads {
        space_id: [1; 16],
        authors: vec![([2; 32], 5), ([3; 32], 9)],
    }]));
    roundtrip(SyncMessage::Want(vec![Want {
        space_id: [1; 16],
        author: [2; 32],
        from_seq: 3,
        to_seq: 8,
    }]));
    roundtrip(SyncMessage::Events(vec![EventEnvelope {
        body: body(EventKind::ChannelDelete { channel: [1; 16] }),
        signature: Signature([1; 64]),
    }]));
    roundtrip(SyncMessage::Done);
    roundtrip(HelloMessage::Request {
        cert: cert(),
        display_name: "A".into(),
        note: "hi".into(),
    });
    roundtrip(HelloMessage::Accept {
        cert: cert(),
        display_name: "B".into(),
    });
    roundtrip(HelloMessage::Decline);
    for r in [
        HelloReply::Received,
        HelloReply::RateLimited,
        HelloReply::Blocked,
        HelloReply::Invalid,
    ] {
        roundtrip(r);
    }
}

#[test]
fn hello_sanity_limits() {
    let long = "x".repeat(MAX_NAME_LEN + 1);
    assert!(!HelloMessage::Request {
        cert: cert(),
        display_name: long,
        note: String::new()
    }
    .is_sane());
    assert!(HelloMessage::Decline.is_sane());
}

#[test]
fn decode_rejects_trailing_and_oversize() {
    let mut b = encode(&ControlMessage::Ping(1)).unwrap();
    b.push(0);
    assert!(matches!(
        decode::<ControlMessage>(&b),
        Err(WireError::TrailingBytes)
    ));
    assert!(matches!(
        decode::<ControlMessage>(&vec![0; MAX_MESSAGE_LEN + 1]),
        Err(WireError::TooLarge(_))
    ));
    assert!(decode::<ControlMessage>(&[0xFF, 0xFF, 0xFF]).is_err());
}

#[test]
fn voice_header_roundtrip() {
    let h = VoiceHeader {
        flags: 1,
        stream_id: 7,
        seq: 0xDEADBEEF,
        ts: 960,
    };
    let pkt = h.write(&[1, 2, 3]);
    assert_eq!(pkt.len(), HEADER_LEN + 3);
    let (h2, payload) = VoiceHeader::parse(&pkt).unwrap();
    assert_eq!(h, h2);
    assert_eq!(payload, &[1, 2, 3]);
    assert!(VoiceHeader::parse(&pkt[..HEADER_LEN - 1]).is_none());
}

#[test]
fn alpns_are_versioned_and_distinct() {
    assert_eq!(alpn::PEER, b"app/peer/1");
    assert_eq!(alpn::HELLO, b"app/hello/1");
    assert_ne!(alpn::PEER, alpn::HELLO);
}

fn code(relay: Option<&str>) -> FriendCode {
    FriendCode {
        user: [0xAB; 32],
        node_id: [0xCD; 32],
        relay_url: relay.map(Into::into),
    }
}

#[test]
fn friend_code_roundtrip() {
    for c in [
        code(None),
        code(Some("https://relay.example.com/")),
        code(Some("")),
    ] {
        let s = c.encode().unwrap();
        assert_eq!(FriendCode::decode(&s).unwrap(), c);
        assert_eq!(FriendCode::decode(&s.to_lowercase()).unwrap(), c);
        assert_eq!(FriendCode::decode(&c.to_link().unwrap()).unwrap(), c);
        let spaced: String = s
            .chars()
            .enumerate()
            .map(|(i, ch)| {
                if i % 5 == 4 {
                    format!("{ch}-")
                } else {
                    ch.to_string()
                }
            })
            .collect();
        assert_eq!(FriendCode::decode(&format!("  {spaced}\n")).unwrap(), c);
    }
}

#[test]
fn friend_code_rejects_invalid() {
    assert_eq!(FriendCode::decode(""), Err(FriendCodeError::TooShort));
    assert_eq!(FriendCode::decode("ABC"), Err(FriendCodeError::TooShort));
    assert_eq!(
        FriendCode::decode("!!!!!!!!!!!!"),
        Err(FriendCodeError::BadEncoding)
    );
    assert_eq!(
        FriendCode::decode("AAAAAAAAAA"),
        Err(FriendCodeError::TooShort)
    );

    let s = code(None).encode().unwrap();
    // Jedes einzelne veraenderte Zeichen wird erkannt.
    for i in 0..s.len() {
        let mut chars: Vec<char> = s.chars().collect();
        chars[i] = if chars[i] == 'A' { 'B' } else { 'A' };
        let t: String = chars.into_iter().collect();
        assert!(FriendCode::decode(&t).is_err(), "Position {i}");
    }
    // Abgeschnitten.
    assert!(FriendCode::decode(&s[..s.len() - 3]).is_err());
    // Falsche Version mit gueltiger Pruefsumme.
    let mut raw = vec![2u8, 0];
    raw.extend_from_slice(&[0; 64]);
    let c = blake3::hash(&raw).as_bytes()[..4].to_vec();
    raw.extend_from_slice(&c);
    assert_eq!(
        FriendCode::decode(&data_encoding::BASE32_NOPAD.encode(&raw)),
        Err(FriendCodeError::UnsupportedVersion(2))
    );
    // Relay zu lang.
    assert_eq!(
        code(Some(&"x".repeat(256))).encode(),
        Err(FriendCodeError::RelayTooLong)
    );
}

#[test]
fn friend_code_never_panics_on_garbage() {
    let mut x: u32 = 12345;
    for len in 0..200 {
        let s: String = (0..len)
            .map(|_| {
                x = x.wrapping_mul(1664525).wrapping_add(1013904223);
                b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567-!"[(x >> 24) as usize % 34] as char
            })
            .collect();
        let _ = FriendCode::decode(&s);
    }
}

#[test]
fn video_frame_header_roundtrip_and_limits() {
    use crate::video::{FrameHeader, FLAG_KEY, HEADER_LEN, MAX_FRAME_LEN};
    let h = FrameHeader {
        stream_id: 3,
        layer: 1,
        flags: FLAG_KEY,
        codec: VideoCodec::Av1,
        frame_no: 0x0102_0304,
        ts: 99_000,
        len: 1234,
    };
    let bytes = h.write();
    assert_eq!(bytes.len(), HEADER_LEN);
    assert_eq!(FrameHeader::parse(&bytes), Some(h));
    assert!(h.is_key());
    // zu kurz, unbekannter Codec, zu grosse Laenge
    assert_eq!(FrameHeader::parse(&bytes[..HEADER_LEN - 1]), None);
    let mut bad = bytes;
    bad[3] = 9;
    assert_eq!(FrameHeader::parse(&bad), None);
    let mut big = bytes;
    big[12..16].copy_from_slice(&((MAX_FRAME_LEN as u32) + 1).to_be_bytes());
    assert_eq!(FrameHeader::parse(&big), None);
}

#[test]
fn old_control_variants_keep_their_wire_index() {
    // Neue Varianten stehen am Ende; alte Clients muessen alte Nachrichten weiter lesen.
    let old = encode(&ControlMessage::RequestKeyframe { stream_id: 1 }).unwrap();
    let new = encode(&ControlMessage::Video(VideoMsg::Stop { stream_id: 1 })).unwrap();
    assert!(new[0] > old[0]);
}
