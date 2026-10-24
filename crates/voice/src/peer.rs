//! Everything the engine keeps for one remote member: jitter buffer, decoder, volume and mute,
//! and the level that tells a UI whether the member is speaking.

use std::time::Instant;

use opus::{Channels, Decoder};

use crate::gate::{rms, to_db};
use crate::jitter::{FRAME_SAMPLES, JitterBuffer, PeerStats, Playout};
use crate::{Error, Level};

/// Frames of the cleaned signal above this level count as speaking.
const SPEAKING_DB: f32 = -50.0;
/// How long the indicator stays on after the last loud frame, in frames.
const HOLD_FRAMES: u32 = 8;

pub(crate) struct Peer {
    jitter: JitterBuffer,
    decoder: Decoder,
    pub(crate) volume: f32,
    pub(crate) muted: bool,
    rms: f32,
    hold: u32,
}

impl Peer {
    pub(crate) fn new() -> Result<Peer, Error> {
        Ok(Peer {
            jitter: JitterBuffer::new(),
            decoder: Decoder::new(48_000, Channels::Mono).map_err(Error::Codec)?,
            volume: 1.0,
            muted: false,
            rms: 0.0,
            hold: 0,
        })
    }

    pub(crate) fn accept(
        &mut self,
        sequence: u16,
        timestamp: u32,
        end_of_talk: bool,
        payload: &[u8],
        now: Instant,
    ) {
        self.jitter
            .insert(sequence, timestamp, end_of_talk, payload, now);
    }

    /// Fill `out` with the next 20 ms of this member. False means silence, and `out` is untouched.
    pub(crate) fn next_frame(&mut self, out: &mut [f32; FRAME_SAMPLES]) -> bool {
        let playout = self.jitter.pop();
        let decoded = if self.muted {
            false
        } else {
            match playout {
                Playout::Silence => false,
                Playout::Frame { payload, .. } => self.decode(&payload, false, out),
                Playout::Recover { next_payload } => self.decode(&next_payload, true, out),
                Playout::Conceal => self.decode(&[], false, out),
            }
        };
        if decoded {
            let level = rms(out);
            self.rms = 0.5 * self.rms + 0.5 * level;
            if to_db(level) >= SPEAKING_DB {
                self.hold = HOLD_FRAMES;
            }
        } else {
            self.rms *= 0.5;
        }
        self.hold = self.hold.saturating_sub(1);
        if self.muted {
            self.hold = 0;
        }
        decoded
    }

    fn decode(&mut self, packet: &[u8], fec: bool, out: &mut [f32; FRAME_SAMPLES]) -> bool {
        match self.decoder.decode_float(packet, out, fec) {
            Ok(n) if n == FRAME_SAMPLES => true,
            Ok(n) => {
                out[n..].fill(0.0);
                true
            }
            Err(_) => false,
        }
    }

    pub(crate) fn level(&self) -> Level {
        Level {
            rms: if self.muted { 0.0 } else { self.rms },
            speaking: self.hold > 0,
        }
    }

    pub(crate) fn stats(&self) -> PeerStats {
        self.jitter.stats()
    }
}
