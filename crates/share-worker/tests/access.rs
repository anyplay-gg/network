//! The Screen Recording permission through the worker program, and (on macOS) the real encoder
//! through the clips recorder. None of it needs a screen or a permission: where the runner has
//! no media engine or the player has not allowed capture, the test says so and passes.

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const WORKER: &str = env!("CARGO_BIN_EXE_anyplay-share");

fn screen_access() -> String {
    let output = Command::new(WORKER)
        .arg("--screen-access")
        .output()
        .expect("the worker runs");
    assert!(output.status.success());
    let line = String::from_utf8(output.stdout).expect("text");
    let value: serde_json::Value = serde_json::from_str(line.trim()).expect("one JSON line");
    value["screen_access"].as_str().expect("a word").to_owned()
}

#[test]
fn the_worker_says_where_the_permission_stands_without_asking() {
    let word = screen_access();
    if cfg!(target_os = "macos") {
        assert!(["granted", "denied"].contains(&word.as_str()), "{word}");
    } else {
        assert_eq!(word, "not_needed");
    }
}

/// Clips for a process that has no window end with a reason the launcher understands, and never
/// hang: without the permission it is `no_permission`.
#[test]
fn clips_end_with_a_reason_the_launcher_knows() {
    let mut child = Command::new(WORKER)
        .args(["--clips", "15", "--pid", "4294967290"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("the worker runs");
    let stdout = child.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            let _ = tx.send(line);
        }
    });
    let deadline = Instant::now() + Duration::from_secs(40);
    let mut reason = None;
    while reason.is_none() {
        let left = deadline.saturating_duration_since(Instant::now());
        let Ok(line) = rx.recv_timeout(left) else {
            break;
        };
        let value: serde_json::Value = serde_json::from_str(&line).expect("JSON lines");
        if value["event"] == "ended" {
            reason = value["reason"].as_str().map(str::to_owned);
        }
    }
    let _ = child.kill();
    let _ = child.wait();
    let reason = reason.expect("the worker said why it ended");
    assert!(
        ["no_permission", "no_window", "no_encoder"].contains(&reason.as_str()),
        "{reason}"
    );
    if cfg!(target_os = "macos") && screen_access() == "denied" {
        assert_eq!(reason, "no_permission");
    }
}

#[cfg(target_os = "macos")]
mod encoder {
    use std::sync::mpsc;

    use anyplay_share_worker::clips::{Command, Event, recorder};
    use anyplay_share_worker::host::PatternSource;
    use anyplay_share_worker::limits::EncoderSetup;
    use anyplay_share_worker::mac::vt::VtEncoder;

    fn wait_for(events: &mpsc::Receiver<Event>, pick: impl Fn(&Event) -> bool) -> Event {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            let event = events.recv_timeout(left).expect("an event in time");
            if pick(&event) {
                return event;
            }
        }
    }

    /// The real VideoToolbox encoder behind the real clips recorder: the file is an MP4 with an
    /// H.264 track that starts on a key frame.
    #[test]
    fn a_clip_from_the_system_encoder_is_a_valid_mp4() {
        let setup = EncoderSetup {
            width: 640,
            height: 360,
            fps: 30,
            kbps: 2000,
        };
        let encoder = match VtEncoder::open_best(setup) {
            Ok(encoder) => encoder,
            Err(why) => {
                assert!(
                    !std::env::var("ANYPLAY_REQUIRE_VT").is_ok_and(|v| v == "1"),
                    "no encoder: {why}"
                );
                eprintln!("skipped: {why}");
                return;
            }
        };
        let mut recording = recorder::Setup::new(1);
        recording.width = setup.width;
        recording.height = setup.height;
        recording.fps = setup.fps;
        recording.kbps = setup.kbps;
        let (command_tx, command_rx) = mpsc::channel();
        let (event_tx, event_rx) = mpsc::channel();
        let thread = std::thread::spawn(move || {
            recorder::run(
                recording,
                Box::new(PatternSource::default()),
                Box::new(encoder),
                command_rx,
                event_tx,
            )
        });
        wait_for(&event_rx, |e| matches!(e, Event::Ready { .. }));
        // Two seconds of pictures: at least one whole group of pictures is kept.
        std::thread::sleep(std::time::Duration::from_millis(2500));
        let folder = std::env::temp_dir().join(format!("anyplay-mac-clip-{}", std::process::id()));
        let path = folder.join("clip.mp4");
        command_tx
            .send(Command::Save {
                path: path.to_string_lossy().into_owned(),
            })
            .unwrap();
        let saved = wait_for(&event_rx, |e| {
            matches!(e, Event::Saved { .. } | Event::SaveFailed { .. })
        });
        let Event::Saved { seconds, bytes, .. } = saved else {
            panic!("not saved: {saved:?}");
        };
        assert!(seconds >= 0.9, "{seconds}");
        assert!(bytes > 0);
        let data = std::fs::read(&path).expect("the clip exists");
        let size = data.len() as u64;
        let mut reader =
            mp4::Mp4Reader::read_header(std::io::Cursor::new(data), size).expect("a valid file");
        let track = reader.tracks().get(&1).expect("a video track");
        assert!(track.sample_count() >= 20, "{}", track.sample_count());
        assert!(reader.read_sample(1, 1).unwrap().unwrap().is_sync);
        command_tx.send(Command::Quit).unwrap();
        thread.join().unwrap();
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&folder);
    }
}
