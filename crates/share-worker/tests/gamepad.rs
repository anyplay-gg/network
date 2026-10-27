//! The guest's gamepad through the viewer's loop, with a fake pad: a press leaves as a pad state,
//! unplugging leaves a neutral one, and the keyboard keeps working with no pad at all.

use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyplay_share::codec::Frame;
use anyplay_share::link::{FromWorker, ToWorker};
use anyplay_share::pad::{PadState, button};
use anyplay_share_worker::gamepad::{GamepadSource, GuestPad, PadButton, RawPad};
use anyplay_share_worker::viewer::{self, FrameSink};
use anyplay_share_worker::{Port, decoder_for};

/// The pad that the test moves by hand.
#[derive(Clone, Default)]
struct Hand(Arc<Mutex<Option<RawPad>>>);

impl GamepadSource for Hand {
    fn poll(&mut self) -> Option<RawPad> {
        *self.0.lock().unwrap()
    }
}

/// A sink that reads the pad the way the window does: a keyboard state, merged with the gamepad.
struct PadSink {
    guest: GuestPad,
    keyboard: Arc<Mutex<PadState>>,
}

impl FrameSink for PadSink {
    fn show(&mut self, _frame: &Frame) {}
    fn pad(&mut self) -> Option<PadState> {
        let keyboard = *self.keyboard.lock().unwrap();
        Some(self.guest.read(keyboard))
    }
}

fn next_pad(
    from_viewer: &mpsc::Receiver<FromWorker>,
    until: impl Fn(&PadState) -> bool,
) -> PadState {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if let Ok(FromWorker::Pad(state)) = from_viewer.recv_timeout(Duration::from_millis(50))
            && until(&state)
        {
            return state;
        }
    }
    panic!("no matching pad state in 5 s");
}

#[test]
fn a_gamepad_drives_the_viewer_and_unplugging_makes_it_neutral() {
    let hand = Hand::default();
    let keyboard = Arc::new(Mutex::new(PadState::NEUTRAL));
    let (to_viewer, from_service) = mpsc::channel();
    let (to_service, from_viewer) = mpsc::channel();
    let (pad_hand, pad_keys) = (hand.clone(), keyboard.clone());
    let viewer = std::thread::spawn(move || {
        let sink = PadSink {
            guest: GuestPad::new(Box::new(pad_hand)),
            keyboard: pad_keys,
        };
        viewer::run(
            Box::new(decoder_for),
            Box::new(sink),
            Port {
                from_service,
                to_service,
            },
        );
    });
    // No pad at all: the first state is neutral, and the keyboard alone plays.
    assert_eq!(next_pad(&from_viewer, |_| true), PadState::NEUTRAL);
    keyboard.lock().unwrap().buttons = button::EAST;
    next_pad(&from_viewer, |s| s.buttons == button::EAST);
    // A pad is plugged in and a button pressed with a stick pushed: both leave, with the key.
    let mut raw = RawPad::new();
    raw.set(PadButton::South, true);
    raw.right = (1.0, 0.0);
    *hand.0.lock().unwrap() = Some(raw);
    let s = next_pad(&from_viewer, |s| s.buttons == button::EAST | button::SOUTH);
    assert_eq!(s.rx, 255);
    // Unplugged: the pad's part is gone at once, the key stays.
    *hand.0.lock().unwrap() = None;
    let s = next_pad(&from_viewer, |s| s.buttons == button::EAST);
    assert_eq!(s.rx, 128);
    *keyboard.lock().unwrap() = PadState::NEUTRAL;
    next_pad(&from_viewer, |s| *s == PadState::NEUTRAL);
    to_viewer.send(ToWorker::Stop).unwrap();
    viewer.join().unwrap();
}
