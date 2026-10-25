//! The recorder loop: a picture from the source, through the encoder, into the ring; and a save
//! on request.

use std::fs::File;
use std::io::BufWriter;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use anyplay_share::codec::{CodecError, VideoEncoder};

use super::mux::{self, MuxError};
use super::ring::{ClipFrame, Ring};
use super::{Command, Event, KEY_SECONDS, MAX_BYTES};
use crate::host::{FrameSource, Next};

/// What a recording is made of.
#[derive(Clone, Copy, Debug)]
pub struct Setup {
    /// How many seconds a clip holds at least.
    pub seconds: u32,
    /// The size, rate and bit rate the encoder was opened with.
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub kbps: u32,
    pub max_bytes: usize,
}

impl Setup {
    pub fn new(seconds: u32) -> Setup {
        Setup {
            seconds: seconds.max(1),
            width: super::WIDTH,
            height: super::HEIGHT,
            fps: super::FPS,
            kbps: super::KBPS,
            max_bytes: MAX_BYTES,
        }
    }
}

/// Records until told to quit, the source ends or the launcher hangs up.
pub fn run(
    setup: Setup,
    mut source: Box<dyn FrameSource>,
    mut encoder: Box<dyn VideoEncoder>,
    commands: Receiver<Command>,
    events: Sender<Event>,
) {
    let _ = events.send(Event::Ready {
        width: setup.width,
        height: setup.height,
        fps: setup.fps,
        kbps: setup.kbps,
    });
    let period = Duration::from_secs_f64(1.0 / f64::from(setup.fps.clamp(1, 60)));
    let key_every = setup.fps.max(1) * KEY_SECONDS;
    let mut ring = Ring::new(u64::from(setup.seconds) * 1_000_000, setup.max_bytes);
    let clock = Instant::now();
    let mut next = Instant::now();
    let mut since_key = key_every;
    let mut key_wanted = true;
    loop {
        // Wait for the next picture's time, taking commands as they come.
        loop {
            let wait = next.saturating_duration_since(Instant::now());
            match commands.recv_timeout(wait) {
                Ok(Command::Save { path }) => save(&ring, &setup, path, &events),
                Ok(Command::Quit) | Err(RecvTimeoutError::Disconnected) => {
                    let _ = events.send(Event::Ended {
                        reason: "quit".into(),
                    });
                    return;
                }
                Err(RecvTimeoutError::Timeout) => break,
            }
        }
        next += period;
        let now = Instant::now();
        if now > next + period * 4 {
            // Far behind (the PC stalled): do not rush a burst of pictures to catch up.
            next = now + period;
        }
        let frame = match source.next_frame() {
            Next::Picture(frame) => frame,
            // Back to the commands: a quit gets through while the window has not drawn.
            Next::NotYet => continue,
            Next::Ended => {
                let _ = events.send(Event::Ended {
                    reason: "window_closed".into(),
                });
                return;
            }
        };
        let force = key_wanted || since_key >= key_every;
        match encoder.encode(&frame, force) {
            Ok(encoded) => {
                if encoded.key {
                    since_key = 0;
                    key_wanted = false;
                } else {
                    since_key += 1;
                }
                ring.push(ClipFrame {
                    key: encoded.key,
                    pts_us: clock.elapsed().as_micros() as u64,
                    data: encoded.data.into(),
                });
            }
            // The encoder keeps the picture and answers later; a key frame asked for is still
            // wanted.
            Err(CodecError::Pending) | Err(CodecError::BadFrame) => {}
            Err(error) => {
                let _ = events.send(Event::Ended {
                    reason: format!("encoder_failed: {error}"),
                });
                return;
            }
        }
    }
}

/// Writes the ring to `path` on a thread of its own, so that the recording goes on. The file is
/// written under a temporary name and renamed, so that a folder never holds half a clip.
fn save(ring: &Ring, setup: &Setup, path: String, events: &Sender<Event>) {
    let frames = ring.snapshot();
    let (width, height) = (setup.width, setup.height);
    let events = events.clone();
    std::thread::spawn(move || {
        let event = match write_file(Path::new(&path), &frames, width, height) {
            Ok((seconds, bytes)) => Event::Saved {
                path,
                seconds,
                bytes,
            },
            Err(MuxError::NoKeyFrame) => Event::SaveFailed {
                reason: "empty".into(),
            },
            Err(error) => Event::SaveFailed {
                reason: error.to_string(),
            },
        };
        let _ = events.send(event);
    });
}

/// Returns the clip's length in seconds and its size.
fn write_file(
    path: &Path,
    frames: &[ClipFrame],
    width: u32,
    height: u32,
) -> Result<(f64, u64), MuxError> {
    if let Some(folder) = path.parent() {
        std::fs::create_dir_all(folder)?;
    }
    let mut temporary: PathBuf = path.into();
    temporary.as_mut_os_string().push(".part");
    let result = (|| {
        let file = File::create(&temporary)?;
        let mut out = BufWriter::new(file);
        mux::write_mp4(&mut out, frames, width, height)?;
        out.into_inner().map_err(|e| e.into_error())?.sync_all()?;
        std::fs::rename(&temporary, path)?;
        Ok(())
    })();
    if let Err(error) = result {
        let _ = std::fs::remove_file(&temporary);
        return Err(error);
    }
    let seconds = match (frames.first(), frames.last()) {
        (Some(a), Some(b)) => (b.pts_us - a.pts_us) as f64 / 1e6,
        _ => 0.0,
    };
    Ok((seconds, std::fs::metadata(path).map_or(0, |m| m.len())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clips::mux::tests::picture;
    use anyplay_share::codec::{EncodedFrame, Frame};
    use anyplay_share::testcodec::TestPattern;
    use std::sync::mpsc;

    /// Sources a picture and encodes it as a fake H.264 stream: a key frame when asked.
    struct FakeEncoder {
        count: u8,
        keys: std::sync::Arc<std::sync::atomic::AtomicU32>,
    }

    impl VideoEncoder for FakeEncoder {
        fn codec(&self) -> &str {
            "h264"
        }
        fn encode(&mut self, frame: &Frame, force_key: bool) -> Result<EncodedFrame, CodecError> {
            self.count = self.count.wrapping_add(1);
            if force_key {
                self.keys.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            Ok(EncodedFrame {
                key: force_key,
                timestamp_us: frame.timestamp_us,
                data: picture(force_key, self.count),
            })
        }
    }

    struct Pattern(TestPattern);
    impl FrameSource for Pattern {
        fn next_frame(&mut self) -> Next {
            Next::Picture(self.0.next_frame())
        }
    }

    fn setup() -> Setup {
        Setup {
            seconds: 1,
            width: 320,
            height: 180,
            fps: 60,
            kbps: 1000,
            max_bytes: usize::MAX,
        }
    }

    fn start(
        setup: Setup,
    ) -> (
        mpsc::Sender<Command>,
        mpsc::Receiver<Event>,
        std::thread::JoinHandle<()>,
        std::sync::Arc<std::sync::atomic::AtomicU32>,
    ) {
        let (command_tx, command_rx) = mpsc::channel();
        let (event_tx, event_rx) = mpsc::channel();
        let keys = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let encoder = FakeEncoder {
            count: 0,
            keys: keys.clone(),
        };
        let thread = std::thread::spawn(move || {
            run(
                setup,
                Box::new(Pattern(TestPattern::default())),
                Box::new(encoder),
                command_rx,
                event_tx,
            )
        });
        (command_tx, event_rx, thread, keys)
    }

    fn wait_for(events: &mpsc::Receiver<Event>, pick: impl Fn(&Event) -> bool) -> Event {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let event = events.recv_timeout(left).expect("an event in time");
            if pick(&event) {
                return event;
            }
        }
    }

    #[test]
    fn a_save_writes_an_mp4_that_starts_on_a_key_frame() {
        let (commands, events, thread, keys) = start(setup());
        assert!(matches!(
            wait_for(&events, |_| true),
            Event::Ready { width: 320, .. }
        ));
        // Let a few key frames' worth of pictures in: 60 pictures a second, a key frame every 120.
        std::thread::sleep(Duration::from_millis(2600));
        let folder = tempdir("save");
        let path = folder.join("clip.mp4");
        commands
            .send(Command::Save {
                path: path.to_string_lossy().into_owned(),
            })
            .unwrap();
        let saved = wait_for(&events, |e| matches!(e, Event::Saved { .. }));
        let Event::Saved { seconds, bytes, .. } = saved else {
            unreachable!()
        };
        assert!(seconds >= 0.9, "{seconds}");
        assert!(bytes > 0);
        assert!(keys.load(std::sync::atomic::Ordering::Relaxed) >= 2);
        let data = std::fs::read(&path).expect("the clip exists");
        let size = data.len() as u64;
        let reader =
            mp4::Mp4Reader::read_header(std::io::Cursor::new(data), size).expect("a valid file");
        assert!(reader.tracks().get(&1).unwrap().sample_count() >= 60);
        assert!(!folder.join("clip.mp4.part").exists());
        commands.send(Command::Quit).unwrap();
        assert!(matches!(
            wait_for(&events, |e| matches!(e, Event::Ended { .. })),
            Event::Ended { reason } if reason == "quit"
        ));
        thread.join().unwrap();
    }

    #[test]
    fn a_save_that_cannot_be_written_is_reported_and_the_recorder_goes_on() {
        let mut setup = setup();
        setup.fps = 10;
        let (commands, events, thread, _) = start(setup);
        // A folder named like a file cannot be created.
        std::thread::sleep(Duration::from_millis(300));
        let folder = tempdir("fail");
        let blocked = folder.join("file");
        std::fs::write(&blocked, b"x").unwrap();
        commands
            .send(Command::Save {
                path: blocked.join("clip.mp4").to_string_lossy().into_owned(),
            })
            .unwrap();
        assert!(matches!(
            wait_for(&events, |e| matches!(e, Event::SaveFailed { .. })),
            Event::SaveFailed { .. }
        ));
        commands.send(Command::Quit).unwrap();
        thread.join().unwrap();
    }

    #[test]
    fn the_commands_and_events_are_json_lines() {
        let save: Command = serde_json::from_str(r#"{"do":"save","path":"C:/a.mp4"}"#).unwrap();
        assert_eq!(
            save,
            Command::Save {
                path: "C:/a.mp4".into()
            }
        );
        let quit: Command = serde_json::from_str(r#"{"do":"quit"}"#).unwrap();
        assert_eq!(quit, Command::Quit);
        let line = Event::Saved {
            path: "a".into(),
            seconds: 1.5,
            bytes: 9,
        }
        .to_line();
        assert_eq!(
            line,
            r#"{"event":"saved","path":"a","seconds":1.5,"bytes":9}"#
        );
        assert_eq!(
            Event::Ended {
                reason: "quit".into()
            }
            .to_line(),
            r#"{"event":"ended","reason":"quit"}"#
        );
    }

    fn tempdir(name: &str) -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("anyplay-clips-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        path
    }
}
