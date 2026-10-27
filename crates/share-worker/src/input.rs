//! The guest's pad, from the viewer's window: which keys make which buttons, and how often a
//! state may leave (`spec/share-play.md`, Input). A real gamepad is merged in by `crate::gamepad`.
//!
//! Only the keys listed in [`PadKey`] are read, and only while the viewer's window has the
//! focus. What leaves is a [`PadState`] and nothing else: no key code, no text, no mouse.
//!
//! | Pad | Keys |
//! |---|---|
//! | Left stick | W A S D |
//! | Right stick | I J K L |
//! | D-pad | the arrow keys |
//! | South, east, west, north | Z, X, C, V |
//! | L1, R1 | Q, E |
//! | L2, R2 | 1, 3 |
//! | L3, R3 | F, G |
//! | Start, select, pad click | Enter, Tab, Backspace |

use anyplay_share::pad::{MIDDLE, PadState, button};

/// A key that makes part of a pad state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PadKey {
    W,
    A,
    S,
    D,
    I,
    J,
    K,
    L,
    Up,
    Down,
    Left,
    Right,
    Z,
    X,
    C,
    V,
    Q,
    E,
    Num1,
    Num3,
    F,
    G,
    Enter,
    Tab,
    Backspace,
}

/// The state that the keys held down make.
pub fn pad_from_keys(down: impl Fn(PadKey) -> bool) -> PadState {
    let axis = |negative: PadKey, positive: PadKey| match (down(negative), down(positive)) {
        (true, false) => 0u8,
        (false, true) => 255,
        _ => MIDDLE,
    };
    let mut buttons = 0;
    for (key, bit) in [
        (PadKey::Up, button::DPAD_UP),
        (PadKey::Down, button::DPAD_DOWN),
        (PadKey::Left, button::DPAD_LEFT),
        (PadKey::Right, button::DPAD_RIGHT),
        (PadKey::Z, button::SOUTH),
        (PadKey::X, button::EAST),
        (PadKey::C, button::WEST),
        (PadKey::V, button::NORTH),
        (PadKey::Q, button::L1),
        (PadKey::E, button::R1),
        (PadKey::F, button::L3),
        (PadKey::G, button::R3),
        (PadKey::Enter, button::START),
        (PadKey::Tab, button::SELECT),
        (PadKey::Backspace, button::PAD_CLICK),
    ] {
        if down(key) {
            buttons |= bit;
        }
    }
    PadState {
        buttons,
        lx: axis(PadKey::A, PadKey::D),
        // Up on a stick is the low end, as on a pad.
        ly: axis(PadKey::W, PadKey::S),
        rx: axis(PadKey::J, PadKey::L),
        ry: axis(PadKey::I, PadKey::K),
        l2: if down(PadKey::Num1) { 255 } else { 0 },
        r2: if down(PadKey::Num3) { 255 } else { 0 },
    }
}

/// The fastest a changed state leaves: 250 a second.
pub const MIN_GAP_US: u64 = 4_000;
/// A state that did not change leaves this often, so that the host knows the viewer is alive.
pub const KEEP_ALIVE_US: u64 = 100_000;

/// Decides when a state leaves the viewer.
#[derive(Debug, Default)]
pub struct PadPacer {
    sent: Option<(u64, PadState)>,
}

impl PadPacer {
    pub fn new() -> PadPacer {
        PadPacer::default()
    }

    /// The state to send now, if one is due: a change at most every 4 ms, an unchanged state
    /// every 100 ms. The first state always goes.
    pub fn due(&mut self, now_us: u64, state: PadState) -> Option<PadState> {
        let send = match self.sent {
            None => true,
            Some((at, last)) => {
                let since = now_us.saturating_sub(at);
                (state != last && since >= MIN_GAP_US) || since >= KEEP_ALIVE_US
            }
        };
        if send {
            self.sent = Some((now_us, state));
            Some(state)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(held: &[PadKey]) -> PadState {
        pad_from_keys(|k| held.contains(&k))
    }

    #[test]
    fn no_key_is_the_neutral_pad() {
        assert_eq!(keys(&[]), PadState::NEUTRAL);
    }

    #[test]
    fn each_key_makes_its_part() {
        assert_eq!(keys(&[PadKey::Z]).buttons, button::SOUTH);
        assert_eq!(keys(&[PadKey::X]).buttons, button::EAST);
        assert_eq!(keys(&[PadKey::C]).buttons, button::WEST);
        assert_eq!(keys(&[PadKey::V]).buttons, button::NORTH);
        assert_eq!(keys(&[PadKey::Up]).buttons, button::DPAD_UP);
        assert_eq!(keys(&[PadKey::Right]).buttons, button::DPAD_RIGHT);
        assert_eq!(keys(&[PadKey::Enter]).buttons, button::START);
        assert_eq!(keys(&[PadKey::Tab]).buttons, button::SELECT);
        assert_eq!(
            keys(&[PadKey::Z, PadKey::Q]).buttons,
            button::SOUTH | button::L1
        );
        assert_eq!(keys(&[PadKey::Num1]).l2, 255);
        assert_eq!(keys(&[PadKey::Num3]).r2, 255);
        let s = keys(&[PadKey::A, PadKey::S, PadKey::L, PadKey::I]);
        assert_eq!((s.lx, s.ly, s.rx, s.ry), (0, 255, 255, 0));
    }

    #[test]
    fn opposite_keys_cancel_and_nothing_sets_a_reserved_bit() {
        let s = keys(&[PadKey::A, PadKey::D, PadKey::W, PadKey::S]);
        assert_eq!((s.lx, s.ly), (MIDDLE, MIDDLE));
        let all = [
            PadKey::W,
            PadKey::A,
            PadKey::S,
            PadKey::D,
            PadKey::I,
            PadKey::J,
            PadKey::K,
            PadKey::L,
            PadKey::Up,
            PadKey::Down,
            PadKey::Left,
            PadKey::Right,
            PadKey::Z,
            PadKey::X,
            PadKey::C,
            PadKey::V,
            PadKey::Q,
            PadKey::E,
            PadKey::Num1,
            PadKey::Num3,
            PadKey::F,
            PadKey::G,
            PadKey::Enter,
            PadKey::Tab,
            PadKey::Backspace,
        ];
        let everything = keys(&all);
        assert!(everything.is_valid());
        assert_eq!(everything.buttons.count_ones(), 15);
    }

    #[test]
    fn the_first_state_goes_and_a_change_waits_for_the_gap() {
        let mut pacer = PadPacer::new();
        let a = keys(&[PadKey::Z]);
        let b = keys(&[PadKey::X]);
        assert_eq!(pacer.due(0, a), Some(a));
        assert_eq!(pacer.due(1_000, b), None, "250 a second at most");
        assert_eq!(pacer.due(3_999, b), None);
        assert_eq!(pacer.due(4_000, b), Some(b));
    }

    #[test]
    fn an_unchanged_state_is_repeated_every_100_ms() {
        let mut pacer = PadPacer::new();
        let a = keys(&[PadKey::Z]);
        assert_eq!(pacer.due(0, a), Some(a));
        for t in (1_000..100_000).step_by(1_000) {
            assert_eq!(pacer.due(t, a), None, "{t}");
        }
        assert_eq!(pacer.due(100_000, a), Some(a));
        assert_eq!(pacer.due(150_000, a), None);
        assert_eq!(pacer.due(200_000, a), Some(a));
    }

    #[test]
    fn a_release_is_never_held_back_longer_than_the_gap() {
        let mut pacer = PadPacer::new();
        let held = keys(&[PadKey::Z]);
        pacer.due(0, held);
        assert_eq!(pacer.due(4_000, PadState::NEUTRAL), Some(PadState::NEUTRAL));
    }
}
