use super::*;

fn bytes(frame: &Frame) -> Vec<u8> {
    let mut out = Vec::new();
    frame.encode(&mut out).unwrap();
    out
}

fn round_trip(message: Message) {
    let frame = Frame::numbered(&message, 3).unwrap();
    let mut decoder = Decoder::new();
    decoder.push(&bytes(&frame));
    let back = decoder.next_frame().unwrap().unwrap();
    assert_eq!(back.kind, message.kind());
    assert_eq!(back.message(), Ok(Some(message)));
}

fn welcome() -> Welcome {
    Welcome {
        version: 0,
        udp_port: 0,
        tcp_port: 0,
        stream_key: [0; STREAM_KEY_LEN],
    }
}

#[test]
fn header_layout_matches_the_spec() {
    let frame = Frame {
        kind: Kind(0x0201),
        request: 7,
        payload: vec![0xAA, 0xBB],
    };
    assert_eq!(
        bytes(&frame),
        [2, 0, 0, 0, 0x01, 0x02, 0, 0, 7, 0, 0, 0, 0xAA, 0xBB]
    );
}

#[test]
fn a_frame_survives_the_round_trip() {
    let frame = Frame::event(&Message::Hello(Hello {
        version: VERSION,
        token: b"secret".to_vec(),
    }))
    .unwrap();
    let mut decoder = Decoder::new();
    decoder.push(&bytes(&frame));
    assert_eq!(decoder.next_frame(), Ok(Some(frame)));
    assert_eq!(decoder.next_frame(), Ok(None));
}

#[test]
fn frames_arriving_byte_by_byte_come_out_whole() {
    let frame = Frame::event(&Message::Reject(Reject {
        reason: "wrong token".into(),
    }))
    .unwrap();
    let mut decoder = Decoder::new();
    let wire = bytes(&frame);
    for (i, byte) in wire.iter().enumerate() {
        decoder.push(&[*byte]);
        let got = decoder.next_frame().unwrap();
        if i + 1 < wire.len() {
            assert_eq!(got, None);
        } else {
            assert_eq!(got, Some(frame.clone()));
        }
    }
}

#[test]
fn two_frames_in_one_read_come_out_in_order() {
    let first = Frame::event(&Message::Welcome(welcome())).unwrap();
    let second = Frame {
        kind: Kind(0x0302),
        request: 9,
        payload: Vec::new(),
    };
    let mut wire = bytes(&first);
    wire.extend(bytes(&second));
    let mut decoder = Decoder::new();
    decoder.push(&wire);
    assert_eq!(decoder.next_frame(), Ok(Some(first)));
    assert_eq!(decoder.next_frame(), Ok(Some(second)));
    assert_eq!(decoder.next_frame(), Ok(None));
}

#[test]
fn many_small_frames_keep_the_buffer_small() {
    let frame = Frame::event(&Message::PeerInfo {
        address: Ipv4Addr::new(198, 18, 0, 2),
    })
    .unwrap();
    let wire = bytes(&frame);
    let mut decoder = Decoder::new();
    for _ in 0..10_000 {
        decoder.push(&wire);
        decoder.push(&wire[..5]);
        assert_eq!(decoder.next_frame(), Ok(Some(frame.clone())));
        decoder.push(&wire[5..]);
        assert_eq!(decoder.next_frame(), Ok(Some(frame.clone())));
        assert_eq!(decoder.next_frame(), Ok(None));
    }
    assert!(decoder.buf.capacity() < 64 * 1024);
}

#[test]
fn an_oversized_length_is_refused_before_the_payload_arrives() {
    let mut header = ((MAX_PAYLOAD + 1) as u32).to_le_bytes().to_vec();
    header.extend_from_slice(&[1, 0, 0, 0, 0, 0, 0, 0]);
    let mut decoder = Decoder::new();
    decoder.push(&header);
    assert_eq!(decoder.next_frame(), Err(Error::TooLarge(MAX_PAYLOAD + 1)));
}

#[test]
fn an_oversized_payload_is_not_encoded() {
    let frame = Frame {
        kind: Kind(0x0200),
        request: 1,
        payload: vec![0; MAX_PAYLOAD + 1],
    };
    assert_eq!(
        frame.encode(&mut Vec::new()),
        Err(Error::TooLarge(MAX_PAYLOAD + 1))
    );
}

#[test]
fn flags_are_ignored() {
    let welcome = Frame::event(&Message::Welcome(welcome())).unwrap();
    let mut wire = bytes(&welcome);
    wire[6] = 0xFF;
    let mut decoder = Decoder::new();
    decoder.push(&wire);
    assert_eq!(decoder.next_frame(), Ok(Some(welcome)));
}

#[test]
fn every_message_survives_the_round_trip() {
    let address = Ipv4Addr::new(198, 18, 4, 20);
    let pairs = vec![
        ("Content-Type".to_owned(), "application/json".to_owned()),
        ("X-Empty".to_owned(), String::new()),
    ];
    for message in [
        Message::Hello(Hello {
            version: 0,
            token: vec![1, 2, 3],
        }),
        Message::Welcome(Welcome {
            version: 0,
            udp_port: 50_001,
            tcp_port: 50_002,
            stream_key: [7; STREAM_KEY_LEN],
        }),
        Message::Reject(Reject {
            reason: "token unknown".into(),
        }),
        Message::Unsupported,
        Message::State(State {
            signed_in: true,
            account_id: 41,
            handle: "Alec".into(),
            nat_type: 2,
            address,
        }),
        Message::State(State::signed_out()),
        Message::Feature {
            user_id: 0x1000_0000,
            feature: 1,
        },
        Message::WebRequest(WebRequest {
            user_id: 0x1000_0000,
            api_group: "group".into(),
            path: "/v1/things?limit=5".into(),
            method: "POST".into(),
            headers: pairs.clone(),
            body: b"{\"a\":1}".to_vec(),
        }),
        Message::WebReply(WebReply {
            status: 201,
            headers: pairs.clone(),
            body: vec![0, 255, 7],
        }),
        Message::WebReply(WebReply::not_offered()),
        Message::PushFilter(PushFilter {
            user_id: 1,
            filter: 2,
            data_types: vec!["a:b:c".into(), "d".into()],
        }),
        Message::PushFilterGone {
            user_id: 1,
            filter: 2,
        },
        Message::PushEvent(PushEvent {
            user_id: 1,
            filter: 2,
            data_type: "a:b:c".into(),
            from: 41,
            to: 42,
            data: b"{}".to_vec(),
            extended: pairs.clone(),
        }),
        Message::Activate(Activate {
            user_id: 1,
            session: "00000000-0000-4000-8000-000000000000".into(),
            session_kind: SessionKind::Player,
        }),
        Message::Activated {
            result: ActivateResult::UnknownSession,
        },
        Message::Deactivate {
            user_id: 1,
            session: "s".into(),
        },
        Message::Peer(Peer {
            session: "s".into(),
            account_id: 42,
            handle: "Lumen".into(),
            address,
            peer_state: PeerState::Connected,
        }),
        Message::PeerInfo { address },
        Message::PeerInfoReply(PeerInfoReply {
            known: true,
            rtt_us: 31_000,
            relayed: false,
        }),
        Message::Bind(Bind {
            vport: 3658,
            transport: Transport::Datagram,
            loopback_port: 61_000,
        }),
        Message::Bound {
            result: BindResult::InUse,
        },
        Message::Unbind {
            vport: 3658,
            transport: Transport::Stream,
        },
        Message::Intent(Intent {
            user_id: -1,
            intent_type: "joinSession".into(),
            properties: pairs.clone(),
        }),
        Message::InvitationDialog(InvitationDialog {
            user_id: 1,
            session: "s".into(),
            max: 3,
        }),
        Message::InvitationDialogReply {
            result: DialogResult::Sent,
            sent: 2,
        },
    ] {
        round_trip(message);
    }
}

#[test]
fn addresses_are_little_endian_numbers() {
    let payload = Message::PeerInfo {
        address: Ipv4Addr::new(198, 18, 0, 1),
    }
    .payload()
    .unwrap();
    assert_eq!(payload, [0x01, 0x00, 0x12, 0xC6]);
}

#[test]
fn strings_are_counted_utf8_without_a_terminator() {
    let payload = Message::Deactivate {
        user_id: 1,
        session: "äb".into(),
    }
    .payload()
    .unwrap();
    assert_eq!(payload, [1, 0, 0, 0, 3, 0, 0xC3, 0xA4, b'b']);
}

#[test]
fn extra_bytes_after_known_fields_are_ignored() {
    let mut payload = Message::PeerInfo {
        address: Ipv4Addr::new(198, 18, 0, 9),
    }
    .payload()
    .unwrap();
    payload.extend_from_slice(b"a field from a newer version");
    assert_eq!(
        Message::parse(Kind::PEER_INFO, &payload),
        Ok(Some(Message::PeerInfo {
            address: Ipv4Addr::new(198, 18, 0, 9)
        }))
    );
}

#[test]
fn short_or_broken_payloads_are_errors() {
    assert_eq!(Message::parse(Kind::HELLO, &[1]), Err(Error::Truncated));
    assert_eq!(
        Message::parse(Kind::WELCOME, &[0, 0]),
        Err(Error::Truncated)
    );
    assert_eq!(
        Message::parse(Kind::DEACTIVATE, &[1, 0, 0, 0, 9, 0, b'a']),
        Err(Error::Truncated)
    );
    assert_eq!(
        Message::parse(Kind::DEACTIVATE, &[1, 0, 0, 0, 1, 0, 0xFF]),
        Err(Error::NotText)
    );
    let mut many = vec![0xFF, 0xFF];
    many.extend_from_slice(&[0; 3]);
    assert_eq!(
        Message::parse(Kind::PUSH_FILTER, &[&[0; 8][..], &many].concat()),
        Err(Error::Truncated)
    );
}

#[test]
fn unknown_kinds_and_values_are_tolerated() {
    assert_eq!(Message::parse(Kind(0x0777), &[1, 2, 3]), Ok(None));
    assert_eq!(
        Message::parse(Kind::BIND.reply(), &[9]),
        Ok(Some(Message::Bound {
            result: BindResult::Unknown
        }))
    );
    let request = Frame {
        kind: Kind(0x0777),
        request: 12,
        payload: Vec::new(),
    };
    let reply = request.unsupported();
    assert_eq!(reply.kind, Kind::UNSUPPORTED);
    assert_eq!(reply.request, 12);
}

#[test]
fn replies_carry_the_top_bit() {
    assert_eq!(Kind::WEB_REQUEST.reply(), Kind(0x8201));
    assert!(Kind(0x8201).is_reply());
    assert!(!Kind::WEB_REQUEST.is_reply());
    assert_eq!(
        Message::WebReply(WebReply::not_offered()).kind(),
        Kind(0x8201)
    );
}

#[test]
fn a_string_too_long_for_its_count_is_not_encoded() {
    let message = Message::Deactivate {
        user_id: 1,
        session: "x".repeat(70_000),
    };
    assert_eq!(message.payload(), Err(Error::TooLong));
}

#[test]
fn traffic_headers_split_from_the_game_bytes() {
    let header = TrafficHeader {
        address: Ipv4Addr::new(198, 18, 0, 3),
        vport: 3658,
        source_vport: 1,
    };
    let mut datagram = header.encode().to_vec();
    assert_eq!(datagram, [0x03, 0x00, 0x12, 0xC6, 0x4A, 0x0E, 0x01, 0x00]);
    datagram.extend_from_slice(b"game");
    assert_eq!(
        TrafficHeader::split(&datagram),
        Some((header, &b"game"[..]))
    );
    assert_eq!(TrafficHeader::split(&datagram[..7]), None);
}
