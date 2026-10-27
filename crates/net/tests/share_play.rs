//! Share play between the net service's host side and a game: the real bridge listener on
//! loopback, a fake game that follows the engine's rules for the share kinds, the frame ring
//! made by the share worker's code and filled through a second mapping the way the engine does,
//! and a guest whose pad states are datagrams.
//!
//! No iroh and no cloud: those carry the datagrams and the invitation, and sit above
//! [`ShareHost`].

use std::time::{Duration, Instant};

use anyplay_bridge_protocol::{
    Frame, Message, OverrideAction, ShareMode, ShareResult, ShareStatus, VERSION, Welcome,
};
use anyplay_fake_game::{Bridge, ShareEngine};
use anyplay_net::bridge::{self, BridgeInput};
use anyplay_net::shareplay::{Event, ShareHost, Step};
use anyplay_share::link::RingReady;
use anyplay_share::pad::{PadSender, PadState, button};
use anyplay_share::ring::{RingWriter, name_from_random};
use anyplay_share_worker::host::{FrameSource, Next};
use anyplay_share_worker::ring_source::{self, RingSource};
use anyplay_share_worker::shm::Shm;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

const WIDTH: u32 = 64;
const HEIGHT: u32 = 36;

struct Stage {
    host: ShareHost,
    frames: UnboundedSender<Frame>,
    inputs: UnboundedReceiver<BridgeInput>,
    /// `None` once the game has exited.
    game: Option<Bridge>,
    source: RingSource<std::sync::Arc<Shm>>,
    ready: RingReady,
    events: Vec<Event>,
    start: Instant,
}

fn ring_name() -> String {
    let mut random = [3u8; 16];
    random[..4].copy_from_slice(&std::process::id().to_le_bytes());
    random[4..12].copy_from_slice(
        &(std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos() as u64)
            .to_le_bytes(),
    );
    name_from_random(random)
}

/// Waits up to five seconds for `condition`.
async fn eventually(what: &str, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !condition() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// A game connected to a bridge listener, as the net service accepts it: the frames to send it
/// and the inputs from it.
async fn connect_game() -> (
    Bridge,
    UnboundedSender<Frame>,
    UnboundedReceiver<BridgeInput>,
) {
    let (tx, mut inputs) = mpsc::unbounded_channel();
    let port = bridge::listen(tx).await.expect("the listener");
    let endpoint = format!("127.0.0.1:{port}");
    let connecting = tokio::spawn(async move { Bridge::connect(&endpoint, "0a0b0c").await });
    let BridgeInput::Hello {
        frames,
        verdict,
        token,
        ..
    } = inputs.recv().await.expect("a hello")
    else {
        panic!("the first input is the hello");
    };
    assert_eq!(token, [0x0a, 0x0b, 0x0c]);
    let welcome = Message::Welcome(Welcome {
        version: VERSION,
        udp_port: 0,
        tcp_port: 0,
        stream_key: [0; 16],
    });
    frames.send(Frame::event(&welcome).unwrap()).unwrap();
    verdict.send(Ok(())).unwrap();
    let game = connecting.await.unwrap().expect("the game connects");
    (game, frames, inputs)
}

impl Stage {
    /// A game connected to the bridge listener, a ring made by the worker's code, and a share
    /// that has asked the game to fill it. Not answered yet.
    async fn begin(mode: ShareMode, refuse: Option<ShareResult>) -> Stage {
        let (game, frames, inputs) = connect_game().await;
        if let Some(result) = refuse {
            game.refuse_share_open(result);
        }
        let (source, _sound, ready) = ring_source::create(&ring_name(), WIDTH, HEIGHT, 60).unwrap();
        let start = Instant::now();
        let (host, steps) = ShareHost::open(0, &ready, mode);
        let mut stage = Stage {
            host,
            frames,
            inputs,
            game: Some(game),
            source,
            ready,
            events: Vec::new(),
            start,
        };
        stage.run(steps);
        stage
    }

    /// `begin`, and the game has said "copying".
    async fn open(mode: ShareMode) -> Stage {
        let mut stage = Stage::begin(mode, None).await;
        stage.pump_until(|s| s.host.is_open()).await;
        stage
    }

    fn game(&self) -> &Bridge {
        self.game.as_ref().expect("the game is still there")
    }

    fn now_us(&self) -> u64 {
        self.start.elapsed().as_micros() as u64
    }

    /// Does what the steps say: frames to the bridge, events kept.
    fn run(&mut self, steps: Vec<Step>) {
        for step in steps {
            match step {
                Step::Bridge(frame) => {
                    let _ = self.frames.send(frame);
                }
                Step::Event(event) => self.events.push(event),
            }
        }
    }

    /// Takes what the game sent, until `done`.
    async fn pump_until(&mut self, done: impl Fn(&Stage) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !done(self) {
            assert!(
                Instant::now() < deadline,
                "timed out; events {:?}",
                self.events
            );
            match tokio::time::timeout(Duration::from_millis(20), self.inputs.recv()).await {
                Ok(Some(BridgeInput::Frame { frame, .. })) => {
                    let steps = self.host.on_bridge(&frame);
                    self.run(steps);
                }
                Ok(Some(BridgeInput::Closed { .. })) => {
                    let steps = self.host.bridge_lost();
                    self.run(steps);
                }
                Ok(Some(BridgeInput::Hello { .. })) | Ok(None) | Err(_) => {}
            }
        }
    }

    fn engine(&self) -> ShareEngine {
        self.game().share_engine()
    }

    /// Sends a pad datagram of the guest and waits until the game saw `count` states.
    async fn guest_presses(&mut self, sender: &mut PadSender, buttons: u32, taken: usize) {
        let datagram = sender.next(PadState {
            buttons,
            ..PadState::NEUTRAL
        });
        let now = self.now_us();
        let steps = self.host.on_pad_datagram(now, &datagram);
        self.run(steps);
        let game = self.game();
        eventually("the game to take the state", || {
            game.share_engine().taken.len() >= taken
        })
        .await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_game_is_asked_for_the_workers_ring_and_fills_it() {
    let mut stage = Stage::open(ShareMode::Watch).await;
    assert_eq!(stage.events, [Event::Opened]);
    let engine = stage.engine();
    let open = engine.opened.expect("the game took the open");
    assert_eq!(open.ring, stage.ready.name);
    assert_eq!(open.ring_bytes, stage.ready.bytes);
    assert_eq!((open.width, open.height, open.max_fps), (WIDTH, HEIGHT, 60));
    assert_eq!(open.mode, ShareMode::Watch);

    // The engine maps the ring by that name, checks it, and writes a picture and sound.
    let memory = Shm::open(&open.ring, open.ring_bytes as usize).unwrap();
    let mut writer =
        RingWriter::open(memory, open.width, open.height, open.ring_bytes as usize).unwrap();
    assert!(matches!(stage.source.next_frame(), Next::NotYet));
    let picture: Vec<u8> = (0..WIDTH * HEIGHT * 4).map(|i| (i % 251) as u8).collect();
    assert!(writer.push_picture(&picture, 1_000_000, 42));
    match stage.source.next_frame() {
        Next::Picture(frame) => {
            assert_eq!((frame.width, frame.height), (WIDTH, HEIGHT));
            assert_eq!(frame.bgra, picture);
            assert_eq!(frame.timestamp_us, 1_000_000);
        }
        other => panic!("a picture from the ring, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_game_that_cannot_open_the_ring_ends_the_share_before_it_starts() {
    let mut stage = Stage::begin(ShareMode::HandOver, Some(ShareResult::RingUnavailable)).await;
    stage.pump_until(|s| s.host.is_closed()).await;
    assert_eq!(
        stage.events,
        [Event::OpenFailed(ShareResult::RingUnavailable)]
    );
    assert_eq!(stage.engine().opened, None);
}

#[tokio::test(flavor = "multi_thread")]
async fn hand_over_puts_the_guests_pad_in_place_of_pad_one() {
    let mut stage = Stage::open(ShareMode::HandOver).await;
    let mut guest = PadSender::new();
    stage.guest_presses(&mut guest, button::SOUTH, 1).await;
    let engine = stage.engine();
    let pad = engine.now[0].expect("pad 1 is the guest's");
    assert_eq!((pad.slot, pad.buttons), (0, button::SOUTH));
    assert_eq!(engine.now[1], None);

    // Another state, and the release.
    stage
        .guest_presses(&mut guest, button::SOUTH | button::R1, 2)
        .await;
    stage.guest_presses(&mut guest, 0, 3).await;
    let engine = stage.engine();
    assert_eq!(engine.now[0].unwrap().buttons, 0);
    assert_eq!(engine.refused, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn together_gives_the_guest_pad_two_and_leaves_pad_one_alone() {
    let mut stage = Stage::open(ShareMode::Together).await;
    let mut guest = PadSender::new();
    stage.guest_presses(&mut guest, button::EAST, 1).await;
    let engine = stage.engine();
    assert_eq!(engine.now[0], None);
    let pad = engine.now[1].expect("pad 2 is the guest's");
    assert_eq!((pad.slot, pad.buttons), (1, button::EAST));
}

#[tokio::test(flavor = "multi_thread")]
async fn watch_gives_the_guest_no_pad_at_all() {
    let mut stage = Stage::open(ShareMode::Watch).await;
    let mut guest = PadSender::new();
    let datagram = guest.next(PadState {
        buttons: button::SOUTH,
        ..PadState::NEUTRAL
    });
    let now = stage.now_us();
    assert!(stage.host.on_pad_datagram(now, &datagram).is_empty());
    tokio::time::sleep(Duration::from_millis(100)).await;
    let engine = stage.engine();
    assert!(engine.taken.is_empty());
    assert_eq!(engine.now, [None, None]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_guest_who_falls_silent_is_neutral_in_the_game() {
    let mut stage = Stage::open(ShareMode::HandOver).await;
    let mut guest = PadSender::new();
    stage.guest_presses(&mut guest, button::SOUTH, 1).await;
    let now = stage.now_us();
    assert!(stage.host.tick(now + 100_000).is_empty());
    let steps = stage.host.tick(now + 300_000);
    stage.run(steps);
    let game = stage.game();
    eventually("the neutral state", || {
        game.share_engine().now[0].is_some_and(|p| p.buttons == 0)
    })
    .await;
    assert_eq!(stage.engine().taken.len(), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn changing_the_mode_moves_or_removes_the_guests_pad() {
    let mut stage = Stage::open(ShareMode::HandOver).await;
    let mut guest = PadSender::new();
    stage.guest_presses(&mut guest, button::SOUTH, 1).await;

    let steps = stage.host.set_mode(ShareMode::Together);
    stage.run(steps);
    let game = stage.game();
    eventually("the engine to follow the mode", || {
        game.share_engine().mode == Some(ShareMode::Together)
    })
    .await;
    assert_eq!(stage.engine().now, [None, None], "pad 1 was taken away");
    stage.guest_presses(&mut guest, button::EAST, 3).await;
    assert_eq!(stage.engine().now[1].unwrap().buttons, button::EAST);

    let steps = stage.host.set_mode(ShareMode::Watch);
    stage.run(steps);
    let game = stage.game();
    eventually("watch", || {
        game.share_engine().mode == Some(ShareMode::Watch)
    })
    .await;
    assert_eq!(stage.engine().now, [None, None]);
    assert_eq!(stage.events.last(), Some(&Event::Mode(ShareMode::Watch)));
}

#[tokio::test(flavor = "multi_thread")]
async fn the_host_taking_control_back_stops_the_guest_pad() {
    let mut stage = Stage::open(ShareMode::Together).await;
    let mut guest = PadSender::new();
    stage.guest_presses(&mut guest, button::SOUTH, 1).await;

    stage.game().send(&Message::Override {
        action: OverrideAction::TookControl,
    });
    stage
        .pump_until(|s| s.events.contains(&Event::TookControl))
        .await;
    let game = stage.game();
    eventually("watch in the engine", || {
        game.share_engine().mode == Some(ShareMode::Watch)
    })
    .await;
    assert_eq!(stage.host.mode(), ShareMode::Watch);
    assert_eq!(stage.engine().now, [None, None]);

    // Whatever the guest sends now goes nowhere.
    let taken = stage.engine().taken.len();
    let datagram = guest.next(PadState {
        buttons: button::NORTH,
        ..PadState::NEUTRAL
    });
    let now = stage.now_us();
    assert!(stage.host.on_pad_datagram(now, &datagram).is_empty());
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(stage.engine().taken.len(), taken);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_stop_key_and_the_status_reach_the_service() {
    let mut stage = Stage::open(ShareMode::Watch).await;
    let status = ShareStatus {
        frames: 120,
        dropped: 2,
        audio_underruns: 0,
    };
    stage.game().send(&Message::ShareStatus(status));
    stage
        .pump_until(|s| s.events.contains(&Event::Status(status)))
        .await;
    stage.game().send(&Message::Override {
        action: OverrideAction::Stop,
    });
    stage
        .pump_until(|s| s.events.contains(&Event::StopRequested))
        .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn closing_leaves_the_game_with_a_neutral_pad_and_no_ring() {
    let mut stage = Stage::open(ShareMode::HandOver).await;
    let mut guest = PadSender::new();
    stage.guest_presses(&mut guest, button::SOUTH, 1).await;
    let steps = stage.host.close();
    stage.run(steps);
    let game = stage.game();
    eventually("the game to see the close", || game.share_engine().closed).await;
    assert_eq!(stage.engine().now, [None, None]);
    // Late datagrams of the guest are not played.
    let datagram = guest.next(PadState {
        buttons: button::SOUTH,
        ..PadState::NEUTRAL
    });
    let now = stage.now_us();
    assert!(stage.host.on_pad_datagram(now, &datagram).is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_game_that_goes_away_ends_the_share() {
    let mut stage = Stage::open(ShareMode::Together).await;
    drop(stage.game.take());
    stage.pump_until(|s| s.host.is_closed()).await;
    assert_eq!(stage.events.last(), Some(&Event::BridgeLost));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_second_share_while_one_runs_is_refused_by_the_game() {
    let stage = Stage::open(ShareMode::Watch).await;
    let ring2 = ring_source::create(&ring_name(), WIDTH, HEIGHT, 60)
        .unwrap()
        .2;
    let (mut second, steps) = ShareHost::open(0, &ring2, ShareMode::Watch);
    for step in steps {
        if let Step::Bridge(frame) = step {
            stage.frames.send(frame).unwrap();
        }
    }
    let mut stage = stage;
    let mut answered = Vec::new();
    while answered.is_empty() {
        let Ok(Some(BridgeInput::Frame { frame, .. })) =
            tokio::time::timeout(Duration::from_secs(5), stage.inputs.recv()).await
        else {
            panic!("an answer");
        };
        answered.extend(second.on_bridge(&frame));
    }
    assert_eq!(
        answered,
        [Step::Event(Event::OpenFailed(ShareResult::Running))]
    );
}
