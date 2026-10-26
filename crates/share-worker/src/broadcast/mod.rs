//! Broadcast: the game's window, encoded as H.264 and published live to an RTMP server (Twitch,
//! YouTube, any other), see spec/broadcast.md.
//!
//! The worker runs in this mode on its own, without a share link: the launcher starts it with
//! `--broadcast`, writes one JSON command per line to its input (the first one carries the
//! server address and the stream key, which are never put on the command line) and reads one
//! JSON event per line from its output. The parts:
//!
//! - [`amf`]: the commands of RTMP (pure);
//! - [`rtmp`]: the connection, the chunk stream and the publisher;
//! - [`flv`]: the bodies of video and audio messages (pure);
//! - [`run`]: the loop that takes pictures from a [`FrameSource`], encodes them and sends them.

pub mod amf;
pub mod flv;
pub mod rtmp;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyplay_share::codec::{CodecError, VideoEncoder};
use serde::{Deserialize, Serialize};

use crate::host::{FrameSource, Next};
use amf::Value;
use rtmp::{Publisher, Target};

/// What the launcher tells the broadcaster, one JSON object per line.
#[derive(Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "do", rename_all = "snake_case")]
pub enum Command {
    /// Where to publish and under which key. Always the first line.
    Publish { url: String, key: String },
    /// End.
    Quit,
}

// The key is a secret: a debug print never shows it.
impl std::fmt::Debug for Command {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Command::Publish { url, .. } => write!(f, "Publish {{ url: {url:?}, key: <hidden> }}"),
            Command::Quit => write!(f, "Quit"),
        }
    }
}

/// What the broadcaster tells the launcher, one JSON object per line.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    /// The server is being reached.
    Connecting,
    /// The server accepted the stream and pictures are going out.
    Live {
        width: u32,
        height: u32,
        fps: u32,
        kbps: u32,
    },
    /// Every 2 seconds while live: how long, the rate really sent, pictures dropped so far.
    Stats {
        seconds: u32,
        kbps: u32,
        dropped: u32,
    },
    /// The broadcast ended. `reason`: `no_encoder`, `no_window`, `window_closed`,
    /// `connect_failed: ...` (in words), `disconnected: ...`, `encoder_failed: ...`, `quit`.
    Ended { reason: String },
}

impl Event {
    pub fn to_line(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }
}

/// The key frame interval: the services ask for at most 2 seconds.
pub const KEY_SECONDS: u32 = 2;
/// How many messages may wait for a slow connection before pictures are dropped.
const QUEUE: usize = 90;
/// How often the stats leave.
const STATS_EVERY: Duration = Duration::from_secs(2);

/// What the encoder was opened with.
#[derive(Clone, Copy, Debug)]
pub struct Setup {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub kbps: u32,
}

/// One picture on its way to the connection.
struct Packet {
    ts_ms: u32,
    key: bool,
    data: Vec<u8>,
}

/// Sends pictures (and the silence that goes with them) until the channel closes or the
/// connection breaks.
fn send_loop(
    publisher: &Publisher,
    setup: Setup,
    packets: Receiver<Packet>,
    sent: &AtomicU64,
    failure: &Mutex<Option<String>>,
) {
    let mut config: Option<flv::VideoConfig> = None;
    let mut audio_frames: u64 = 0;
    let fail = |why: String| {
        if let Ok(mut slot) = failure.lock() {
            slot.get_or_insert(why);
        }
    };
    let metadata = [
        ("duration", Value::Number(0.0)),
        ("width", Value::Number(f64::from(setup.width))),
        ("height", Value::Number(f64::from(setup.height))),
        ("videocodecid", Value::Number(7.0)),
        ("videodatarate", Value::Number(f64::from(setup.kbps))),
        ("framerate", Value::Number(f64::from(setup.fps))),
        ("audiocodecid", Value::Number(10.0)),
        ("audiodatarate", Value::Number(0.0)),
        ("audiosamplerate", Value::Number(flv::AAC_RATE as f64)),
        ("audiosamplesize", Value::Number(16.0)),
        ("stereo", Value::Bool(true)),
        ("encoder", Value::text("AnyPlay Launcher")),
    ];
    if let Err(why) = publisher.metadata(&metadata) {
        return fail(why);
    }
    for packet in packets {
        if packet.key {
            // A key frame brings the configuration; a changed one is announced again.
            if let Some(new) = flv::VideoConfig::from_annex_b(&packet.data)
                && config.as_ref() != Some(&new)
            {
                let first = config.is_none();
                if let Err(why) = publisher.video(packet.ts_ms, &new.tag()) {
                    return fail(why);
                }
                if first && let Err(why) = publisher.audio(0, &flv::audio_config_tag()) {
                    return fail(why);
                }
                config = Some(new);
            }
        }
        // Nothing can be shown before the first configuration.
        if config.is_none() {
            continue;
        }
        // The silence up to this picture's time, so that audio and video stay in step.
        loop {
            let at = audio_frames * flv::AAC_FRAME_SAMPLES * 1000 / flv::AAC_RATE;
            if at > u64::from(packet.ts_ms) {
                break;
            }
            if let Err(why) = publisher.audio(at as u32, &flv::audio_tag(&flv::AAC_SILENCE)) {
                return fail(why);
            }
            audio_frames += 1;
        }
        let Some(tag) = flv::video_tag(&packet.data, packet.key) else {
            continue;
        };
        if let Err(why) = publisher.video(packet.ts_ms, &tag) {
            return fail(why);
        }
        sent.fetch_add(tag.len() as u64, Ordering::Relaxed);
    }
}

/// Publishes until told to quit, the source ends, the connection breaks or the encoder fails.
pub fn run(
    setup: Setup,
    url: &str,
    key: &str,
    mut source: Box<dyn FrameSource>,
    mut encoder: Box<dyn VideoEncoder>,
    commands: Receiver<Command>,
    events: Sender<Event>,
) {
    let end = |reason: String| {
        let _ = events.send(Event::Ended { reason });
    };
    let _ = events.send(Event::Connecting);
    let target = match Target::parse(url) {
        Ok(target) => target,
        Err(why) => return end(format!("connect_failed: {why}")),
    };
    let publisher = match Publisher::open(&target, key) {
        Ok(publisher) => Arc::new(publisher),
        Err(why) => return end(format!("connect_failed: {why}")),
    };
    let _ = events.send(Event::Live {
        width: setup.width,
        height: setup.height,
        fps: setup.fps,
        kbps: setup.kbps,
    });

    let (queue, packets): (SyncSender<Packet>, Receiver<Packet>) = sync_channel(QUEUE);
    let sent = Arc::new(AtomicU64::new(0));
    let failure = Arc::new(Mutex::new(None));
    let sender = {
        let (publisher, sent, failure) = (publisher.clone(), sent.clone(), failure.clone());
        std::thread::Builder::new()
            .name("broadcast-send".into())
            .spawn(move || send_loop(&publisher, setup, packets, &sent, &failure))
    };
    let Ok(sender) = sender else {
        publisher.close();
        return end("connect_failed: the sender could not start".into());
    };

    let reason = pump(
        setup,
        &mut *source,
        &mut *encoder,
        &commands,
        &events,
        &Link {
            queue: &queue,
            publisher: &publisher,
            sent: &sent,
            failure: &failure,
        },
    );
    drop(queue);
    // Let what is queued go out before the stream is closed, but not for long.
    let _ = sender.join();
    publisher.close();
    end(reason);
}

/// The way out of the pump: the queue to the sender and what it reports back.
struct Link<'a> {
    queue: &'a SyncSender<Packet>,
    publisher: &'a Publisher,
    sent: &'a AtomicU64,
    failure: &'a Mutex<Option<String>>,
}

impl Link<'_> {
    fn why_lost(&self) -> Option<String> {
        self.failure
            .lock()
            .ok()
            .and_then(|f| f.clone())
            .or_else(|| self.publisher.lost())
    }
}

fn pump(
    setup: Setup,
    source: &mut dyn FrameSource,
    encoder: &mut dyn VideoEncoder,
    commands: &Receiver<Command>,
    events: &Sender<Event>,
    link: &Link<'_>,
) -> String {
    let period = Duration::from_secs_f64(1.0 / f64::from(setup.fps.clamp(1, 60)));
    let key_every = setup.fps.max(1) * KEY_SECONDS;
    let clock = Instant::now();
    let mut next = Instant::now();
    let mut since_key = key_every;
    let mut key_wanted = true;
    // After a dropped picture the following ones refer to something the viewers never got.
    let mut waiting_for_key = false;
    let mut dropped: u32 = 0;
    let mut stats_at = Instant::now() + STATS_EVERY;
    let mut stats_bytes: u64 = 0;
    let mut stats_clock = Instant::now();
    loop {
        loop {
            let wait = next.saturating_duration_since(Instant::now());
            match commands.recv_timeout(wait) {
                Ok(Command::Quit) | Err(RecvTimeoutError::Disconnected) => return "quit".into(),
                Ok(Command::Publish { .. }) => {}
                Err(RecvTimeoutError::Timeout) => break,
            }
        }
        if let Some(why) = link.why_lost() {
            return format!("disconnected: {why}");
        }
        next += period;
        let now = Instant::now();
        if now > next + period * 4 {
            next = now + period;
        }
        if now >= stats_at {
            let total = link.sent.load(Ordering::Relaxed);
            let seconds = now.duration_since(stats_clock).as_secs_f64().max(0.001);
            let kbps = ((total - stats_bytes) as f64 * 8.0 / 1000.0 / seconds) as u32;
            stats_bytes = total;
            stats_clock = now;
            stats_at = now + STATS_EVERY;
            let _ = events.send(Event::Stats {
                seconds: clock.elapsed().as_secs() as u32,
                kbps,
                dropped,
            });
        }
        let frame = match source.next_frame() {
            Next::Picture(frame) => frame,
            Next::NotYet => continue,
            Next::Ended => return "window_closed".into(),
        };
        let force = key_wanted || since_key >= key_every;
        match encoder.encode(&frame, force) {
            Ok(encoded) => {
                if encoded.key {
                    since_key = 0;
                    key_wanted = false;
                    waiting_for_key = false;
                } else {
                    since_key += 1;
                }
                if waiting_for_key {
                    continue;
                }
                let packet = Packet {
                    ts_ms: clock.elapsed().as_millis() as u32,
                    key: encoded.key,
                    data: encoded.data,
                };
                match link.queue.try_send(packet) {
                    Ok(()) => {}
                    Err(TrySendError::Full(_)) => {
                        // The connection cannot keep up: drop until the next key frame, and ask
                        // for it now.
                        dropped += 1;
                        waiting_for_key = true;
                        key_wanted = true;
                    }
                    Err(TrySendError::Disconnected(_)) => {
                        let why = link
                            .why_lost()
                            .unwrap_or_else(|| "the connection ended".into());
                        return format!("disconnected: {why}");
                    }
                }
            }
            Err(CodecError::Pending) | Err(CodecError::BadFrame) => {}
            Err(error) => return format!("encoder_failed: {error}"),
        }
    }
}

#[cfg(test)]
mod tests;
