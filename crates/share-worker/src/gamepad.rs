//! The guest's real gamepad, beside the keyboard (`spec/share-play.md`, Input).
//!
//! Three small pieces, so that everything but the operating system's side can be tested:
//!
//! - [`RawPad`]: one snapshot of a gamepad, in the shape every pad API has (buttons held, two
//!   sticks from -1 to 1, two triggers from 0 to 1).
//! - [`pad_from_raw`] and [`merge`]: the pure maps from a snapshot to a [`PadState`], and the
//!   keyboard and the pad into one state.
//! - [`GamepadSource`]: the thin seam to the system. The real one is [`GilrsSource`] (gilrs,
//!   Apache-2.0 or MIT; WGI on Windows, evdev on Linux); a test drives the viewer with a fake.
//!
//! Only the pad's buttons, sticks and triggers are read. There is no home button, no touch and
//! no motion in a pad state, so none of them is read, and nothing but a [`PadState`] leaves.
//! The pad's click (a touchpad press) has no button in gilrs's list; it stays on the keyboard
//! (Backspace) until a backend reports it.
//!
//! The first connected gamepad is the guest's pad. A pad that is unplugged makes the pad part
//! neutral at once (the keyboard still plays); the next connected one takes over. When the
//! system has no gamepad backend, or none is connected, the viewer plays on the keyboard alone
//! and says nothing.
//!
//! Merging (keyboard and pad at the same time): buttons are OR'd; for each stick axis, the
//! value that is farther from the middle wins (the pad on a tie); for each trigger, the larger
//! value wins. So a pad at rest never hides the keyboard, and a held key never fights a stick.

use anyplay_share::pad::{MIDDLE, PadState, button};

/// A button of a gamepad, by its place on the pad and not by its printed name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PadButton {
    DpadUp,
    DpadDown,
    DpadLeft,
    DpadRight,
    South,
    East,
    West,
    North,
    L1,
    R1,
    L3,
    R3,
    Start,
    Select,
    PadClick,
}

impl PadButton {
    /// Every button, once.
    pub const ALL: [PadButton; 15] = [
        PadButton::DpadUp,
        PadButton::DpadDown,
        PadButton::DpadLeft,
        PadButton::DpadRight,
        PadButton::South,
        PadButton::East,
        PadButton::West,
        PadButton::North,
        PadButton::L1,
        PadButton::R1,
        PadButton::L3,
        PadButton::R3,
        PadButton::Start,
        PadButton::Select,
        PadButton::PadClick,
    ];

    /// The bit in [`PadState::buttons`].
    pub fn bit(self) -> u32 {
        match self {
            PadButton::DpadUp => button::DPAD_UP,
            PadButton::DpadDown => button::DPAD_DOWN,
            PadButton::DpadLeft => button::DPAD_LEFT,
            PadButton::DpadRight => button::DPAD_RIGHT,
            PadButton::South => button::SOUTH,
            PadButton::East => button::EAST,
            PadButton::West => button::WEST,
            PadButton::North => button::NORTH,
            PadButton::L1 => button::L1,
            PadButton::R1 => button::R1,
            PadButton::L3 => button::L3,
            PadButton::R3 => button::R3,
            PadButton::Start => button::START,
            PadButton::Select => button::SELECT,
            PadButton::PadClick => button::PAD_CLICK,
        }
    }
}

/// One snapshot of a gamepad. Sticks follow the pad APIs: x is right-positive and **y is
/// up-positive**, both from -1.0 to 1.0. Triggers are 0.0 (released) to 1.0 (pressed through).
/// Values outside the range, and NaN, are tolerated by [`pad_from_raw`].
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct RawPad {
    held: u32,
    pub left: (f32, f32),
    pub right: (f32, f32),
    pub l2: f32,
    pub r2: f32,
}

impl RawPad {
    /// A pad at rest.
    pub fn new() -> RawPad {
        RawPad::default()
    }

    /// Marks `which` as held (or not).
    pub fn set(&mut self, which: PadButton, held: bool) {
        if held {
            self.held |= which.bit();
        } else {
            self.held &= !which.bit();
        }
    }

    pub fn is_held(&self, which: PadButton) -> bool {
        self.held & which.bit() != 0
    }
}

/// A stick inside this radius is at rest: pads drift, and a drifting stick would walk a
/// character. Beyond it the range is stretched, so that the edge of the dead zone is 0.
pub const STICK_DEADZONE: f32 = 0.15;
/// A trigger below this is released.
pub const TRIGGER_DEADZONE: f32 = 0.05;

/// One stick axis in a pad state: 128 in the middle, 0 and 255 at the ends.
fn axis_byte(v: f32) -> u8 {
    let v = v.clamp(-1.0, 1.0);
    let scaled = if v >= 0.0 {
        f32::from(MIDDLE) + v * 127.0
    } else {
        f32::from(MIDDLE) + v * 128.0
    };
    scaled.round().clamp(0.0, 255.0) as u8
}

/// A stick with a round dead zone: (x, y up-positive) to (x, y) as pad bytes. Up is the low end
/// of y in a pad state, as on a pad.
fn stick_bytes((x, y): (f32, f32)) -> (u8, u8) {
    if !x.is_finite() || !y.is_finite() {
        return (MIDDLE, MIDDLE);
    }
    let length = x.hypot(y);
    if length <= STICK_DEADZONE {
        return (MIDDLE, MIDDLE);
    }
    // Stretch the live range to 0..1 and keep the direction. A pad's corners can read past 1.
    let stretched = ((length - STICK_DEADZONE) / (1.0 - STICK_DEADZONE)).min(1.0);
    let k = stretched / length;
    (axis_byte(x * k), axis_byte(-(y * k)))
}

fn trigger_byte(v: f32) -> u8 {
    if !v.is_finite() || v <= TRIGGER_DEADZONE {
        return 0;
    }
    let stretched = ((v - TRIGGER_DEADZONE) / (1.0 - TRIGGER_DEADZONE)).min(1.0);
    (stretched * 255.0).round() as u8
}

/// The pad state that a snapshot makes.
pub fn pad_from_raw(raw: &RawPad) -> PadState {
    let (lx, ly) = stick_bytes(raw.left);
    let (rx, ry) = stick_bytes(raw.right);
    PadState {
        buttons: raw.held & anyplay_share::pad::BUTTON_MASK,
        lx,
        ly,
        rx,
        ry,
        l2: trigger_byte(raw.l2),
        r2: trigger_byte(raw.r2),
    }
}

fn farther(keyboard: u8, pad: u8) -> u8 {
    if keyboard.abs_diff(MIDDLE) > pad.abs_diff(MIDDLE) {
        keyboard
    } else {
        pad
    }
}

/// The keyboard's state and the pad's into one (see the module docs for the rules).
pub fn merge(keyboard: PadState, pad: PadState) -> PadState {
    PadState {
        buttons: keyboard.buttons | pad.buttons,
        lx: farther(keyboard.lx, pad.lx),
        ly: farther(keyboard.ly, pad.ly),
        rx: farther(keyboard.rx, pad.rx),
        ry: farther(keyboard.ry, pad.ry),
        l2: keyboard.l2.max(pad.l2),
        r2: keyboard.r2.max(pad.r2),
    }
}

/// Where snapshots of the guest's gamepad come from.
pub trait GamepadSource {
    /// The first connected gamepad right now, or `None` when there is none (never connected,
    /// unplugged, or no backend on this system). It must not block: it is called from the
    /// viewer's loop, a few hundred times a second.
    fn poll(&mut self) -> Option<RawPad>;
}

/// A source with no gamepad at all.
pub struct NoGamepad;

impl GamepadSource for NoGamepad {
    fn poll(&mut self) -> Option<RawPad> {
        None
    }
}

/// The guest's pad: the keyboard's state, with the gamepad merged in.
pub struct GuestPad {
    source: Box<dyn GamepadSource>,
}

impl GuestPad {
    pub fn new(source: Box<dyn GamepadSource>) -> GuestPad {
        GuestPad { source }
    }

    /// The system's gamepad where this build has one, otherwise the keyboard alone.
    pub fn system() -> GuestPad {
        #[cfg(any(windows, target_os = "linux"))]
        {
            GuestPad::new(Box::new(GilrsSource::new()))
        }
        #[cfg(not(any(windows, target_os = "linux")))]
        {
            GuestPad::new(Box::new(NoGamepad))
        }
    }

    /// The state to send: `keyboard` merged with the first gamepad, if there is one.
    pub fn read(&mut self, keyboard: PadState) -> PadState {
        match self.source.poll() {
            Some(raw) => merge(keyboard, pad_from_raw(&raw)),
            None => keyboard,
        }
    }
}

#[cfg(any(windows, target_os = "linux"))]
pub use system::GilrsSource;

#[cfg(any(windows, target_os = "linux"))]
mod system {
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::time::{Duration, Instant};

    use gilrs::{Axis, Button, GamepadId, Gilrs};

    use super::{GamepadSource, PadButton, RawPad};

    /// After a failed start the backend is tried again this often (a pad system that comes up
    /// late, say). Starting it can take a moment, so not on every poll.
    const RETRY: Duration = Duration::from_secs(5);
    /// Events handled per poll at most, so that a flood cannot hold the viewer's loop.
    const EVENTS_PER_POLL: usize = 256;

    enum Backend {
        NotStarted,
        Ready(Box<Gilrs>),
        Failed(Instant),
    }

    /// The system's gamepads through gilrs. Starts on the first poll, never panics into the
    /// viewer, and reads the first connected pad until it is unplugged.
    pub struct GilrsSource {
        backend: Backend,
        active: Option<GamepadId>,
    }

    impl GilrsSource {
        pub fn new() -> GilrsSource {
            GilrsSource {
                backend: Backend::NotStarted,
                active: None,
            }
        }

        fn start() -> Option<Gilrs> {
            catch_unwind(AssertUnwindSafe(|| Gilrs::new().ok()))
                .ok()
                .flatten()
        }
    }

    impl Default for GilrsSource {
        fn default() -> Self {
            GilrsSource::new()
        }
    }

    /// The place on the pad that a gilrs button stands for. Home (`Mode`), the extra buttons and
    /// unknown ones are not part of a pad state and map to nothing. The second triggers are
    /// values, not buttons, and are read apart.
    pub fn map_button(button: Button) -> Option<PadButton> {
        Some(match button {
            Button::DPadUp => PadButton::DpadUp,
            Button::DPadDown => PadButton::DpadDown,
            Button::DPadLeft => PadButton::DpadLeft,
            Button::DPadRight => PadButton::DpadRight,
            Button::South => PadButton::South,
            Button::East => PadButton::East,
            Button::West => PadButton::West,
            Button::North => PadButton::North,
            Button::LeftTrigger => PadButton::L1,
            Button::RightTrigger => PadButton::R1,
            Button::LeftThumb => PadButton::L3,
            Button::RightThumb => PadButton::R3,
            Button::Start => PadButton::Start,
            Button::Select => PadButton::Select,
            _ => return None,
        })
    }

    const MAPPED: [Button; 14] = [
        Button::DPadUp,
        Button::DPadDown,
        Button::DPadLeft,
        Button::DPadRight,
        Button::South,
        Button::East,
        Button::West,
        Button::North,
        Button::LeftTrigger,
        Button::RightTrigger,
        Button::LeftThumb,
        Button::RightThumb,
        Button::Start,
        Button::Select,
    ];

    /// A snapshot from what gilrs says about one pad: whether a button is held, the value of an
    /// axis (sticks and d-pad axes from -1 to 1, y up-positive: gilrs flips the reversed y of
    /// evdev and of WGI) and the value of a button that has one (`None` for a digital button).
    /// Pure, so that the table in the tests can stand in for the pads gilrs knows.
    pub fn raw_from_gilrs(
        pressed: impl Fn(Button) -> bool,
        axis: impl Fn(Axis) -> f32,
        value: impl Fn(Button) -> Option<f32>,
    ) -> RawPad {
        let mut raw = RawPad::new();
        for button in MAPPED {
            if let Some(which) = map_button(button)
                && pressed(button)
            {
                raw.set(which, true);
            }
        }
        // Some pads report the d-pad as two axes.
        let (dx, dy) = (axis(Axis::DPadX), axis(Axis::DPadY));
        for (held, which) in [
            (dx < -0.5, PadButton::DpadLeft),
            (dx > 0.5, PadButton::DpadRight),
            (dy > 0.5, PadButton::DpadUp),
            (dy < -0.5, PadButton::DpadDown),
        ] {
            if held {
                raw.set(which, true);
            }
        }
        raw.left = (axis(Axis::LeftStickX), axis(Axis::LeftStickY));
        raw.right = (axis(Axis::RightStickX), axis(Axis::RightStickY));
        // A trigger is a button with a value; a digital one reads 0 or 1. `LeftZ` and `RightZ`
        // are not triggers (SDL's "leftz" is a third stick axis), so they are not read.
        let trigger =
            |button: Button| value(button).unwrap_or(if pressed(button) { 1.0 } else { 0.0 });
        raw.l2 = trigger(Button::LeftTrigger2);
        raw.r2 = trigger(Button::RightTrigger2);
        raw
    }

    impl GamepadSource for GilrsSource {
        fn poll(&mut self) -> Option<RawPad> {
            if matches!(self.backend, Backend::NotStarted)
                || matches!(self.backend, Backend::Failed(at) if at.elapsed() >= RETRY)
            {
                self.backend = match GilrsSource::start() {
                    Some(gilrs) => Backend::Ready(Box::new(gilrs)),
                    None => Backend::Failed(Instant::now()),
                };
            }
            let Backend::Ready(gilrs) = &mut self.backend else {
                return None;
            };
            // Events keep gilrs's picture of the pads current (connect, disconnect, values).
            for _ in 0..EVENTS_PER_POLL {
                if gilrs.next_event().is_none() {
                    break;
                }
            }
            if self
                .active
                .is_none_or(|id| gilrs.connected_gamepad(id).is_none())
            {
                self.active = gilrs
                    .gamepads()
                    .find(|(_, g)| g.is_connected())
                    .map(|(id, _)| id);
            }
            let pad = gilrs.connected_gamepad(self.active?)?;
            Some(raw_from_gilrs(
                |button| pad.is_pressed(button),
                |axis| pad.value(axis),
                |button| pad.button_data(button).map(|d| d.value()),
            ))
        }
    }

    #[cfg(test)]
    mod tests {
        use anyplay_share::pad::{PadState, button as bit};

        use super::super::pad_from_raw;
        use super::*;

        /// What gilrs reports for one pad, as the three questions `raw_from_gilrs` asks.
        #[derive(Default)]
        struct FakePad {
            pressed: Vec<Button>,
            axes: Vec<(Axis, f32)>,
            values: Vec<(Button, f32)>,
        }

        impl FakePad {
            fn with_axes(axes: &[(Axis, f32)]) -> FakePad {
                FakePad {
                    axes: axes.to_vec(),
                    ..FakePad::default()
                }
            }

            fn with_trigger(value: f32) -> FakePad {
                FakePad {
                    values: vec![(Button::LeftTrigger2, value)],
                    ..FakePad::default()
                }
            }

            fn state(&self) -> PadState {
                pad_from_raw(&raw_from_gilrs(
                    |b| self.pressed.contains(&b),
                    |a| {
                        let found = self.axes.iter().find(|(x, _)| *x == a);
                        found.map_or(0.0, |(_, v)| *v)
                    },
                    |b| self.values.iter().find(|(x, _)| *x == b).map(|(_, v)| *v),
                ))
            }
        }

        /// How a family of pads reaches gilrs. gilrs maps every pad onto one standard layout by
        /// position through SDL mappings (`South` is the bottom face button, whatever it is
        /// printed: A on an Xbox pad, Cross on a DualShock 4 or DualSense, B on a Switch Pro
        /// pad), so the names are the same everywhere. What differs is how a pad delivers the
        /// d-pad and the second triggers.
        #[derive(Clone, Copy, Debug)]
        enum Family {
            /// WGI: d-pad as buttons, analog triggers with a value.
            Xbox,
            /// d-pad as a hat (axes), analog triggers with a value.
            DualSense,
            /// d-pad as buttons, digital second triggers (no value).
            SwitchPro,
        }

        const FAMILIES: [Family; 3] = [Family::Xbox, Family::DualSense, Family::SwitchPro];

        impl Family {
            /// The pad with the button at `position` held down.
            fn press(self, position: Button) -> FakePad {
                let mut pad = FakePad::default();
                match (self, position) {
                    (Family::DualSense, Button::DPadUp) => pad.axes.push((Axis::DPadY, 1.0)),
                    (Family::DualSense, Button::DPadDown) => pad.axes.push((Axis::DPadY, -1.0)),
                    (Family::DualSense, Button::DPadLeft) => pad.axes.push((Axis::DPadX, -1.0)),
                    (Family::DualSense, Button::DPadRight) => pad.axes.push((Axis::DPadX, 1.0)),
                    (_, Button::LeftTrigger2 | Button::RightTrigger2) => {
                        pad.pressed.push(position);
                        if !matches!(self, Family::SwitchPro) {
                            pad.values.push((position, 1.0));
                        }
                    }
                    _ => pad.pressed.push(position),
                }
                pad
            }
        }

        #[test]
        fn equal_positions_on_every_pad_make_the_same_bits() {
            // Position on gilrs's standard layout, and the bit it must make.
            let table = [
                (Button::South, bit::SOUTH),
                (Button::East, bit::EAST),
                (Button::West, bit::WEST),
                (Button::North, bit::NORTH),
                (Button::LeftTrigger, bit::L1),
                (Button::RightTrigger, bit::R1),
                (Button::LeftThumb, bit::L3),
                (Button::RightThumb, bit::R3),
                (Button::Start, bit::START),
                (Button::Select, bit::SELECT),
                (Button::DPadUp, bit::DPAD_UP),
                (Button::DPadDown, bit::DPAD_DOWN),
                (Button::DPadLeft, bit::DPAD_LEFT),
                (Button::DPadRight, bit::DPAD_RIGHT),
                // The second triggers are values, not buttons.
                (Button::LeftTrigger2, 0),
                (Button::RightTrigger2, 0),
                // Home and the extra buttons are no part of a pad state.
                (Button::Mode, 0),
                (Button::C, 0),
                (Button::Z, 0),
            ];
            for family in FAMILIES {
                for (position, want) in table {
                    let got = family.press(position).state();
                    assert_eq!(got.buttons, want, "{family:?} {position:?}");
                    assert!(got.is_valid());
                }
            }
        }

        #[test]
        fn equal_triggers_on_every_pad_make_the_same_values() {
            for family in FAMILIES {
                let left = family.press(Button::LeftTrigger2).state();
                let right = family.press(Button::RightTrigger2).state();
                assert_eq!((left.l2, left.r2), (255, 0), "{family:?}");
                assert_eq!((right.l2, right.r2), (0, 255), "{family:?}");
            }
            // An analog trigger runs through its range; a released one is zero.
            assert_eq!(FakePad::with_trigger(0.0).state().l2, 0);
            assert_eq!(FakePad::with_trigger(0.04).state().l2, 0);
            assert!((115..=135).contains(&FakePad::with_trigger(0.5).state().l2));
            assert_eq!(FakePad::with_trigger(1.0).state().l2, 255);
            // `LeftZ` and `RightZ` are not triggers.
            let z = FakePad::with_axes(&[(Axis::LeftZ, 1.0), (Axis::RightZ, 1.0)]);
            assert_eq!(z.state(), PadState::NEUTRAL);
        }

        #[test]
        fn sticks_read_the_same_on_every_pad() {
            // gilrs gives y up-positive on every backend; a pad state has up at the low end.
            let stick = |lx: f32, ly: f32, rx: f32, ry: f32| {
                let pad = FakePad::with_axes(&[
                    (Axis::LeftStickX, lx),
                    (Axis::LeftStickY, ly),
                    (Axis::RightStickX, rx),
                    (Axis::RightStickY, ry),
                ]);
                let s = pad.state();
                assert_eq!(s.buttons, 0);
                (s.lx, s.ly, s.rx, s.ry)
            };
            assert_eq!(stick(0.0, 0.0, 0.0, 0.0), (128, 128, 128, 128), "rest");
            assert_eq!(stick(0.0, 1.0, 0.0, 0.0), (128, 0, 128, 128), "left up");
            assert_eq!(stick(1.0, 0.0, 0.0, 0.0), (255, 128, 128, 128), "left right");
            assert_eq!(stick(0.0, 0.0, 0.0, -1.0), (128, 128, 128, 255), "right down");
            assert_eq!(stick(0.0, 0.0, -1.0, 0.0), (128, 128, 0, 128), "right left");
            assert_eq!(stick(0.1, -0.1, 0.1, 0.1), (128, 128, 128, 128), "dead zone");
        }

        #[test]
        fn the_dpad_as_axes_and_as_buttons_agree() {
            let hat = |x: f32, y: f32| {
                FakePad::with_axes(&[(Axis::DPadX, x), (Axis::DPadY, y)])
                    .state()
                    .buttons
            };
            assert_eq!(hat(0.0, 0.0), 0);
            assert_eq!(hat(0.0, 0.4), 0, "half a hat is rest");
            // Up is positive on the hat, as on a stick.
            assert_eq!(hat(0.0, 1.0), bit::DPAD_UP);
            assert_eq!(hat(-1.0, -1.0), bit::DPAD_LEFT | bit::DPAD_DOWN);
            // Buttons and axes from one pad at once still make one bit.
            let mut both = FakePad::with_axes(&[(Axis::DPadY, 1.0)]);
            both.pressed.push(Button::DPadUp);
            assert_eq!(both.state().buttons, bit::DPAD_UP);
        }

        #[test]
        fn every_pad_button_has_one_gilrs_button_and_home_has_none() {
            let mut seen: Vec<PadButton> = MAPPED.iter().filter_map(|b| map_button(*b)).collect();
            assert_eq!(seen.len(), 14);
            seen.sort_by_key(|b| b.bit());
            seen.dedup();
            assert_eq!(seen.len(), 14, "no two buttons share a place");
            // Pad click has no gilrs button: it comes from the keyboard (Backspace).
            assert!(!seen.contains(&PadButton::PadClick));
            assert_eq!(map_button(Button::Mode), None);
            assert_eq!(map_button(Button::C), None);
            assert_eq!(map_button(Button::Unknown), None);
        }

        #[test]
        fn a_system_without_a_backend_or_pad_degrades_to_nothing() {
            // Headless CI: no pad is connected (or no backend starts); this must not panic
            // or block, and the answer is "no gamepad".
            let mut source = GilrsSource::new();
            for _ in 0..3 {
                let _ = source.poll();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw_with(buttons: &[PadButton]) -> RawPad {
        let mut raw = RawPad::new();
        for b in buttons {
            raw.set(*b, true);
        }
        raw
    }

    fn left(x: f32, y: f32) -> PadState {
        let mut raw = RawPad::new();
        raw.left = (x, y);
        pad_from_raw(&raw)
    }

    #[test]
    fn a_pad_at_rest_is_the_neutral_state() {
        assert_eq!(pad_from_raw(&RawPad::new()), PadState::NEUTRAL);
    }

    #[test]
    fn every_button_makes_exactly_its_bit() {
        let mut all = 0;
        for which in PadButton::ALL {
            let state = pad_from_raw(&raw_with(&[which]));
            assert_eq!(state.buttons, which.bit(), "{which:?}");
            assert_eq!(state.buttons.count_ones(), 1);
            assert!(state.is_valid());
            all |= state.buttons;
        }
        assert_eq!(
            all,
            anyplay_share::pad::BUTTON_MASK,
            "all 15, no reserved bit"
        );
        let everything = pad_from_raw(&raw_with(&PadButton::ALL));
        assert_eq!(everything.buttons, anyplay_share::pad::BUTTON_MASK);
    }

    #[test]
    fn a_button_can_be_set_and_cleared() {
        let mut raw = raw_with(&[PadButton::South, PadButton::Start]);
        raw.set(PadButton::South, false);
        assert!(!raw.is_held(PadButton::South));
        assert!(raw.is_held(PadButton::Start));
    }

    #[test]
    fn sticks_cover_the_whole_range_with_128_in_the_middle() {
        assert_eq!((left(-1.0, 0.0).lx, left(1.0, 0.0).lx), (0, 255));
        assert_eq!(left(0.0, 0.0).lx, 128);
        // Up on a stick is the low end of y in a pad state, down the high end.
        assert_eq!((left(0.0, 1.0).ly, left(0.0, -1.0).ly), (0, 255));
        // The right stick is read on its own.
        let mut raw = RawPad::new();
        raw.right = (1.0, 1.0);
        let s = pad_from_raw(&raw);
        assert!(s.rx > 200 && s.ry < 60, "{} {}", s.rx, s.ry);
        assert_eq!((s.lx, s.ly), (128, 128));
    }

    #[test]
    fn the_dead_zone_is_round_and_the_live_range_starts_at_zero() {
        assert_eq!(left(0.14, 0.0).lx, 128, "inside");
        assert_eq!(left(0.1, 0.1).lx, 128, "the diagonal is inside (0.141)");
        assert_eq!(left(0.0, -0.15).ly, 128, "on the edge");
        let just_out = left(0.2, 0.0).lx;
        assert!(just_out > 128 && just_out < 150, "{just_out}");
        // Monotonic over the live range.
        let mut last = 128;
        for i in 15..=100 {
            let v = left(i as f32 / 100.0, 0.0).lx;
            assert!(v >= last, "{i}");
            last = v;
        }
        assert_eq!(last, 255);
    }

    #[test]
    fn out_of_range_and_broken_values_are_clamped_or_neutral() {
        assert_eq!(left(5.0, 0.0).lx, 255);
        assert_eq!(left(0.0, -7.0).ly, 255);
        assert_eq!(left(-99.0, 0.0).lx, 0);
        let mut raw = RawPad::new();
        raw.right = (f32::NAN, 0.5);
        raw.l2 = 3.0;
        raw.r2 = f32::INFINITY;
        let s = pad_from_raw(&raw);
        assert_eq!((s.rx, s.ry), (128, 128), "NaN is neutral");
        assert_eq!(s.l2, 255);
        assert_eq!(s.r2, 0, "infinity is not a trigger value");
        assert!(s.is_valid());
        // A corner of a square-gated pad reads past 1 on the diagonal; the direction is kept.
        let s = left(1.0, 1.0);
        assert!(s.lx > 200 && s.ly < 60, "{} {}", s.lx, s.ly);
    }

    #[test]
    fn triggers_run_from_zero_to_255_with_a_small_dead_zone() {
        let trigger = |v: f32| {
            let mut raw = RawPad::new();
            raw.l2 = v;
            raw.r2 = v;
            let s = pad_from_raw(&raw);
            assert_eq!(s.l2, s.r2);
            s.l2
        };
        assert_eq!(trigger(0.0), 0);
        assert_eq!(trigger(0.04), 0);
        assert_eq!(trigger(1.0), 255);
        assert_eq!(trigger(-1.0), 0);
        let half = trigger(0.5);
        assert!((115..=135).contains(&half), "{half}");
        // Triggers touch no button bit: L2 and R2 are values, not buttons.
        let mut raw = RawPad::new();
        raw.l2 = 1.0;
        assert_eq!(pad_from_raw(&raw).buttons, 0);
    }

    #[test]
    fn everything_at_once_is_still_a_valid_state() {
        let mut raw = raw_with(&PadButton::ALL);
        raw.left = (1.0, -1.0);
        raw.right = (-1.0, 1.0);
        raw.l2 = 1.0;
        raw.r2 = 1.0;
        assert!(pad_from_raw(&raw).is_valid());
    }

    fn keys(buttons: u32, lx: u8, l2: u8) -> PadState {
        PadState {
            buttons,
            lx,
            l2,
            ..PadState::NEUTRAL
        }
    }

    #[test]
    fn merging_ors_buttons_and_takes_the_farther_stick_and_the_larger_trigger() {
        let keyboard = keys(button::SOUTH, 0, 255);
        let mut raw = RawPad::new();
        raw.set(PadButton::East, true);
        raw.left = (0.5, 0.0);
        raw.l2 = 0.5;
        let merged = merge(keyboard, pad_from_raw(&raw));
        assert_eq!(merged.buttons, button::SOUTH | button::EAST);
        assert_eq!(merged.lx, 0, "the key is farther than half a stick");
        assert_eq!(merged.l2, 255);
        assert!(merged.is_valid());
        // A stick that is farther than the key wins.
        assert_eq!(merge(keys(0, 100, 0), keys(0, 255, 0)).lx, 255);
        // On a tie the pad wins, and a resting pad never hides a key.
        assert_eq!(merge(keys(0, 0, 0), keys(0, 0, 0)).lx, 0);
        assert_eq!(merge(keys(0, 0, 0), PadState::NEUTRAL).lx, 0);
        assert_eq!(
            merge(PadState::NEUTRAL, PadState::NEUTRAL),
            PadState::NEUTRAL
        );
    }

    /// Plays the given snapshots, one per poll, then reports no pad.
    pub struct Fake(pub Vec<Option<RawPad>>);

    impl GamepadSource for Fake {
        fn poll(&mut self) -> Option<RawPad> {
            if self.0.is_empty() {
                None
            } else {
                self.0.remove(0)
            }
        }
    }

    #[test]
    fn unplugging_leaves_the_keyboard_alone_and_neutral_without_keys() {
        let pressed = raw_with(&[PadButton::South]);
        let mut pad = GuestPad::new(Box::new(Fake(vec![Some(pressed), None, None])));
        let neutral = PadState::NEUTRAL;
        assert_eq!(pad.read(neutral).buttons, button::SOUTH);
        assert_eq!(pad.read(neutral), PadState::NEUTRAL, "unplugged is neutral");
        assert_eq!(pad.read(keys(button::EAST, 128, 0)).buttons, button::EAST);
    }

    #[test]
    fn no_gamepad_source_is_the_keyboard_alone() {
        let mut pad = GuestPad::new(Box::new(NoGamepad));
        let k = keys(button::START, 255, 7);
        assert_eq!(pad.read(k), k);
    }

    #[test]
    fn the_system_pad_never_fails_without_a_device() {
        // With no pad connected (CI) this is the keyboard alone.
        let mut pad = GuestPad::system();
        let k = keys(button::START, 0, 0);
        let got = pad.read(k);
        assert_eq!(got.buttons & button::START, button::START);
    }
}
