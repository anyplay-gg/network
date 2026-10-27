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
        let mut recording = recorder::Setup::new(1);
        recording.width = setup.width;
        recording.height = setup.height;
        recording.fps = setup.fps;
        recording.kbps = setup.kbps;
        let (command_tx, command_rx) = mpsc::channel();
        let (event_tx, event_rx) = mpsc::channel();
        let (opened_tx, opened_rx) = mpsc::channel::<Result<(), String>>();
        // The encoder lives on the recorder's thread, as in the worker.
        let thread = std::thread::spawn(move || {
            let encoder = match VtEncoder::open_best(setup) {
                Ok(encoder) => {
                    let _ = opened_tx.send(Ok(()));
                    encoder
                }
                Err(why) => {
                    let _ = opened_tx.send(Err(why));
                    return;
                }
            };
            recorder::run(
                recording,
                Box::new(PatternSource::default()),
                Box::new(encoder),
                command_rx,
                event_tx,
            )
        });
        if let Err(why) = opened_rx.recv().unwrap() {
            assert!(
                !std::env::var("ANYPLAY_REQUIRE_VT").is_ok_and(|v| v == "1"),
                "no encoder: {why}"
            );
            eprintln!("skipped: {why}");
            return;
        }
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

/// Capture of a real window, on a Mac where the player (or the CI runner) has allowed Screen
/// Recording. It opens Calculator, so it runs only when `ANYPLAY_MAC_CAPTURE_TEST=1`.
#[cfg(target_os = "macos")]
mod real_window {
    use std::io::Write;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    use anyplay_share_worker::capture::{self, WindowTarget};
    use anyplay_share_worker::mac::sck::SckSound;
    use anyplay_share_worker::sound::SoundSource;

    use super::{WORKER, screen_access};

    /// One test at a time has Calculator: the next one starts it again after this one closed it.
    static ONE_AT_A_TIME: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Calculator, started for the test and closed after it.
    struct Calculator {
        pid: u32,
        started_here: bool,
        _turn: std::sync::MutexGuard<'static, ()>,
    }

    fn pid_of_calculator() -> Option<u32> {
        let out = Command::new("pgrep")
            .args(["-x", "Calculator"])
            .output()
            .ok()?;
        String::from_utf8(out.stdout)
            .ok()?
            .lines()
            .next()?
            .trim()
            .parse()
            .ok()
    }

    impl Calculator {
        /// `None` when this run is not asked to capture a window, or capture is not allowed.
        fn open() -> Option<Calculator> {
            if !std::env::var("ANYPLAY_MAC_CAPTURE_TEST").is_ok_and(|v| v == "1") {
                eprintln!("skipped: ANYPLAY_MAC_CAPTURE_TEST is not 1");
                return None;
            }
            assert_eq!(
                screen_access(),
                "granted",
                "the CI runner is expected to allow Screen Recording"
            );
            let turn = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
            let started_here = pid_of_calculator().is_none();
            Command::new("open")
                .args(["-a", "Calculator"])
                .status()
                .ok()?;
            let deadline = Instant::now() + Duration::from_secs(20);
            while Instant::now() < deadline {
                if let Some(pid) = pid_of_calculator() {
                    // The window needs a moment to be drawn.
                    std::thread::sleep(Duration::from_secs(3));
                    return Some(Calculator {
                        pid,
                        started_here,
                        _turn: turn,
                    });
                }
                std::thread::sleep(Duration::from_millis(250));
            }
            panic!("Calculator did not start");
        }
    }

    impl Drop for Calculator {
        fn drop(&mut self) {
            if self.started_here {
                let _ = Command::new("osascript")
                    .args(["-e", "quit app \"Calculator\""])
                    .status();
            }
        }
    }

    /// The window of another program is found by its process, captured by ScreenCaptureKit and
    /// the pictures are real (not one flat colour).
    #[test]
    fn a_real_window_is_found_and_its_pictures_have_content() {
        let Some(calculator) = Calculator::open() else {
            return;
        };
        let target = WindowTarget {
            window: None,
            pids: vec![calculator.pid],
            title_contains: String::new(),
        };
        let mut source = capture::open(&target).expect("the window is captured");
        let frame = source
            .wait_frame(Duration::from_secs(15))
            .expect("a picture arrives");
        assert!(frame.is_well_formed());
        assert!(
            frame.width >= 200 && frame.height >= 150,
            "{}x{}",
            frame.width,
            frame.height
        );
        let first = &frame.bgra[..4];
        assert!(
            frame.bgra.as_chunks::<4>().0.iter().any(|p| p != first),
            "the picture is one flat colour"
        );
        // A window that does not change gives its last picture again.
        let again = source.wait_frame(Duration::from_secs(5));
        assert!(again.is_some());
    }

    /// The worker program records a real window and saves a clip.
    #[test]
    fn the_worker_saves_a_clip_of_a_real_window() {
        let Some(calculator) = Calculator::open() else {
            return;
        };
        let folder = std::env::temp_dir().join(format!("anyplay-mac-real-{}", std::process::id()));
        let path = folder.join("real.mp4");
        let mut child = Command::new(WORKER)
            .args(["--clips", "2", "--pid", &calculator.pid.to_string()])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("the worker runs");
        let mut input = child.stdin.take().unwrap();
        let output = child.stdout.take().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            use std::io::BufRead;
            for line in std::io::BufReader::new(output)
                .lines()
                .map_while(Result::ok)
            {
                let _ = tx.send(line);
            }
        });
        let wait = |event: &str| {
            let deadline = Instant::now() + Duration::from_secs(40);
            loop {
                let left = deadline.saturating_duration_since(Instant::now());
                let line = rx.recv_timeout(left).expect("an event in time");
                let value: serde_json::Value = serde_json::from_str(&line).unwrap();
                if value["event"] == event {
                    return value;
                }
                assert_ne!(value["event"], "ended", "{value}");
            }
        };
        wait("ready");
        std::thread::sleep(Duration::from_millis(4500));
        writeln!(
            input,
            "{}",
            serde_json::json!({"do": "save", "path": path.to_string_lossy()})
        )
        .unwrap();
        let saved = wait("saved");
        assert!(saved["seconds"].as_f64().unwrap() >= 1.5, "{saved}");
        let data = std::fs::read(&path).expect("the clip exists");
        let size = data.len() as u64;
        let mut reader =
            mp4::Mp4Reader::read_header(std::io::Cursor::new(data), size).expect("a valid file");
        assert!(reader.tracks().get(&1).unwrap().sample_count() >= 30);
        assert!(reader.read_sample(1, 1).unwrap().unwrap().is_sync);
        let _ = writeln!(input, r#"{{"do":"quit"}}"#);
        drop(input);
        let _ = child.wait();
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&folder);
    }

    /// The game's sound: the application is listened to, and the stream stays up. (Calculator
    /// is silent; what arrives is silence or nothing, which is what the source must handle.)
    #[test]
    fn an_application_can_be_listened_to() {
        let Some(calculator) = Calculator::open() else {
            return;
        };
        let mut sound = SckSound::open(&[calculator.pid]).expect("the sound stream starts");
        let mut samples = Vec::new();
        for _ in 0..50 {
            assert!(sound.read(&mut samples).expect("reads"), "the stream ended");
        }
        // Whatever arrived is whole stereo frames of finite numbers.
        assert_eq!(samples.len() % 2, 0);
        assert!(samples.iter().all(|s| s.is_finite()));
    }
}
