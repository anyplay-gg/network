//! Window capture against an ordinary window that this test opens: the window is found by the
//! test's own process (not by its title), captured with Windows Graphics Capture, and the
//! picture goes through the H.264 encoder and decoder of the system.
//!
//! It needs a desktop that is shown, so it is ignored by default (a build machine without one
//! cannot capture). Run it with `cargo test -p anyplay-share-worker --test capture -- --ignored`.

#![cfg(windows)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyplay_share::codec::{CodecError, Frame, VideoDecoder, VideoEncoder};
use anyplay_share_worker::capture::{self, WindowTarget};
use anyplay_share_worker::mf::{EncoderSetup, Kind, MfDecoder, MfEncoder};
use minifb::{Window, WindowOptions};

/// RGB of the window's content: a blue that cannot be mistaken for the title bar or the desktop.
const COLOUR: u32 = 0x2060c0;

struct TestWindow {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl TestWindow {
    /// Opens a window of 640 by 360 filled with [`COLOUR`], and keeps it alive on a thread.
    fn open(title: &str) -> TestWindow {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let title = title.to_owned();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || {
            let mut window = Window::new(&title, 640, 360, WindowOptions::default())
                .expect("a window can be opened on this desktop");
            window.set_target_fps(0);
            let buffer = vec![COLOUR; 640 * 360];
            let _ = ready_tx.send(());
            while !flag.load(Ordering::Relaxed) && window.is_open() {
                window
                    .update_with_buffer(&buffer, 640, 360)
                    .expect("window update");
                std::thread::sleep(Duration::from_millis(15));
            }
        });
        ready_rx.recv().expect("the window thread started");
        TestWindow {
            stop,
            thread: Some(thread),
        }
    }
}

impl Drop for TestWindow {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// The first picture the source gives that is not smaller than the window's content.
fn first_picture(source: &mut dyn anyplay_share_worker::host::FrameSource) -> Frame {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let frame = source
            .wait_frame(Duration::from_secs(5))
            .expect("the capture did not end");
        if frame.width >= 640 && frame.height >= 360 {
            return frame;
        }
        assert!(Instant::now() < deadline, "no picture of the window");
    }
}

fn centre(frame: &Frame) -> [u8; 3] {
    let (x, y) = (frame.width as usize / 2, frame.height as usize / 2 + 12);
    let at = (y * frame.width as usize + x) * 4;
    [frame.bgra[at + 2], frame.bgra[at + 1], frame.bgra[at]]
}

fn near(a: [u8; 3], rgb: u32) -> bool {
    let want = [(rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8];
    a.iter().zip(want).all(|(&a, b)| a.abs_diff(b) <= 6)
}

#[test]
#[ignore = "needs a desktop that is shown"]
fn the_window_of_a_process_is_found_captured_and_streamed() {
    let _window = TestWindow::open("A window that is not the game");
    // The title below matches nothing: the window can only be found through the process.
    let target = WindowTarget {
        window: None,
        pids: vec![std::process::id()],
        title_contains: "this title is not on any window".into(),
    };
    let mut source = capture::open(&target).expect("the window of this process is found");
    let picture = first_picture(source.as_mut());
    assert!(
        near(centre(&picture), COLOUR),
        "the middle of the window is {:?}",
        centre(&picture)
    );

    // Through the system's encoder and decoder at 1280x720: the window's shape is kept.
    let setup = EncoderSetup {
        width: 1280,
        height: 720,
        fps: 30,
        kbps: 4000,
    };
    let mut encoder = MfEncoder::open_best(setup).expect("an H.264 encoder");
    let mut decoder = MfDecoder::open().expect("an H.264 decoder");
    let mut shown = None;
    for n in 0..30 {
        let mut frame = source
            .wait_frame(Duration::from_secs(5))
            .expect("frames keep coming");
        frame.timestamp_us = n * 33_333;
        match encoder.encode(&frame, n == 0) {
            Ok(encoded) => match decoder.decode(&encoded) {
                Ok(out) => shown = Some(out),
                Err(CodecError::Pending | CodecError::MissingReference) => {}
                Err(other) => panic!("decode: {other}"),
            },
            Err(CodecError::Pending) => {}
            Err(other) => panic!("encode: {other}"),
        }
    }
    let out = shown.expect("a picture came out of the decoder");
    assert_eq!((out.width, out.height), (1280, 720));
    let (x, y) = (640usize, 360usize);
    let at = (y * 1280 + x) * 4;
    let rgb = [out.bgra[at + 2], out.bgra[at + 1], out.bgra[at]];
    assert!(
        rgb.iter()
            .zip([0x20u8, 0x60, 0xc0])
            .all(|(&a, b)| a.abs_diff(b) <= 14),
        "the middle after the codec is {rgb:?}"
    );
}

#[test]
#[ignore = "needs a desktop that is shown"]
fn a_window_of_another_process_is_not_taken_when_processes_are_named() {
    let _window = TestWindow::open("A window of this process");
    // Process 4 is the system; its windows (none) are the only ones that would count. The
    // search gives up instead of falling back to the title.
    let target = WindowTarget {
        window: None,
        pids: vec![4],
        title_contains: "A window of this process".into(),
    };
    assert!(capture::find_window(&target).is_err());
}

#[test]
#[ignore = "needs a desktop that is shown"]
fn without_processes_the_title_is_the_fallback() {
    let _window = TestWindow::open("AnyPlay fallback title window");
    let target = WindowTarget {
        window: None,
        pids: Vec::new(),
        title_contains: "AnyPlay fallback title window".into(),
    };
    assert!(capture::find_window(&target).is_ok());
}

#[test]
fn a_software_encoder_alone_is_enough_to_stream() {
    // No desktop needed: the system's software encoder exists on every Windows with Media
    // Foundation; where it does not, there is nothing to test.
    let setup = EncoderSetup {
        width: 320,
        height: 180,
        fps: 30,
        kbps: 1000,
    };
    let Ok(mut encoder) = MfEncoder::open(setup, Kind::Software) else {
        return;
    };
    let frame = Frame {
        width: 320,
        height: 180,
        timestamp_us: 0,
        bgra: vec![128; 320 * 180 * 4],
    };
    let mut got = false;
    for n in 0..10 {
        let mut f = frame.clone();
        f.timestamp_us = n * 33_333;
        got |= encoder.encode(&f, n == 0).is_ok();
    }
    assert!(got);
}

/// The worker program in clips mode, against a window of this process: it reports `ready`, keeps
/// the window's pictures, saves an MP4 file on `save` and ends on `quit`. Without a hardware
/// encoder it ends with `no_encoder`, which is also an answer the spec allows, so the test stops.
#[test]
#[ignore = "needs a desktop that is shown"]
fn the_clips_worker_saves_the_window_as_an_mp4_file() {
    use std::io::{BufRead, BufReader, Write};
    use std::process::{Command, Stdio};

    let _window = TestWindow::open("A window to clip");
    let mut child = Command::new(env!("CARGO_BIN_EXE_anyplay-share"))
        .args(["--clips", "15", "--pid", &std::process::id().to_string()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("the worker starts");
    let mut input = child.stdin.take().unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let first = lines.next().expect("an event").expect("a line");
    if first.contains("no_encoder") {
        eprintln!("no hardware H.264 encoder here: {first}");
        let _ = child.wait();
        return;
    }
    assert!(first.contains("\"event\":\"ready\""), "{first}");
    std::thread::sleep(Duration::from_secs(5));
    let path = std::env::temp_dir().join(format!("anyplay-clip-test-{}.mp4", std::process::id()));
    let command = serde_json::json!({"do": "save", "path": path.to_string_lossy()});
    writeln!(input, "{command}").unwrap();
    let saved = lines.next().expect("an event").expect("a line");
    assert!(saved.contains("\"event\":\"saved\""), "{saved}");
    writeln!(input, r#"{{"do":"quit"}}"#).unwrap();
    assert!(child.wait().unwrap().success());

    let data = std::fs::read(&path).expect("the clip exists");
    let _ = std::fs::remove_file(&path);
    let size = data.len() as u64;
    let reader = mp4::Mp4Reader::read_header(std::io::Cursor::new(data), size).expect("an MP4");
    let track = reader.tracks().get(&1).expect("a video track");
    assert!(track.sample_count() >= 60, "{}", track.sample_count());
    assert!(reader.duration().as_secs_f64() >= 3.0);
}
