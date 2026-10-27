//! The guest's pad (`spec/share-play.md`, Input): the pad state, the pad datagram, the guest's
//! sender that repeats older states, and the host's gate that applies the rules before a state
//! may reach the game. Pure: time comes in as microseconds from the caller's monotonic clock.
//!
//! Nothing but a pad state can be sent: a fixed list of buttons, two sticks and two triggers.
//! There is no home button, touch, motion, keyboard or mouse, so there is nothing in this
//! module that could reach the host's computer.

use std::collections::VecDeque;
use std::fmt;

use crate::wire::VERSION;

/// The datagram type of a pad datagram.
pub const TYPE_PAD: u8 = 4;
/// Bytes in front of the states.
pub const HEADER_LEN: usize = 5;
/// Bytes of one state on the wire.
pub const STATE_LEN: usize = 10;
/// States in one datagram, at most: the newest and the two before it.
pub const MAX_STATES: usize = 3;
/// The bits of `buttons` that mean something; bits 15 to 31 must be 0.
pub const BUTTON_MASK: u32 = 0x7FFF;
/// The middle of a stick.
pub const MIDDLE: u8 = 128;

/// States accepted per second.
pub const RATE_PER_SECOND: u64 = 300;
/// States accepted in one burst.
pub const BURST: u64 = 30;
/// Invalid datagrams in [`INVALID_WINDOW_US`] that end the connection (close code 7).
pub const INVALID_LIMIT: usize = 20;
/// The window in which invalid datagrams are counted: 5 seconds.
pub const INVALID_WINDOW_US: u64 = 5_000_000;
/// Silence after which the game gets a neutral state: 250 ms.
pub const SILENCE_US: u64 = 250_000;

/// The names of the bits of [`PadState::buttons`].
pub mod button {
    pub const DPAD_UP: u32 = 1 << 0;
    pub const DPAD_DOWN: u32 = 1 << 1;
    pub const DPAD_LEFT: u32 = 1 << 2;
    pub const DPAD_RIGHT: u32 = 1 << 3;
    pub const SOUTH: u32 = 1 << 4;
    pub const EAST: u32 = 1 << 5;
    pub const WEST: u32 = 1 << 6;
    pub const NORTH: u32 = 1 << 7;
    pub const L1: u32 = 1 << 8;
    pub const R1: u32 = 1 << 9;
    pub const L3: u32 = 1 << 10;
    pub const R3: u32 = 1 << 11;
    pub const START: u32 = 1 << 12;
    pub const SELECT: u32 = 1 << 13;
    pub const PAD_CLICK: u32 = 1 << 14;
}

/// One state of a pad, complete and not a change.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PadState {
    pub buttons: u32,
    pub lx: u8,
    pub ly: u8,
    pub rx: u8,
    pub ry: u8,
    pub l2: u8,
    pub r2: u8,
}

impl PadState {
    /// No buttons, sticks in the middle, triggers up.
    pub const NEUTRAL: PadState = PadState {
        buttons: 0,
        lx: MIDDLE,
        ly: MIDDLE,
        rx: MIDDLE,
        ry: MIDDLE,
        l2: 0,
        r2: 0,
    };

    /// True when no reserved bit is set.
    pub fn is_valid(&self) -> bool {
        self.buttons & !BUTTON_MASK == 0
    }

    /// The state as the link carries it between the worker and the net service, and as the
    /// datagram carries it: `buttons` big-endian, then the six bytes.
    pub fn to_bytes(&self) -> [u8; STATE_LEN] {
        let b = self.buttons.to_be_bytes();
        [
            b[0], b[1], b[2], b[3], self.lx, self.ly, self.rx, self.ry, self.l2, self.r2,
        ]
    }

    /// Reads one state; `None` for a slice that is not exactly [`STATE_LEN`] bytes. A reserved
    /// bit is not checked here: [`PadState::is_valid`] says.
    pub fn from_bytes(b: &[u8]) -> Option<PadState> {
        let b: &[u8; STATE_LEN] = b.try_into().ok()?;
        Some(PadState {
            buttons: u32::from_be_bytes([b[0], b[1], b[2], b[3]]),
            lx: b[4],
            ly: b[5],
            rx: b[6],
            ry: b[7],
            l2: b[8],
            r2: b[9],
        })
    }
}

impl Default for PadState {
    fn default() -> Self {
        PadState::NEUTRAL
    }
}

/// Why a pad datagram is refused; each is one rule of the spec.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PadRefusal {
    /// Shorter than the header, or the length is not the header plus `count` states.
    Length,
    /// Another version than 0.
    Version(u8),
    /// Not a pad datagram.
    Type(u8),
    /// `count` is 0 or above 3.
    Count(u8),
    /// A reserved button bit is set.
    ReservedButtons,
}

impl fmt::Display for PadRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PadRefusal::Length => write!(f, "pad datagram of the wrong length"),
            PadRefusal::Version(v) => write!(f, "pad datagram version {v} is not supported"),
            PadRefusal::Type(t) => write!(f, "datagram type {t} is not a pad"),
            PadRefusal::Count(c) => write!(f, "{c} states in a pad datagram"),
            PadRefusal::ReservedButtons => write!(f, "pad datagram sets reserved buttons"),
        }
    }
}

impl std::error::Error for PadRefusal {}

/// A pad datagram: the sequence of its newest state and up to three states, newest first.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PadDatagram {
    pub sequence: u16,
    pub states: Vec<PadState>,
}

impl PadDatagram {
    /// Reads a pad datagram, exactly: the version, the type, 1 to 3 states, the exact length
    /// and no reserved button bit.
    pub fn parse(b: &[u8]) -> Result<PadDatagram, PadRefusal> {
        if b.len() < HEADER_LEN {
            return Err(PadRefusal::Length);
        }
        if b[0] != VERSION {
            return Err(PadRefusal::Version(b[0]));
        }
        if b[1] != TYPE_PAD {
            return Err(PadRefusal::Type(b[1]));
        }
        let count = b[4];
        if count == 0 || usize::from(count) > MAX_STATES {
            return Err(PadRefusal::Count(count));
        }
        if b.len() != HEADER_LEN + STATE_LEN * usize::from(count) {
            return Err(PadRefusal::Length);
        }
        let mut states = Vec::with_capacity(usize::from(count));
        for chunk in b[HEADER_LEN..].as_chunks::<STATE_LEN>().0 {
            let state = PadState::from_bytes(chunk).ok_or(PadRefusal::Length)?;
            if !state.is_valid() {
                return Err(PadRefusal::ReservedButtons);
            }
            states.push(state);
        }
        Ok(PadDatagram {
            sequence: u16::from_be_bytes([b[2], b[3]]),
            states,
        })
    }

    /// Writes the datagram. States beyond [`MAX_STATES`] are not written, and an empty list
    /// writes nothing (`None`), as does a state with a reserved bit.
    pub fn encode(&self) -> Option<Vec<u8>> {
        if self.states.is_empty() || self.states.iter().any(|s| !s.is_valid()) {
            return None;
        }
        let states = &self.states[..self.states.len().min(MAX_STATES)];
        let mut out = Vec::with_capacity(HEADER_LEN + STATE_LEN * states.len());
        out.push(VERSION);
        out.push(TYPE_PAD);
        out.extend_from_slice(&self.sequence.to_be_bytes());
        out.push(states.len() as u8);
        for state in states {
            out.extend_from_slice(&state.to_bytes());
        }
        Some(out)
    }
}

/// The guest's side: numbers the states and repeats the two before the newest in every
/// datagram, so that one or two lost datagrams cost nothing.
#[derive(Debug, Default)]
pub struct PadSender {
    sequence: u16,
    started: bool,
    previous: VecDeque<PadState>,
}

impl PadSender {
    pub fn new() -> PadSender {
        PadSender::default()
    }

    /// The sequence number of the newest datagram `next` made.
    pub fn sequence(&self) -> u16 {
        self.sequence
    }

    /// The datagram for the next state. A state with a reserved bit is cleaned of it, so the
    /// host never gets a reason to refuse what an honest viewer sends.
    pub fn next(&mut self, state: PadState) -> Vec<u8> {
        let state = PadState {
            buttons: state.buttons & BUTTON_MASK,
            ..state
        };
        if self.started {
            self.sequence = self.sequence.wrapping_add(1);
        }
        self.started = true;
        let mut states = vec![state];
        states.extend(self.previous.iter().copied());
        self.previous.push_front(state);
        self.previous.truncate(MAX_STATES - 1);
        PadDatagram {
            sequence: self.sequence,
            states,
        }
        .encode()
        .expect("a cleaned state is valid and the list is not empty")
    }
}

/// What the host's gate lets through when the mode allows pad input.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PadMode {
    /// No pad input is applied.
    Watch,
    /// The guest's state replaces the host's pad 1.
    HandOver,
    /// The guest's state is a second pad.
    Together,
}

impl PadMode {
    /// The pad the guest plays on: 0 for pad 1, 1 for pad 2, `None` in `watch`.
    pub fn slot(self) -> Option<u8> {
        match self {
            PadMode::Watch => None,
            PadMode::HandOver => Some(0),
            PadMode::Together => Some(1),
        }
    }
}

/// Why a datagram changed nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dropped {
    /// The mode is `watch`: dropped without a reply, and not counted as invalid.
    Watch,
    /// Its newest state is at or behind the newest one seen.
    Stale,
    /// More than 300 states a second, or a burst above 30.
    TooFast,
    /// It did not parse; counted towards the close.
    Invalid(PadRefusal),
}

/// What one datagram did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Gated {
    /// The states to apply, oldest first, with their sequence numbers.
    pub apply: Vec<(u16, PadState)>,
    /// Why nothing (or fewer states) was applied.
    pub dropped: Option<Dropped>,
    /// More than 20 invalid datagrams in 5 seconds: the connection ends with code 7.
    pub close: bool,
}

impl Gated {
    fn nothing(dropped: Dropped, close: bool) -> Gated {
        Gated {
            apply: Vec::new(),
            dropped: Some(dropped),
            close,
        }
    }
}

/// The host's rules for pad datagrams (`spec/share-play.md`, Rules the host's net service
/// applies). One gate belongs to one share.
#[derive(Debug)]
pub struct PadGate {
    mode: PadMode,
    newest: Option<u16>,
    /// Tokens in the bucket, in thousandths of a state.
    tokens: u64,
    refilled_us: u64,
    invalid: VecDeque<u64>,
    last_valid_us: Option<u64>,
    neutral_sent: bool,
}

impl PadGate {
    /// A gate in `watch` mode.
    pub fn new(now_us: u64) -> PadGate {
        PadGate {
            mode: PadMode::Watch,
            newest: None,
            tokens: BURST * 1_000,
            refilled_us: now_us,
            invalid: VecDeque::new(),
            last_valid_us: None,
            neutral_sent: false,
        }
    }

    pub fn mode(&self) -> PadMode {
        self.mode
    }

    /// Changes the mode. Whatever the guest held is forgotten with the old mode: the caller
    /// sends the game a neutral state on the old pad, because the engine removes it.
    pub fn set_mode(&mut self, mode: PadMode) {
        if mode == self.mode {
            return;
        }
        self.mode = mode;
        self.newest = None;
        self.last_valid_us = None;
        self.neutral_sent = false;
    }

    fn refill(&mut self, now_us: u64) {
        let elapsed = now_us.saturating_sub(self.refilled_us);
        self.refilled_us = now_us.max(self.refilled_us);
        // RATE_PER_SECOND states a second is RATE_PER_SECOND thousandths a millisecond, that
        // is RATE_PER_SECOND thousandths per 1000 microseconds.
        let added = elapsed.saturating_mul(RATE_PER_SECOND) / 1_000;
        self.tokens = (self.tokens + added).min(BURST * 1_000);
    }

    fn note_invalid(&mut self, now_us: u64) -> bool {
        self.invalid.push_back(now_us);
        while let Some(&oldest) = self.invalid.front() {
            if now_us.saturating_sub(oldest) > INVALID_WINDOW_US {
                self.invalid.pop_front();
            } else {
                break;
            }
        }
        self.invalid.len() > INVALID_LIMIT
    }

    /// Takes one datagram of the share connection.
    pub fn on_datagram(&mut self, now_us: u64, bytes: &[u8]) -> Gated {
        if self.mode == PadMode::Watch {
            return Gated::nothing(Dropped::Watch, false);
        }
        let datagram = match PadDatagram::parse(bytes) {
            Ok(d) => d,
            Err(refusal) => {
                let close = self.note_invalid(now_us);
                return Gated::nothing(Dropped::Invalid(refusal), close);
            }
        };
        // How many of the datagram's states are new: state `i` has sequence `sequence - i`.
        // The first datagram of a mode counts its newest state only: the repeats behind it
        // belong to what the guest pressed before, perhaps on another pad.
        let fresh = match self.newest {
            None => 1,
            Some(newest) => {
                let ahead = datagram.sequence.wrapping_sub(newest) as i16;
                if ahead <= 0 {
                    return Gated::nothing(Dropped::Stale, false);
                }
                datagram.states.len().min(ahead as usize)
            }
        };
        self.refill(now_us);
        let wanted = fresh as u64 * 1_000;
        let take = if self.tokens >= wanted {
            fresh
        } else if self.tokens >= 1_000 {
            // Over the rate: keep the newest.
            1
        } else {
            return Gated::nothing(Dropped::TooFast, false);
        };
        self.tokens -= take as u64 * 1_000;
        let dropped = (take < fresh).then_some(Dropped::TooFast);
        let apply = (0..take)
            .rev()
            .map(|i| (datagram.sequence.wrapping_sub(i as u16), datagram.states[i]))
            .collect();
        self.newest = Some(datagram.sequence);
        self.last_valid_us = Some(now_us);
        self.neutral_sent = false;
        Gated {
            apply,
            dropped,
            close: false,
        }
    }

    /// Call often (at least every 50 ms). After 250 ms without a valid datagram it gives the
    /// neutral state once, and the engine keeps it until a new datagram arrives.
    pub fn poll(&mut self, now_us: u64) -> Option<PadState> {
        if self.mode == PadMode::Watch || self.neutral_sent {
            return None;
        }
        let last = self.last_valid_us?;
        if now_us.saturating_sub(last) >= SILENCE_US {
            self.neutral_sent = true;
            return Some(PadState::NEUTRAL);
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(buttons: u32, lx: u8) -> PadState {
        PadState {
            buttons,
            lx,
            ..PadState::NEUTRAL
        }
    }

    fn datagram(sequence: u16, states: &[PadState]) -> Vec<u8> {
        PadDatagram {
            sequence,
            states: states.to_vec(),
        }
        .encode()
        .unwrap()
    }

    fn gate(mode: PadMode) -> PadGate {
        let mut g = PadGate::new(0);
        g.set_mode(mode);
        g
    }

    #[test]
    fn the_layout_is_the_one_in_the_spec() {
        let s = PadState {
            buttons: 0x0000_4011,
            lx: 1,
            ly: 2,
            rx: 3,
            ry: 4,
            l2: 5,
            r2: 6,
        };
        assert_eq!(
            datagram(0x0102, &[s]),
            [0, 4, 1, 2, 1, 0, 0, 0x40, 0x11, 1, 2, 3, 4, 5, 6]
        );
        assert_eq!(datagram(0x0102, &[s, s, s]).len(), 5 + 30);
    }

    #[test]
    fn a_datagram_survives_the_round_trip() {
        let states = [
            state(button::SOUTH, 0),
            state(button::EAST, 255),
            PadState::NEUTRAL,
        ];
        let bytes = datagram(65_535, &states);
        let back = PadDatagram::parse(&bytes).unwrap();
        assert_eq!(back.sequence, 65_535);
        assert_eq!(back.states, states);
        assert_eq!(back.encode().unwrap(), bytes);
    }

    #[test]
    fn the_parser_is_exact() {
        let good = datagram(1, &[PadState::NEUTRAL]);
        assert!(PadDatagram::parse(&good).is_ok());
        let mut v = good.clone();
        v[0] = 1;
        assert_eq!(PadDatagram::parse(&v), Err(PadRefusal::Version(1)));
        let mut t = good.clone();
        t[1] = 1;
        assert_eq!(PadDatagram::parse(&t), Err(PadRefusal::Type(1)));
        for count in [0u8, 4, 255] {
            let mut c = good.clone();
            c[4] = count;
            assert_eq!(PadDatagram::parse(&c), Err(PadRefusal::Count(count)));
        }
        let mut long = good.clone();
        long.push(0);
        assert_eq!(PadDatagram::parse(&long), Err(PadRefusal::Length));
        assert_eq!(PadDatagram::parse(&good[..14]), Err(PadRefusal::Length));
        assert_eq!(PadDatagram::parse(&[]), Err(PadRefusal::Length));
        let mut reserved = good.clone();
        reserved[5] = 0x80; // bit 31
        assert_eq!(
            PadDatagram::parse(&reserved),
            Err(PadRefusal::ReservedButtons)
        );
        let mut bit15 = good;
        bit15[7] = 0x80; // bit 15
        assert_eq!(PadDatagram::parse(&bit15), Err(PadRefusal::ReservedButtons));
    }

    #[test]
    fn a_state_with_a_reserved_bit_is_not_written() {
        let bad = PadState {
            buttons: 1 << 15,
            ..PadState::NEUTRAL
        };
        let d = PadDatagram {
            sequence: 0,
            states: vec![bad],
        };
        assert_eq!(d.encode(), None);
        assert_eq!(
            PadDatagram {
                sequence: 0,
                states: vec![]
            }
            .encode(),
            None
        );
    }

    #[test]
    fn the_sender_repeats_the_two_states_before_the_newest() {
        let mut sender = PadSender::new();
        let a = state(button::SOUTH, 10);
        let b = state(button::EAST, 20);
        let c = state(button::WEST, 30);
        let d = state(button::NORTH, 40);
        let first = PadDatagram::parse(&sender.next(a)).unwrap();
        assert_eq!((first.sequence, first.states.len()), (0, 1));
        let second = PadDatagram::parse(&sender.next(b)).unwrap();
        assert_eq!((second.sequence, second.states), (1, vec![b, a]));
        let third = PadDatagram::parse(&sender.next(c)).unwrap();
        assert_eq!((third.sequence, third.states), (2, vec![c, b, a]));
        let fourth = PadDatagram::parse(&sender.next(d)).unwrap();
        assert_eq!((fourth.sequence, fourth.states), (3, vec![d, c, b]));
    }

    #[test]
    fn the_sender_cleans_reserved_bits_and_wraps() {
        let mut sender = PadSender::new();
        sender.sequence = 65_535;
        sender.started = true;
        let dirty = PadState {
            buttons: 0xFFFF_FFFF,
            ..PadState::NEUTRAL
        };
        let parsed = PadDatagram::parse(&sender.next(dirty)).unwrap();
        assert_eq!(parsed.sequence, 0);
        assert_eq!(parsed.states[0].buttons, BUTTON_MASK);
    }

    #[test]
    fn watch_drops_pad_datagrams_without_counting_them() {
        let mut g = gate(PadMode::Watch);
        let out = g.on_datagram(1, &datagram(0, &[state(1, 0)]));
        assert_eq!(out.dropped, Some(Dropped::Watch));
        assert!(out.apply.is_empty() && !out.close);
        for i in 0..100 {
            let out = g.on_datagram(2 + i, b"garbage");
            assert!(!out.close);
        }
    }

    #[test]
    fn a_valid_state_is_applied_in_hand_over_and_together() {
        for mode in [PadMode::HandOver, PadMode::Together] {
            let mut g = gate(mode);
            let s = state(button::SOUTH, 200);
            let out = g.on_datagram(1_000, &datagram(5, &[s]));
            assert_eq!(out.apply, vec![(5, s)]);
            assert_eq!(out.dropped, None);
        }
        assert_eq!(PadMode::HandOver.slot(), Some(0));
        assert_eq!(PadMode::Together.slot(), Some(1));
        assert_eq!(PadMode::Watch.slot(), None);
    }

    #[test]
    fn older_states_in_a_datagram_fill_the_gap_oldest_first() {
        let mut g = gate(PadMode::HandOver);
        let (a, b, c, d) = (state(1, 1), state(2, 2), state(4, 3), state(8, 4));
        assert_eq!(g.on_datagram(0, &datagram(10, &[a])).apply, vec![(10, a)]);
        // The datagram with 11 was lost; 12 repeats it.
        let out = g.on_datagram(4_000, &datagram(12, &[c, b, a]));
        assert_eq!(out.apply, vec![(11, b), (12, c)]);
        // The next one repeats what is seen: only the newest is new.
        let out = g.on_datagram(8_000, &datagram(13, &[d, c, b]));
        assert_eq!(out.apply, vec![(13, d)]);
    }

    #[test]
    fn the_first_datagram_of_a_mode_counts_its_newest_state_only() {
        let mut g = gate(PadMode::Together);
        let (a, b, c) = (state(1, 1), state(2, 2), state(4, 3));
        let out = g.on_datagram(0, &datagram(40, &[c, b, a]));
        assert_eq!(out.apply, vec![(40, c)], "the repeats are from before");
    }

    #[test]
    fn a_state_behind_the_newest_is_dropped() {
        let mut g = gate(PadMode::HandOver);
        let s = state(1, 1);
        assert!(!g.on_datagram(0, &datagram(100, &[s])).apply.is_empty());
        for sequence in [100u16, 99, 98, 100u16.wrapping_sub(30_000)] {
            let out = g.on_datagram(1_000, &datagram(sequence, &[s, s, s]));
            assert_eq!(out.dropped, Some(Dropped::Stale), "sequence {sequence}");
            assert!(out.apply.is_empty());
        }
    }

    #[test]
    fn the_sequence_wraps() {
        let mut g = gate(PadMode::HandOver);
        let (a, b) = (state(1, 1), state(2, 2));
        g.on_datagram(0, &datagram(65_535, &[a]));
        let out = g.on_datagram(4_000, &datagram(0, &[b, a]));
        assert_eq!(out.apply, vec![(0, b)]);
        let out = g.on_datagram(8_000, &datagram(65_534, &[a]));
        assert_eq!(out.dropped, Some(Dropped::Stale));
    }

    #[test]
    fn the_rate_is_300_a_second_with_a_burst_of_30() {
        let mut g = gate(PadMode::HandOver);
        let s = state(1, 1);
        // 40 states at one instant: the first 30 pass, the rest are dropped.
        let mut passed = 0;
        for i in 0..40u16 {
            let out = g.on_datagram(0, &datagram(i, &[s]));
            passed += out.apply.len();
        }
        assert_eq!(passed, 30);
        // After a second the bucket is full again, and 300 a second then go through.
        let mut passed = 0;
        for i in 0..300u64 {
            let at = 1_000_000 + i * 1_000_000 / 300;
            let out = g.on_datagram(at, &datagram(100 + i as u16, &[s]));
            passed += out.apply.len();
        }
        assert!(passed >= 295, "{passed}");
        // 600 a second is twice too many.
        let mut g = gate(PadMode::HandOver);
        let mut passed = 0;
        for i in 0..1_200u64 {
            let out = g.on_datagram(i * 1_000_000 / 600, &datagram(i as u16, &[s]));
            passed += out.apply.len();
        }
        assert!((600..=660).contains(&passed), "{passed}");
    }

    #[test]
    fn over_the_rate_the_newest_state_is_kept() {
        let mut g = gate(PadMode::HandOver);
        let s = state(1, 1);
        for i in 0..29u16 {
            g.on_datagram(0, &datagram(i, &[s]));
        }
        // One token is left; three new states want three.
        let (a, b, c) = (state(2, 2), state(4, 3), state(8, 4));
        let out = g.on_datagram(0, &datagram(32, &[c, b, a]));
        assert_eq!(out.apply, vec![(32, c)]);
        assert_eq!(out.dropped, Some(Dropped::TooFast));
    }

    #[test]
    fn many_invalid_datagrams_close_the_connection() {
        let mut g = gate(PadMode::HandOver);
        for i in 0..INVALID_LIMIT as u64 {
            let out = g.on_datagram(i * 1_000, b"nope");
            assert!(matches!(out.dropped, Some(Dropped::Invalid(_))));
            assert!(!out.close, "{i}");
        }
        assert!(g.on_datagram(30_000, b"nope").close);
    }

    #[test]
    fn invalid_datagrams_spread_over_time_do_not_close() {
        let mut g = gate(PadMode::HandOver);
        for i in 0..200u64 {
            // One every 400 ms: 12 or 13 in any 5 seconds.
            assert!(!g.on_datagram(i * 400_000, b"nope").close, "{i}");
        }
    }

    #[test]
    fn silence_gives_one_neutral_state() {
        let mut g = gate(PadMode::HandOver);
        assert_eq!(
            g.poll(10_000_000),
            None,
            "no state seen, nothing to neutralise"
        );
        g.on_datagram(1_000_000, &datagram(1, &[state(button::SOUTH, 0)]));
        assert_eq!(g.poll(1_100_000), None);
        assert_eq!(g.poll(1_249_999), None);
        assert_eq!(g.poll(1_250_000), Some(PadState::NEUTRAL));
        assert_eq!(g.poll(1_300_000), None, "only once");
        // A new datagram ends the silence and the next silence is neutral again.
        g.on_datagram(2_000_000, &datagram(2, &[state(button::EAST, 0)]));
        assert_eq!(g.poll(2_100_000), None);
        assert_eq!(g.poll(2_250_000), Some(PadState::NEUTRAL));
    }

    #[test]
    fn a_change_of_mode_forgets_the_old_stream() {
        let mut g = gate(PadMode::HandOver);
        g.on_datagram(0, &datagram(500, &[state(1, 1)]));
        g.set_mode(PadMode::Watch);
        assert_eq!(g.poll(1_000_000), None);
        assert_eq!(
            g.on_datagram(2_000, &datagram(501, &[state(1, 1)])).dropped,
            Some(Dropped::Watch)
        );
        g.set_mode(PadMode::Together);
        // The viewer's numbering may restart: a lower sequence is fine again.
        let out = g.on_datagram(3_000, &datagram(3, &[state(1, 1)]));
        assert_eq!(out.apply.len(), 1);
    }

    /// A small generator for the fixed-seed tests.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
    }

    #[test]
    fn random_bytes_are_read_or_refused_and_read_ones_write_back() {
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        for _ in 0..20_000 {
            let len = (rng.next() % 40) as usize;
            let mut bytes: Vec<u8> = (0..len).map(|_| rng.next() as u8).collect();
            if len >= 2 && rng.next().is_multiple_of(2) {
                bytes[0] = 0;
                bytes[1] = TYPE_PAD;
            }
            if let Ok(d) = PadDatagram::parse(&bytes) {
                assert_eq!(d.encode().unwrap(), bytes);
            }
        }
    }

    #[test]
    fn a_gate_never_applies_a_reserved_bit() {
        let mut rng = Rng(7);
        let mut g = gate(PadMode::Together);
        for i in 0..20_000u64 {
            let len = (rng.next() % 40) as usize;
            let mut bytes: Vec<u8> = (0..len).map(|_| rng.next() as u8).collect();
            if len >= 2 {
                bytes[0] = 0;
                bytes[1] = TYPE_PAD;
            }
            let out = g.on_datagram(i * 10_000, &bytes);
            for (_, s) in out.apply {
                assert!(s.is_valid());
            }
        }
    }
}
