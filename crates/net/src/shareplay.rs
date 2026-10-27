//! The host's side of share play towards the game: the frame ring handed to the engine, the
//! mode, and the guest's pad states on their way in (`spec/share-play.md`, Bridge protocol
//! additions and Input).
//!
//! [`ShareHost`] is a state machine and nothing else: it takes what happens (a ring that the
//! share worker made, a bridge frame, a datagram of the share connection, the clock) and
//! answers with [`Step`]s, the frames to send to the bridge and what the rest of the service
//! has to do. It owns no socket and no clock, so the rules are tested without either. The share
//! connection and the cloud's `Share` record sit above it and are not part of it.
//!
//! What it holds to:
//!
//! * Pad states reach the game only while the ring is open and the mode is `hand_over` or
//!   `together`, and only through [`PadGate`]: exact parsing, 300 states a second, the order of
//!   the sequence, 20 invalid datagrams in 5 seconds end the connection.
//! * A state is a pad and nothing else; there is no message here that carries a key, a file or
//!   a pointer.
//! * Whenever the guest's pad goes away (the mode changes, the share closes, the guest falls
//!   silent for 250 ms, the bridge is lost) the game has been given a neutral state first.

use anyplay_bridge_protocol::{
    Frame, Message, OverrideAction, ShareMode, ShareOpen, SharePad, ShareResult, ShareStatus,
};
use anyplay_share::link::RingReady;
use anyplay_share::pad::{PadGate, PadMode, PadState};

/// The close code of the share connection for too many invalid pad datagrams.
pub const CLOSE_TOO_MANY_INVALID: u16 = 7;

/// What the service has to do after an input.
#[derive(Debug, PartialEq, Eq)]
pub enum Step {
    /// A frame for the bridge.
    Bridge(Frame),
    /// Something the rest of the service acts on.
    Event(Event),
}

/// What the rest of the service learns.
#[derive(Debug, PartialEq, Eq)]
pub enum Event {
    /// The engine copies the game's pictures into the ring.
    Opened,
    /// The engine refused the ring. The share cannot start.
    OpenFailed(ShareResult),
    /// The mode is now this one (the host asked for it, or took control back).
    Mode(ShareMode),
    /// The host took control back on their own pad: the mode is now `watch`. The guest, the
    /// launcher and the cloud are told.
    TookControl,
    /// The host asked to stop (the stop key, or the chord while only watching).
    StopRequested,
    /// The engine's count of frames, dropped frames and sound underruns, once a second.
    Status(ShareStatus),
    /// Close the share connection with this code.
    CloseGuest(u16),
    /// The bridge went away: the share is over.
    BridgeLost,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    /// `share_open` is sent, with this request number, and not answered.
    Opening(u32),
    Open,
    Closed,
}

/// One share on the host's PC, from the ring to the game's pad.
#[derive(Debug)]
pub struct ShareHost {
    gate: PadGate,
    phase: Phase,
    mode: ShareMode,
    /// The sequence of the newest state sent to the game, for the neutral states after it.
    sequence: u16,
}

fn pad_mode(mode: ShareMode) -> PadMode {
    match mode {
        ShareMode::HandOver => PadMode::HandOver,
        ShareMode::Together => PadMode::Together,
        ShareMode::Watch | ShareMode::Unknown => PadMode::Watch,
    }
}

fn event(message: Message) -> Step {
    Step::Bridge(Frame::event(&message).expect("a message of this size can be framed"))
}

impl ShareHost {
    /// Starts a share: the worker's ring is ready, and the game is asked to fill it. The
    /// first steps are the `share_open` request.
    pub fn open(now_us: u64, ring: &RingReady, mode: ShareMode) -> (ShareHost, Vec<Step>) {
        let mode = if mode == ShareMode::Unknown {
            ShareMode::Watch
        } else {
            mode
        };
        let mut gate = PadGate::new(now_us);
        gate.set_mode(pad_mode(mode));
        let request = 1;
        let open = Message::ShareOpen(ShareOpen {
            ring: ring.name.clone(),
            ring_bytes: ring.bytes,
            width: ring.width,
            height: ring.height,
            max_fps: ring.max_fps,
            mode,
        });
        let frame = Frame::numbered(&open, request).expect("a share open can be framed");
        (
            ShareHost {
                gate,
                phase: Phase::Opening(request),
                mode,
                sequence: 0,
            },
            vec![Step::Bridge(frame)],
        )
    }

    pub fn mode(&self) -> ShareMode {
        self.mode
    }

    /// True while the engine copies pictures and a pad may be played.
    pub fn is_open(&self) -> bool {
        self.phase == Phase::Open
    }

    /// True once the share is over: closed by the host, refused, or the bridge was lost.
    pub fn is_closed(&self) -> bool {
        self.phase == Phase::Closed
    }

    /// The slot the guest plays on in the current mode.
    fn slot(&self) -> Option<u8> {
        self.gate.mode().slot()
    }

    fn neutral(&self) -> Option<Step> {
        let slot = self.slot()?;
        (self.phase != Phase::Closed)
            .then(|| event(Message::SharePad(SharePad::neutral(slot, self.sequence))))
    }

    /// The host changes the mode. The guest's pad is neutral on the old slot before it goes.
    pub fn set_mode(&mut self, mode: ShareMode) -> Vec<Step> {
        if self.phase == Phase::Closed || mode == ShareMode::Unknown || mode == self.mode {
            return Vec::new();
        }
        let mut steps = Vec::new();
        steps.extend(self.neutral());
        self.mode = mode;
        self.gate.set_mode(pad_mode(mode));
        steps.push(event(Message::ShareMode { mode }));
        steps.push(Step::Event(Event::Mode(mode)));
        steps
    }

    /// The share ends from the host's side: neutral, then the game stops copying.
    pub fn close(&mut self) -> Vec<Step> {
        if self.phase == Phase::Closed {
            return Vec::new();
        }
        let mut steps = Vec::new();
        steps.extend(self.neutral());
        steps.push(event(Message::ShareClose));
        self.phase = Phase::Closed;
        steps
    }

    /// The bridge's connection broke. The engine has already neutralised the guest's pad and
    /// stopped copying; the share is over.
    pub fn bridge_lost(&mut self) -> Vec<Step> {
        if self.phase == Phase::Closed {
            return Vec::new();
        }
        self.phase = Phase::Closed;
        vec![Step::Event(Event::BridgeLost)]
    }

    /// A frame from the bridge: the answer to `share_open`, the host's override, the status.
    pub fn on_bridge(&mut self, frame: &Frame) -> Vec<Step> {
        let Ok(Some(message)) = frame.message() else {
            return Vec::new();
        };
        match message {
            Message::ShareOpened { result } => {
                if self.phase != Phase::Opening(frame.request) {
                    return Vec::new();
                }
                if result == ShareResult::Copying {
                    self.phase = Phase::Open;
                    vec![Step::Event(Event::Opened)]
                } else {
                    self.phase = Phase::Closed;
                    vec![Step::Event(Event::OpenFailed(result))]
                }
            }
            Message::Override { action } if self.phase == Phase::Open => match action {
                OverrideAction::TookControl => {
                    let mut steps = Vec::new();
                    if self.mode != ShareMode::Watch {
                        // The engine has already dropped the guest's pad; this keeps both
                        // sides of the bridge in the same mode.
                        steps.extend(self.neutral());
                        self.mode = ShareMode::Watch;
                        self.gate.set_mode(PadMode::Watch);
                        steps.push(event(Message::ShareMode {
                            mode: ShareMode::Watch,
                        }));
                    }
                    steps.push(Step::Event(Event::TookControl));
                    steps
                }
                OverrideAction::Stop => vec![Step::Event(Event::StopRequested)],
                OverrideAction::Unknown => Vec::new(),
            },
            Message::ShareStatus(status) if self.phase == Phase::Open => {
                vec![Step::Event(Event::Status(status))]
            }
            _ => Vec::new(),
        }
    }

    /// A datagram that came on the share connection and is a pad datagram (the caller has
    /// looked at its type byte).
    pub fn on_pad_datagram(&mut self, now_us: u64, bytes: &[u8]) -> Vec<Step> {
        if self.phase != Phase::Open {
            return Vec::new();
        }
        let verdict = self.gate.on_datagram(now_us, bytes);
        let mut steps = Vec::new();
        if let Some(slot) = self.slot() {
            for (sequence, state) in verdict.apply {
                self.sequence = sequence;
                steps.push(event(Message::SharePad(share_pad(slot, sequence, state))));
            }
        }
        if verdict.close {
            steps.push(Step::Event(Event::CloseGuest(CLOSE_TOO_MANY_INVALID)));
        }
        steps
    }

    /// Call at least every 50 ms. After 250 ms without a valid pad datagram the game gets a
    /// neutral state, once.
    pub fn tick(&mut self, now_us: u64) -> Vec<Step> {
        if self.phase != Phase::Open {
            return Vec::new();
        }
        match (self.gate.poll(now_us), self.slot()) {
            (Some(state), Some(slot)) => {
                vec![event(Message::SharePad(share_pad(
                    slot,
                    self.sequence,
                    state,
                )))]
            }
            _ => Vec::new(),
        }
    }
}

fn share_pad(slot: u8, sequence: u16, state: PadState) -> SharePad {
    SharePad {
        slot,
        sequence,
        buttons: state.buttons,
        lx: state.lx,
        ly: state.ly,
        rx: state.rx,
        ry: state.ry,
        l2: state.l2,
        r2: state.r2,
    }
}

#[cfg(test)]
mod tests {
    use anyplay_share::pad::{PadDatagram, PadSender, button};

    use super::*;

    fn ring() -> RingReady {
        RingReady {
            name: "anyplay-00112233445566ff".into(),
            bytes: 123_456,
            width: 1280,
            height: 720,
            max_fps: 60,
        }
    }

    /// The messages among the bridge frames of `steps`.
    fn sent(steps: &[Step]) -> Vec<Message> {
        steps
            .iter()
            .filter_map(|s| match s {
                Step::Bridge(f) => f.message().unwrap(),
                Step::Event(_) => None,
            })
            .collect()
    }

    fn events(steps: &[Step]) -> Vec<&Event> {
        steps
            .iter()
            .filter_map(|s| match s {
                Step::Event(e) => Some(e),
                Step::Bridge(_) => None,
            })
            .collect()
    }

    fn opened(host: &mut ShareHost) {
        let reply = Frame::numbered(
            &Message::ShareOpened {
                result: ShareResult::Copying,
            },
            1,
        )
        .unwrap();
        assert_eq!(events(&host.on_bridge(&reply)), [&Event::Opened]);
    }

    fn started(mode: ShareMode) -> ShareHost {
        let (mut host, _) = ShareHost::open(0, &ring(), mode);
        opened(&mut host);
        host
    }

    fn datagram(sender: &mut PadSender, buttons: u32) -> Vec<u8> {
        sender.next(PadState {
            buttons,
            ..PadState::NEUTRAL
        })
    }

    #[test]
    fn opening_asks_the_game_to_fill_the_ring_the_worker_made() {
        let (host, steps) = ShareHost::open(0, &ring(), ShareMode::Together);
        let [Step::Bridge(frame)] = steps.as_slice() else {
            panic!("one bridge frame, got {steps:?}");
        };
        assert_eq!(frame.request, 1, "a request, so that it is answered");
        assert_eq!(
            frame.message().unwrap(),
            Some(Message::ShareOpen(ShareOpen {
                ring: ring().name,
                ring_bytes: 123_456,
                width: 1280,
                height: 720,
                max_fps: 60,
                mode: ShareMode::Together,
            }))
        );
        assert!(!host.is_open() && !host.is_closed());
    }

    #[test]
    fn the_game_says_whether_it_copies() {
        let (mut host, _) = ShareHost::open(0, &ring(), ShareMode::Watch);
        let reply =
            |result, request| Frame::numbered(&Message::ShareOpened { result }, request).unwrap();
        // An answer to another request is not ours.
        assert!(host.on_bridge(&reply(ShareResult::Copying, 9)).is_empty());
        assert!(!host.is_open());
        let steps = host.on_bridge(&reply(ShareResult::Copying, 1));
        assert_eq!(events(&steps), [&Event::Opened]);
        assert!(host.is_open());

        for refusal in [
            ShareResult::RingUnavailable,
            ShareResult::Running,
            ShareResult::NotPresenting,
            ShareResult::Unknown,
        ] {
            let (mut host, _) = ShareHost::open(0, &ring(), ShareMode::Watch);
            let steps = host.on_bridge(&reply(refusal, 1));
            assert_eq!(events(&steps), [&Event::OpenFailed(refusal)]);
            assert!(host.is_closed());
        }
    }

    #[test]
    fn a_pad_state_reaches_the_game_on_the_slot_of_the_mode() {
        for (mode, slot) in [(ShareMode::HandOver, 0u8), (ShareMode::Together, 1)] {
            let mut host = started(mode);
            let mut sender = PadSender::new();
            let steps = host.on_pad_datagram(1_000, &datagram(&mut sender, button::SOUTH));
            assert_eq!(
                sent(&steps),
                [Message::SharePad(SharePad {
                    slot,
                    sequence: 0,
                    buttons: button::SOUTH,
                    lx: 128,
                    ly: 128,
                    rx: 128,
                    ry: 128,
                    l2: 0,
                    r2: 0,
                })]
            );
        }
    }

    #[test]
    fn watch_and_a_share_that_is_not_open_pass_no_pad() {
        let mut sender = PadSender::new();
        let mut watch = started(ShareMode::Watch);
        assert!(
            watch
                .on_pad_datagram(1_000, &datagram(&mut sender, button::SOUTH))
                .is_empty()
        );
        let (mut opening, _) = ShareHost::open(0, &ring(), ShareMode::HandOver);
        assert!(
            opening
                .on_pad_datagram(1_000, &datagram(&mut sender, button::SOUTH))
                .is_empty()
        );
        let mut closed = started(ShareMode::HandOver);
        closed.close();
        assert!(
            closed
                .on_pad_datagram(1_000, &datagram(&mut sender, button::SOUTH))
                .is_empty()
        );
    }

    #[test]
    fn malformed_and_reserved_datagrams_reach_no_game() {
        let mut host = started(ShareMode::HandOver);
        let mut good = PadSender::new().next(PadState::NEUTRAL);
        good[5] = 0x80; // a reserved bit
        assert!(host.on_pad_datagram(1_000, &good).is_empty());
        assert!(host.on_pad_datagram(2_000, b"not a pad").is_empty());
        assert!(host.on_pad_datagram(3_000, &[]).is_empty());
    }

    #[test]
    fn many_invalid_datagrams_end_the_guest_connection() {
        let mut host = started(ShareMode::HandOver);
        let mut closed = false;
        for i in 0..40u64 {
            let steps = host.on_pad_datagram(i * 1_000, b"nope");
            if events(&steps).contains(&&Event::CloseGuest(7)) {
                closed = true;
                assert!(i >= 20);
                break;
            }
        }
        assert!(closed);
    }

    #[test]
    fn silence_gives_the_game_one_neutral_state_on_the_guests_slot() {
        let mut host = started(ShareMode::Together);
        let mut sender = PadSender::new();
        host.on_pad_datagram(1_000_000, &datagram(&mut sender, button::SOUTH));
        assert!(host.tick(1_100_000).is_empty());
        let steps = host.tick(1_250_000);
        assert_eq!(
            sent(&steps),
            [Message::SharePad(SharePad::neutral(1, 0))],
            "the neutral state is sent on pad 2 with the last sequence"
        );
        assert!(host.tick(2_000_000).is_empty(), "once");
    }

    #[test]
    fn leaving_a_pad_mode_neutralises_the_pad_before_the_mode_changes() {
        let mut host = started(ShareMode::HandOver);
        let mut sender = PadSender::new();
        host.on_pad_datagram(1_000, &datagram(&mut sender, button::SOUTH));
        let steps = host.set_mode(ShareMode::Watch);
        assert_eq!(
            sent(&steps),
            [
                Message::SharePad(SharePad::neutral(0, 0)),
                Message::ShareMode {
                    mode: ShareMode::Watch
                },
            ]
        );
        assert_eq!(events(&steps), [&Event::Mode(ShareMode::Watch)]);
        // Watch: nothing the guest sends gets through any more.
        assert!(
            host.on_pad_datagram(2_000, &datagram(&mut sender, button::EAST))
                .is_empty()
        );
        assert!(host.set_mode(ShareMode::Watch).is_empty(), "no change");
    }

    #[test]
    fn going_from_hand_over_to_together_moves_the_guest_to_pad_two() {
        let mut host = started(ShareMode::HandOver);
        let mut sender = PadSender::new();
        host.on_pad_datagram(1_000, &datagram(&mut sender, button::SOUTH));
        let steps = host.set_mode(ShareMode::Together);
        assert_eq!(
            sent(&steps),
            [
                Message::SharePad(SharePad::neutral(0, 0)),
                Message::ShareMode {
                    mode: ShareMode::Together
                },
            ]
        );
        // The viewer's numbering goes on; the gate starts again for the new mode.
        let steps = host.on_pad_datagram(2_000, &datagram(&mut sender, button::EAST));
        let messages = sent(&steps);
        let [Message::SharePad(pad)] = messages.as_slice() else {
            panic!("one pad state");
        };
        assert_eq!((pad.slot, pad.buttons), (1, button::EAST));
    }

    #[test]
    fn the_host_taking_control_back_makes_it_watch_everywhere() {
        let mut host = started(ShareMode::Together);
        let took = Frame::event(&Message::Override {
            action: OverrideAction::TookControl,
        })
        .unwrap();
        let steps = host.on_bridge(&took);
        assert_eq!(
            sent(&steps),
            [
                Message::SharePad(SharePad::neutral(1, 0)),
                Message::ShareMode {
                    mode: ShareMode::Watch
                },
            ]
        );
        assert_eq!(events(&steps), [&Event::TookControl]);
        assert_eq!(host.mode(), ShareMode::Watch);
        let mut sender = PadSender::new();
        assert!(
            host.on_pad_datagram(1_000, &datagram(&mut sender, button::SOUTH))
                .is_empty()
        );
        // While only watching, the chord is a plain take-back: nothing to change.
        let steps = host.on_bridge(&took);
        assert!(sent(&steps).is_empty());
    }

    #[test]
    fn the_stop_key_asks_to_stop() {
        let mut host = started(ShareMode::HandOver);
        let stop = Frame::event(&Message::Override {
            action: OverrideAction::Stop,
        })
        .unwrap();
        let steps = host.on_bridge(&stop);
        assert_eq!(events(&steps), [&Event::StopRequested]);
        assert!(sent(&steps).is_empty(), "the caller decides how to stop");
    }

    #[test]
    fn the_status_passes_up() {
        let mut host = started(ShareMode::Watch);
        let status = ShareStatus {
            frames: 60,
            dropped: 1,
            audio_underruns: 0,
        };
        let steps = host.on_bridge(&Frame::event(&Message::ShareStatus(status)).unwrap());
        assert_eq!(events(&steps), [&Event::Status(status)]);
    }

    #[test]
    fn frames_before_the_game_answered_are_ignored() {
        let (mut host, _) = ShareHost::open(0, &ring(), ShareMode::HandOver);
        let took = Frame::event(&Message::Override {
            action: OverrideAction::Stop,
        })
        .unwrap();
        assert!(host.on_bridge(&took).is_empty());
        let status = Frame::event(&Message::ShareStatus(ShareStatus {
            frames: 1,
            dropped: 0,
            audio_underruns: 0,
        }))
        .unwrap();
        assert!(host.on_bridge(&status).is_empty());
        // Frames of kinds this version does not know are skipped.
        let unknown = Frame {
            kind: anyplay_bridge_protocol::Kind(0x05FF),
            request: 0,
            payload: vec![1, 2, 3],
        };
        assert!(host.on_bridge(&unknown).is_empty());
    }

    #[test]
    fn closing_neutralises_then_tells_the_game_to_stop_copying() {
        let mut host = started(ShareMode::Together);
        let steps = host.close();
        assert_eq!(
            sent(&steps),
            [
                Message::SharePad(SharePad::neutral(1, 0)),
                Message::ShareClose
            ]
        );
        assert!(host.is_closed());
        assert!(host.close().is_empty(), "once");
        assert!(host.set_mode(ShareMode::Watch).is_empty());
        assert!(host.tick(10_000_000).is_empty());
    }

    #[test]
    fn closing_in_watch_only_closes() {
        let mut host = started(ShareMode::Watch);
        assert_eq!(sent(&host.close()), [Message::ShareClose]);
    }

    #[test]
    fn a_lost_bridge_ends_the_share_without_talking_to_it() {
        let mut host = started(ShareMode::HandOver);
        let steps = host.bridge_lost();
        assert_eq!(events(&steps), [&Event::BridgeLost]);
        assert!(sent(&steps).is_empty());
        assert!(host.is_closed());
        assert!(host.bridge_lost().is_empty());
    }

    #[test]
    fn states_lost_on_the_way_are_filled_from_the_repeats() {
        let mut host = started(ShareMode::HandOver);
        let mut sender = PadSender::new();
        let first = datagram(&mut sender, button::SOUTH);
        let _lost = datagram(&mut sender, button::EAST);
        let third = datagram(&mut sender, button::WEST);
        assert_eq!(sent(&host.on_pad_datagram(1_000, &first)).len(), 1);
        let steps = host.on_pad_datagram(5_000, &third);
        let buttons: Vec<u32> = sent(&steps)
            .iter()
            .map(|m| match m {
                Message::SharePad(p) => p.buttons,
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(
            buttons,
            [button::EAST, button::WEST],
            "the release is not lost"
        );
        assert!(PadDatagram::parse(&third).is_ok());
    }
}
