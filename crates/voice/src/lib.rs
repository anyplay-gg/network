//! Party voice for the AnyPlay network.
//!
//! [`Engine`] is the whole voice side of a party: it turns the microphone into small datagrams
//! and the datagrams of up to seven other members into one stream for the speakers. It knows
//! nothing about parties, accounts or tunnels. Bytes go in and bytes come out; the net service
//! moves them.
//!
//! ```
//! use anyplay_voice::{Config, Engine, MemoryDevice, PeerId};
//!
//! let (device, _hardware) = MemoryDevice::new(48_000, 48_000);
//! let mut voice = Engine::new(Config::default(), Box::new(device)).unwrap();
//!
//! voice.set_talking(true); // push-to-talk is down
//! voice.pump(); // call this every 5 to 10 ms
//! for datagram in voice.outgoing() {
//!     // send `datagram` on the voice stream of every other member's tunnel
//!     # let _ = datagram;
//! }
//!
//! // a datagram arrived from a member's tunnel
//! # let datagram = [0u8; 4];
//! let _ = voice.incoming(PeerId(7), &datagram);
//! let _ = voice.levels(); // who is speaking, for the party window
//! ```
//!
//! # How the net service uses it
//!
//! The net service owns one `Engine` per party the player is in, only while voice is switched on
//! (`crates/net/src/voice.rs`). For every other member who takes part it holds a QUIC connection
//! of its own with the ALPN `anyplay/voice/0`, on the same iroh endpoint as the game tunnels,
//! because game datagrams carry no tag byte to tell them from voice (the framing and the rules
//! about who may connect are in `spec/voice.md`). The relay-only setting needs no code here: the
//! endpoint has no direct addresses then, so the connection goes through the relay.
//!
//! * a task calls [`Engine::pump`] every 5 to 10 ms, then sends each of [`Engine::outgoing`] to
//!   every member's tunnel;
//! * a datagram from a member's tunnel goes to [`Engine::incoming`] with that member's
//!   [`PeerId`];
//! * the launcher sends push-to-talk, mode, volume and mute over the control protocol, and the
//!   net service calls the matching setters; [`Engine::levels`] flows back as speaking indicators;
//! * a member who leaves the party is dropped with [`Engine::remove_peer`];
//! * the microphone and speakers come from an [`AudioIo`]: with the feature `devices`,
//!   `CpalDevice::open_named` with the names `input_names` and `output_names` list.
//!
//! # Echo
//!
//! There is no acoustic echo cancellation. Push-to-talk is the default, and with headphones that
//! is all a party needs. With the open microphone ([`TalkMode::VoiceActivity`]) and speakers, the
//! members hear themselves back. See `docs/adr/` for the decision and what would replace it.

mod device;
mod gate;
mod jitter;
mod mix;
mod peer;
mod resample;
mod send;
pub mod wire;

#[cfg(feature = "devices")]
mod cpal_device;

use std::collections::HashMap;
use std::fmt;
use std::time::Instant;

pub use device::{AudioIo, MemoryDevice, MemoryHandle};
pub use gate::VadConfig;
pub use jitter::PeerStats;

#[cfg(feature = "devices")]
pub use cpal_device::{CpalDevice, input_names, output_names};

use jitter::FRAME_SAMPLES;
use mix::Mixer;
use peer::Peer;
use resample::Resampler;
use send::Sender;
use wire::Frame;

/// The most other members in a party of eight.
pub const MAX_PEERS: usize = 7;

/// Milliseconds of audio the engine keeps queued at the device. More survives a late
/// [`Engine::pump`]; less is less delay.
const PLAYBACK_QUEUE_MS: u32 = 60;
/// Datagrams waiting to be taken by [`Engine::outgoing`] before the oldest are dropped.
const OUTGOING_LIMIT: usize = 100;

/// Another member of the party, as the caller names them. The engine only compares and stores it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PeerId(pub u64);

/// When the microphone is open.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TalkMode {
    /// While [`Engine::set_talking`] is true. The default.
    #[default]
    PushToTalk,
    /// Whenever the noise suppressor hears speech. Without headphones this feeds the speakers
    /// back into the microphone; see the crate docs.
    VoiceActivity,
}

/// Settings of an [`Engine`]. Every one has a default that suits a party.
#[derive(Clone, Debug)]
pub struct Config {
    pub mode: TalkMode,
    /// RNNoise noise suppression on the microphone.
    pub noise_suppression: bool,
    /// Opus bitrate in bits per second, 16000 to 64000.
    pub bitrate: u32,
    /// What the encoder should expect to lose, in percent, which sets how much redundancy it adds.
    pub expected_loss_percent: u8,
    /// Opus discontinuous transmission: while the sound is not speech, send one small frame
    /// every 400 ms instead of fifty a second.
    pub dtx: bool,
    pub vad: VadConfig,
}

impl Default for Config {
    fn default() -> Config {
        Config {
            mode: TalkMode::PushToTalk,
            noise_suppression: true,
            bitrate: 28_000,
            expected_loss_percent: 10,
            dtx: true,
            vad: VadConfig::default(),
        }
    }
}

/// How loud a member is and whether the UI should light them up.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Level {
    /// Recent loudness of the cleaned signal, 0.0 to 1.0, before the player's volume setting.
    pub rms: f32,
    pub speaking: bool,
}

/// The speaking indicators of a party.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Levels {
    /// The player's own microphone, after the gate.
    pub me: Level,
    /// Every member the engine has heard from, ordered by id.
    pub peers: Vec<(PeerId, Level)>,
}

/// Why the engine could not start.
#[derive(Debug)]
pub enum Error {
    /// The codec refused a setting.
    Codec(opus::Error),
    /// A setting is outside what the engine supports.
    Config(&'static str),
    /// The sound hardware could not be opened.
    Device(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Codec(e) => write!(f, "voice codec: {e}"),
            Error::Config(what) => write!(f, "voice setting: {what}"),
            Error::Device(what) => write!(f, "sound device: {what}"),
        }
    }
}

impl std::error::Error for Error {}

/// Why a datagram was not used.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rejected {
    /// Not a voice frame.
    Malformed(wire::WireError),
    /// The Opus packet is not exactly 20 ms.
    NotTwentyMilliseconds,
    /// Seven members are already being heard; this is an eighth.
    TooManyPeers,
}

impl fmt::Display for Rejected {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Rejected::Malformed(e) => e.fmt(f),
            Rejected::NotTwentyMilliseconds => write!(f, "voice packet is not 20 ms long"),
            Rejected::TooManyPeers => write!(f, "more than seven voice peers"),
        }
    }
}

impl std::error::Error for Rejected {}

/// The voice side of one party. See the [crate docs](crate).
pub struct Engine {
    device: Box<dyn AudioIo>,
    sender: Sender,
    peers: HashMap<PeerId, Peer>,
    mixer: Mixer,
    playback: Resampler,
    playback_rate: u32,
    scratch: Vec<f32>,
    outgoing: Vec<Vec<u8>>,
    muted: bool,
    deafened: bool,
}

impl Engine {
    /// An engine on `device`. Nothing is captured or played until [`pump`](Engine::pump).
    pub fn new(config: Config, device: Box<dyn AudioIo>) -> Result<Engine, Error> {
        if !(16_000..=64_000).contains(&config.bitrate) {
            return Err(Error::Config("bitrate must be 16000 to 64000"));
        }
        if config.expected_loss_percent > 100 {
            return Err(Error::Config("expected loss is a percentage"));
        }
        for rate in [device.capture_rate(), device.playback_rate()] {
            if !(8_000..=192_000).contains(&rate) {
                return Err(Error::Config("device rate must be 8000 to 192000 Hz"));
            }
        }
        let playback_rate = device.playback_rate();
        Ok(Engine {
            sender: Sender::new(&config, device.capture_rate())?,
            peers: HashMap::new(),
            mixer: Mixer::new(),
            playback: Resampler::new(48_000, playback_rate),
            playback_rate,
            scratch: Vec::new(),
            outgoing: Vec::new(),
            muted: false,
            deafened: false,
            device,
        })
    }

    /// Push-to-talk: whether the key is down. Has no effect in [`TalkMode::VoiceActivity`].
    pub fn set_talking(&mut self, talking: bool) {
        self.sender.set_talking(talking);
    }

    /// Mutes the player's own microphone: nothing is sent while it is true, whatever the mode.
    pub fn set_mic_muted(&mut self, muted: bool) {
        self.muted = muted;
        self.sender.set_silenced(self.muted || self.deafened);
    }

    /// Deafens the player: nobody is heard, and the microphone is silenced too. The members'
    /// frames still arrive and are dropped unplayed, so hearing resumes at once.
    pub fn set_deafened(&mut self, deafened: bool) {
        self.deafened = deafened;
        self.sender.set_silenced(self.muted || self.deafened);
    }

    pub fn set_mode(&mut self, mode: TalkMode) {
        self.sender.set_mode(mode);
    }

    /// How loud one member is heard, 0.0 (silent) to 2.0 (double). Applies to members not yet
    /// heard from too.
    pub fn set_peer_volume(&mut self, peer: PeerId, volume: f32) {
        if let Some(p) = self.peer(peer) {
            p.volume = if volume.is_finite() {
                volume.clamp(0.0, 2.0)
            } else {
                1.0
            };
        }
    }

    /// Stop (or resume) hearing one member. Their frames still arrive and are dropped unplayed.
    pub fn mute(&mut self, peer: PeerId, muted: bool) {
        if let Some(p) = self.peer(peer) {
            p.muted = muted;
        }
    }

    /// Forget a member who left, with their volume and mute.
    pub fn remove_peer(&mut self, peer: PeerId) {
        self.peers.remove(&peer);
    }

    /// A datagram from `peer`'s tunnel.
    pub fn incoming(&mut self, peer: PeerId, datagram: &[u8]) -> Result<(), Rejected> {
        self.incoming_at(peer, datagram, Instant::now())
    }

    /// [`incoming`](Engine::incoming) with the arrival time given, for tests that simulate a
    /// network.
    pub fn incoming_at(
        &mut self,
        peer: PeerId,
        datagram: &[u8],
        now: Instant,
    ) -> Result<(), Rejected> {
        let frame = Frame::parse(datagram).map_err(Rejected::Malformed)?;
        if opus::packet::get_nb_samples(frame.payload, 48_000).ok() != Some(FRAME_SAMPLES) {
            return Err(Rejected::NotTwentyMilliseconds);
        }
        let known = self.peers.contains_key(&peer);
        if !known && self.peers.len() >= MAX_PEERS {
            return Err(Rejected::TooManyPeers);
        }
        let Some(p) = self.peer(peer) else {
            return Ok(());
        };
        p.accept(
            frame.sequence,
            frame.timestamp,
            frame.end_of_talk,
            frame.payload,
            now,
        );
        Ok(())
    }

    /// Move audio: take what the microphone captured, and fill the speakers' queue. Call every
    /// 5 to 10 ms. Datagrams to send are then in [`outgoing`](Engine::outgoing).
    pub fn pump(&mut self) {
        self.scratch.clear();
        self.device.read_capture(&mut self.scratch);
        self.sender.push(&self.scratch);
        self.outgoing.extend(self.sender.take_output());
        if self.outgoing.len() > OUTGOING_LIMIT {
            let excess = self.outgoing.len() - OUTGOING_LIMIT;
            self.outgoing.drain(..excess);
        }

        let want = (self.playback_rate * PLAYBACK_QUEUE_MS / 1000) as usize;
        // A device that never drains must not hold this loop.
        for _ in 0..16 {
            if self.device.playback_queued() >= want {
                break;
            }
            let frame = self.mix_frame();
            self.scratch.clear();
            self.playback.process(&frame, &mut self.scratch);
            self.device.write_playback(&self.scratch);
        }
    }

    /// The datagrams to send since the last call, oldest first. Each goes to every other member.
    pub fn outgoing(&mut self) -> Vec<Vec<u8>> {
        std::mem::take(&mut self.outgoing)
    }

    /// Who is speaking.
    pub fn levels(&self) -> Levels {
        let mut peers: Vec<_> = self.peers.iter().map(|(id, p)| (*id, p.level())).collect();
        peers.sort_by_key(|(id, _)| *id);
        Levels {
            me: self.sender.level(),
            peers,
        }
    }

    /// What the jitter buffer saw of one member.
    pub fn peer_stats(&self, peer: PeerId) -> Option<PeerStats> {
        self.peers.get(&peer).map(Peer::stats)
    }

    fn peer(&mut self, id: PeerId) -> Option<&mut Peer> {
        if !self.peers.contains_key(&id) {
            if self.peers.len() >= MAX_PEERS {
                return None;
            }
            self.peers.insert(id, Peer::new().ok()?);
        }
        self.peers.get_mut(&id)
    }

    fn mix_frame(&mut self) -> [f32; FRAME_SAMPLES] {
        let mut frame = [0.0f32; FRAME_SAMPLES];
        for p in self.peers.values_mut() {
            if p.next_frame(&mut frame) && !self.deafened {
                self.mixer.add(&frame, p.volume);
            }
        }
        self.mixer.finish()
    }
}
