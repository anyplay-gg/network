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
        nonce: vec![7; NONCE_LEN],
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
fn decoder_with_custom_max_payload_refuses_oversized_length() {
    let mut header = ((MAX_HELLO_PAYLOAD + 1) as u32).to_le_bytes().to_vec();
    header.extend_from_slice(&[1, 0, 0, 0, 0, 0, 0, 0]);
    let mut decoder = Decoder::with_max_payload(MAX_HELLO_PAYLOAD);
    decoder.push(&header);
    assert_eq!(
        decoder.next_frame(),
        Err(Error::TooLarge(MAX_HELLO_PAYLOAD + 1))
    );
}

#[test]
fn decoder_with_custom_max_payload_accepts_exact_boundary() {
    let mut wire = (MAX_HELLO_PAYLOAD as u32).to_le_bytes().to_vec();
    wire.extend_from_slice(&[1, 0, 0, 0, 0, 0, 0, 0]);
    wire.extend(vec![0xAA; MAX_HELLO_PAYLOAD]);
    let mut decoder = Decoder::with_max_payload(MAX_HELLO_PAYLOAD);
    decoder.push(&wire);
    let frame = decoder.next_frame().unwrap().unwrap();
    assert_eq!(frame.payload.len(), MAX_HELLO_PAYLOAD);
    assert_eq!(frame.payload, vec![0xAA; MAX_HELLO_PAYLOAD]);
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

/// One message of every kind, with fields that are not all zero.
fn samples() -> Vec<Message> {
    let address = Ipv4Addr::new(198, 18, 4, 20);
    let pairs = vec![
        ("Content-Type".to_owned(), "application/json".to_owned()),
        ("X-Empty".to_owned(), String::new()),
    ];
    vec![
        Message::Hello(Hello {
            version: 1,
            nonce: vec![1; NONCE_LEN],
        }),
        Message::Challenge(Challenge {
            version: 1,
            nonce: [2; NONCE_LEN],
            proof: [3; PROOF_LEN],
        }),
        Message::Auth(Auth {
            proof: [4; PROOF_LEN],
        }),
        Message::Welcome(Welcome {
            version: 1,
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
        Message::ShareOpen(ShareOpen {
            ring: "anyplay-ring-9f3a".into(),
            ring_bytes: 1_920 * 1_080 * 4 * 3 + 1_000,
            width: 1_920,
            height: 1_080,
            max_fps: 60,
            mode: ShareMode::Together,
        }),
        Message::ShareOpened {
            result: ShareResult::NotPresenting,
        },
        Message::ShareClose,
        Message::ShareMode {
            mode: ShareMode::HandOver,
        },
        Message::SharePad(SharePad {
            slot: 1,
            sequence: 65_535,
            buttons: 0x0000_7FFF,
            lx: 0,
            ly: 255,
            rx: 128,
            ry: 1,
            l2: 200,
            r2: 7,
        }),
        Message::Override {
            action: OverrideAction::Stop,
        },
        Message::ShareStatus(ShareStatus {
            frames: 3_600,
            dropped: 12,
            audio_underruns: 1,
        }),
        Message::ScoreRecord(ScoreRecord {
            user_id: -1,
            board: "weekly-1".into(),
            score: -9_007_199_254_740_991,
        }),
        Message::ScoreRecorded(ScoreRecorded {
            result: ScoreResult::Stored,
            score: 9_007_199_254_740_991,
            rank: 17,
            total: 4_021,
        }),
        Message::BoardRead(BoardRead {
            user_id: 0,
            board: "7".into(),
            view: BoardView::Around,
            start: 0,
            count: 11,
        }),
        Message::BoardPage(BoardPage {
            result: BoardResult::Done,
            total: 3,
            me_rank: 2,
            me_score: 450,
            entries: vec![
                BoardRow {
                    rank: 1,
                    account_id: 9,
                    handle: "Lumen".into(),
                    score: 900,
                    achieved: 1_790_000_000,
                },
                BoardRow {
                    rank: 2,
                    account_id: 4,
                    handle: "Ember".into(),
                    score: 450,
                    achieved: 1_790_000_100,
                },
            ],
        }),
        Message::BoardPage(BoardPage {
            result: BoardResult::UnknownBoard,
            total: 0,
            me_rank: 0,
            me_score: 0,
            entries: Vec::new(),
        }),
    ]
}

#[test]
fn leaderboard_kinds_are_the_ones_in_the_spec() {
    let kinds: Vec<u16> = [
        Kind::SCORE_RECORD,
        Kind::SCORE_RECORD.reply(),
        Kind::BOARD_READ,
        Kind::BOARD_READ.reply(),
    ]
    .iter()
    .map(|k| k.0)
    .collect();
    assert_eq!(kinds, [0x0601, 0x8601, 0x0602, 0x8602]);
}

#[test]
fn leaderboard_payloads_have_the_layout_of_the_spec() {
    let record = Message::ScoreRecord(ScoreRecord {
        user_id: 1,
        board: "b".into(),
        score: -2,
    });
    assert_eq!(
        record.payload().unwrap(),
        [
            1, 0, 0, 0, // user_id
            1, 0, b'b', // board: str
            0xFE, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, // score: i64
        ]
    );
    let recorded = Message::ScoreRecorded(ScoreRecorded {
        result: ScoreResult::Kept,
        score: 0x0102,
        rank: 3,
        total: 4,
    });
    assert_eq!(
        recorded.payload().unwrap(),
        [
            1, // result
            2, 1, 0, 0, 0, 0, 0, 0, // score
            3, 0, 0, 0, // rank
            4, 0, 0, 0, // total
        ]
    );
    let read = Message::BoardRead(BoardRead {
        user_id: 0,
        board: "x".into(),
        view: BoardView::Friends,
        start: 5,
        count: 25,
    });
    assert_eq!(
        read.payload().unwrap(),
        [
            0, 0, 0, 0, // user_id
            1, 0, b'x', // board
            3,    // view
            5, 0, 0, 0, // start
            25, 0, // count
        ]
    );
    let page = Message::BoardPage(BoardPage {
        result: BoardResult::Done,
        total: 9,
        me_rank: 2,
        me_score: 7,
        entries: vec![BoardRow {
            rank: 1,
            account_id: 0x0102,
            handle: "ab".into(),
            score: 5,
            achieved: 6,
        }],
    });
    assert_eq!(
        page.payload().unwrap(),
        [
            0, // result
            9, 0, 0, 0, // total
            2, 0, 0, 0, // me_rank
            7, 0, 0, 0, 0, 0, 0, 0, // me_score
            1, 0, // entry count
            1, 0, 0, 0, // rank
            2, 1, 0, 0, 0, 0, 0, 0, // account_id
            2, 0, b'a', b'b', // handle
            5, 0, 0, 0, 0, 0, 0, 0, // score
            6, 0, 0, 0, 0, 0, 0, 0, // achieved
        ]
    );
}

#[test]
fn a_board_page_cut_short_is_truncated_and_a_huge_count_costs_nothing() {
    let page = Message::BoardPage(BoardPage {
        result: BoardResult::Done,
        total: 1,
        me_rank: 0,
        me_score: 0,
        entries: vec![BoardRow {
            rank: 1,
            account_id: 1,
            handle: "ab".into(),
            score: 1,
            achieved: 1,
        }],
    });
    let payload = page.payload().unwrap();
    for cut in 0..payload.len() {
        assert_eq!(
            Message::parse(Kind::BOARD_READ.reply(), &payload[..cut]),
            Err(Error::Truncated),
            "cut at {cut}"
        );
    }
    // A count of 65,535 rows with none behind it is refused without a large allocation.
    let mut lying = payload[..17].to_vec();
    lying.extend_from_slice(&u16::MAX.to_le_bytes());
    assert_eq!(
        Message::parse(Kind::BOARD_READ.reply(), &lying),
        Err(Error::Truncated)
    );
}

#[test]
fn unknown_leaderboard_values_are_tolerated() {
    let Ok(Some(Message::ScoreRecorded(m))) = Message::parse(
        Kind::SCORE_RECORD.reply(),
        &[77, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    ) else {
        panic!("not read");
    };
    assert_eq!(m.result, ScoreResult::Unknown);
    let Ok(Some(Message::BoardRead(m))) =
        Message::parse(Kind::BOARD_READ, &[0, 0, 0, 0, 0, 0, 9, 0, 0, 0, 0, 1, 0])
    else {
        panic!("not read");
    };
    assert_eq!(m.view, BoardView::Unknown);
}

#[test]
fn share_play_kinds_are_the_ones_in_the_spec() {
    let kinds: Vec<u16> = [
        Kind::SHARE_OPEN,
        Kind::SHARE_OPEN.reply(),
        Kind::SHARE_CLOSE,
        Kind::SHARE_MODE,
        Kind::SHARE_PAD,
        Kind::OVERRIDE,
        Kind::SHARE_STATUS,
    ]
    .iter()
    .map(|k| k.0)
    .collect();
    assert_eq!(
        kinds,
        [0x0501, 0x8501, 0x0502, 0x0503, 0x0504, 0x0505, 0x0506]
    );
}

#[test]
fn share_play_payloads_have_the_layout_of_the_spec() {
    let open = Message::ShareOpen(ShareOpen {
        ring: "r1".into(),
        ring_bytes: 0x0102_0304,
        width: 1_280,
        height: 720,
        max_fps: 60,
        mode: ShareMode::HandOver,
    });
    assert_eq!(
        open.payload().unwrap(),
        [
            2, 0, b'r', b'1', // ring: str
            4, 3, 2, 1, // ring_bytes
            0, 5, 0, 0, // width
            0xD0, 2, 0, 0, // height
            60, 0, 0, 0, // max_fps
            1, // mode
        ]
    );
    assert_eq!(
        Message::ShareOpened {
            result: ShareResult::Running
        }
        .payload()
        .unwrap(),
        [2]
    );
    assert!(Message::ShareClose.payload().unwrap().is_empty());
    assert_eq!(
        Message::ShareMode {
            mode: ShareMode::Together
        }
        .payload()
        .unwrap(),
        [2]
    );
    let pad = Message::SharePad(SharePad {
        slot: 1,
        sequence: 0x0201,
        buttons: 0x0000_4010,
        lx: 1,
        ly: 2,
        rx: 3,
        ry: 4,
        l2: 5,
        r2: 6,
    });
    assert_eq!(
        pad.payload().unwrap(),
        [1, 1, 2, 0x10, 0x40, 0, 0, 1, 2, 3, 4, 5, 6]
    );
    assert_eq!(
        Message::Override {
            action: OverrideAction::TookControl
        }
        .payload()
        .unwrap(),
        [1]
    );
    assert_eq!(
        Message::ShareStatus(ShareStatus {
            frames: 1,
            dropped: 2,
            audio_underruns: 3
        })
        .payload()
        .unwrap(),
        [1, 0, 0, 0, 2, 0, 0, 0, 3, 0, 0, 0]
    );
}

#[test]
fn a_neutral_pad_is_nothing_pressed_and_sticks_in_the_middle() {
    assert_eq!(
        SharePad::neutral(1, 9),
        SharePad {
            slot: 1,
            sequence: 9,
            buttons: 0,
            lx: 128,
            ly: 128,
            rx: 128,
            ry: 128,
            l2: 0,
            r2: 0,
        }
    );
}

#[test]
fn unknown_share_values_are_tolerated() {
    assert_eq!(
        Message::parse(Kind::SHARE_MODE, &[9]),
        Ok(Some(Message::ShareMode {
            mode: ShareMode::Unknown
        }))
    );
    assert_eq!(
        Message::parse(Kind::SHARE_OPEN.reply(), &[77]),
        Ok(Some(Message::ShareOpened {
            result: ShareResult::Unknown
        }))
    );
    assert_eq!(
        Message::parse(Kind::OVERRIDE, &[0]),
        Ok(Some(Message::Override {
            action: OverrideAction::Unknown
        }))
    );
}

#[test]
fn every_message_survives_the_round_trip() {
    for message in samples() {
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

// Random input. The other side of the connection is a game process, so nothing it sends may
// crash the net service. A fixed seed keeps every run the same.

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    fn bytes(&mut self, len: usize) -> Vec<u8> {
        (0..len).map(|_| self.next() as u8).collect()
    }
}

/// Every kind this version reads, and one it does not.
fn kinds() -> Vec<Kind> {
    let mut kinds: Vec<Kind> = samples().iter().map(Message::kind).collect();
    kinds.push(Kind(0x0777));
    kinds
}

/// A message read from any payload is written back so that it reads the same. True when the
/// payload was read as a message.
fn reads_back_the_same(kind: Kind, payload: &[u8]) -> bool {
    let Ok(Some(message)) = Message::parse(kind, payload) else {
        return false;
    };
    let again = message.payload().unwrap();
    assert_eq!(Message::parse(kind, &again), Ok(Some(message)));
    true
}

#[test]
fn random_payloads_are_read_or_refused() {
    let kinds = kinds();
    let mut rng = Rng(0x5EED);
    let mut read = 0;
    for _ in 0..50_000 {
        let kind = kinds[rng.below(kinds.len())];
        let len = rng.below(80);
        read += usize::from(reads_back_the_same(kind, &rng.bytes(len)));
    }
    // About half are read; the rest are refused.
    assert!(read > 10_000, "only {read} payloads were read");
}

#[test]
fn damaged_messages_are_read_or_refused() {
    let samples = samples();
    let mut rng = Rng(0xD00D);
    let mut read = 0;
    for _ in 0..50_000 {
        let message = &samples[rng.below(samples.len())];
        let mut payload = message.payload().unwrap();
        for _ in 0..1 + rng.below(3) {
            match rng.below(3) {
                0 if !payload.is_empty() => {
                    let at = rng.below(payload.len());
                    payload[at] ^= 1 << rng.below(8);
                }
                1 => payload.truncate(rng.below(payload.len() + 1)),
                _ => {
                    let len = rng.below(8);
                    payload.extend(rng.bytes(len));
                }
            }
        }
        read += usize::from(reads_back_the_same(message.kind(), &payload));
    }
    assert!(read > 10_000, "only {read} payloads were read");
}

#[test]
fn a_cut_message_is_truncated_never_misread() {
    for message in samples() {
        // These end in a field that takes the rest of the payload, so a cut one still reads.
        if matches!(message, Message::Hello(_) | Message::Reject(_)) {
            continue;
        }
        let payload = message.payload().unwrap();
        for len in 0..payload.len() {
            assert_eq!(
                Message::parse(message.kind(), &payload[..len]),
                Err(Error::Truncated),
                "{message:?} cut at {len}"
            );
        }
    }
}

#[test]
fn the_stream_reads_the_same_however_it_is_cut() {
    let frames: Vec<Frame> = samples()
        .iter()
        .zip(1..)
        .map(|(message, request)| Frame::numbered(message, request).unwrap())
        .collect();
    let mut wire = Vec::new();
    for frame in &frames {
        frame.encode(&mut wire).unwrap();
    }
    let mut rng = Rng(0xC0FFEE);
    for _ in 0..500 {
        let mut decoder = Decoder::new();
        let mut got = Vec::new();
        let mut at = 0;
        while at < wire.len() {
            let end = (at + 1 + rng.below(40)).min(wire.len());
            decoder.push(&wire[at..end]);
            at = end;
            while let Some(frame) = decoder.next_frame().unwrap() {
                got.push(frame);
            }
        }
        assert_eq!(got, frames);
    }
}

#[test]
fn random_bytes_on_the_connection_are_framed_or_refused() {
    let kinds = kinds();
    let mut rng = Rng(0xBAD_5EED);
    let mut decoder = Decoder::new();
    let (mut frames, mut read) = (0, 0);
    for round in 0..20_000 {
        // A new connection now and then, since noise can announce a frame that never ends.
        if round % 100 == 0 {
            decoder = Decoder::new();
        }
        // Mostly a plausible header with a random payload, sometimes plain noise.
        let chunk = if rng.below(4) == 0 {
            let len = rng.below(32);
            rng.bytes(len)
        } else {
            let len = rng.below(48);
            let mut chunk = (len as u32).to_le_bytes().to_vec();
            chunk.extend(kinds[rng.below(kinds.len())].0.to_le_bytes());
            chunk.extend(rng.bytes(6 + len));
            chunk
        };
        decoder.push(&chunk);
        loop {
            match decoder.next_frame() {
                Ok(Some(frame)) => {
                    assert!(frame.payload.len() <= MAX_PAYLOAD);
                    frames += 1;
                    read += usize::from(reads_back_the_same(frame.kind, &frame.payload));
                }
                Ok(None) => break,
                // The net service closes such a connection.
                Err(Error::TooLarge(_)) => {
                    decoder = Decoder::new();
                    break;
                }
                Err(other) => panic!("the decoder refuses only sizes, not {other:?}"),
            }
        }
    }
    assert!(
        frames > 5_000 && read > 1_000,
        "{frames} frames, {read} read"
    );
}

#[test]
fn handshake_kinds_and_layouts_are_the_ones_in_the_spec() {
    assert_eq!(VERSION, 1);
    assert_eq!([Kind::CHALLENGE.0, Kind::AUTH.0], [0x0004, 0x0005]);

    // The hello frame of the specification's test vectors.
    let nonce: Vec<u8> = (0x20..0x40).collect();
    let hello = Frame::event(&Message::Hello(Hello {
        version: VERSION,
        nonce: nonce.clone(),
    }))
    .unwrap();
    let mut expected = vec![34, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 1, 0];
    expected.extend_from_slice(&nonce);
    assert_eq!(bytes(&hello), expected);

    let challenge = Message::Challenge(Challenge {
        version: 1,
        nonce: [2; NONCE_LEN],
        proof: [3; PROOF_LEN],
    });
    let mut payload = vec![1, 0];
    payload.extend_from_slice(&[2; NONCE_LEN]);
    payload.extend_from_slice(&[3; PROOF_LEN]);
    assert_eq!(challenge.payload().unwrap(), payload);
    assert_eq!(payload.len(), 66);

    let auth = Message::Auth(Auth {
        proof: [4; PROOF_LEN],
    });
    assert_eq!(auth.payload().unwrap(), vec![4; PROOF_LEN]);
}

#[test]
fn a_short_challenge_or_auth_is_truncated() {
    assert_eq!(
        Message::parse(Kind::CHALLENGE, &[1, 0, 0]),
        Err(Error::Truncated)
    );
    assert_eq!(
        Message::parse(Kind::AUTH, &[0; PROOF_LEN - 1]),
        Err(Error::Truncated)
    );
    // Bytes after the fields are ignored, as for every message.
    let mut longer = vec![9; PROOF_LEN + 5];
    longer[PROOF_LEN..].fill(0);
    assert_eq!(
        Message::parse(Kind::AUTH, &longer),
        Ok(Some(Message::Auth(Auth {
            proof: [9; PROOF_LEN]
        })))
    );
}

#[test]
fn the_handshake_is_read_with_the_small_limit_until_it_is_done() {
    let mut header = ((MAX_HELLO_PAYLOAD + 1) as u32).to_le_bytes().to_vec();
    header.extend_from_slice(&[5, 0, 0, 0, 0, 0, 0, 0]);
    let mut decoder = Decoder::with_max_payload(MAX_HELLO_PAYLOAD);
    decoder.push(&header);
    assert_eq!(
        decoder.next_frame(),
        Err(Error::TooLarge(MAX_HELLO_PAYLOAD + 1))
    );
    // The same header is fine once the limit is raised.
    let mut decoder = Decoder::with_max_payload(MAX_HELLO_PAYLOAD);
    decoder.set_max_payload(MAX_PAYLOAD);
    decoder.push(&header);
    assert_eq!(decoder.next_frame(), Ok(None));
}
