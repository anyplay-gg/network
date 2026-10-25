//! Window capture on Linux against an ordinary window that this test opens: the player picks it
//! in the desktop portal's dialog, its pictures come from PipeWire, and they go through Cisco's
//! OpenH264 and back.
//!
//! It needs a desktop, a ScreenCast portal, a person to pick the window ("AnyPlay capture test")
//! and Cisco's library (`ANYPLAY_OPENH264`), so it is ignored by default. Run it with
//! `cargo test -p anyplay-share-worker --test capture_linux -- --ignored --nocapture`.

#![cfg(target_os = "linux")]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyplay_share::codec::{CodecError, Frame, VideoDecoder, VideoEncoder};
use anyplay_share_worker::capture::{self, WindowTarget};
use anyplay_share_worker::limits::EncoderSetup;
use anyplay_share_worker::oh264::{OhDecoder, OhEncoder};
use minifb::{Window, WindowOptions};

/// RGB of the window's content: a blue that cannot be mistaken for a border or the desktop.
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

fn centre(frame: &Frame) -> [u8; 3] {
    let (x, y) = (frame.width as usize / 2, frame.height as usize / 2);
    let at = (y * frame.width as usize + x) * 4;
    [frame.bgra[at + 2], frame.bgra[at + 1], frame.bgra[at]]
}

fn near(a: [u8; 3], rgb: u32, by: u8) -> bool {
    let want = [(rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8];
    a.iter().zip(want).all(|(&a, b)| a.abs_diff(b) <= by)
}

#[test]
#[ignore = "needs a desktop, the portal, a person to pick the window and Cisco's OpenH264"]
fn the_picked_window_is_captured_and_streamed_through_openh264() {
    let _window = TestWindow::open("AnyPlay capture test");
    std::thread::sleep(Duration::from_millis(500));
    eprintln!("pick the window \"AnyPlay capture test\" in the dialog");
    let mut source = capture::open(&WindowTarget::default()).expect("a window was picked");
    let deadline = Instant::now() + Duration::from_secs(5);
    let picture = loop {
        let frame = source.next_frame().expect("the capture did not end");
        if frame.width >= 320 && frame.height >= 180 {
            break frame;
        }
        assert!(Instant::now() < deadline, "no picture of the window");
    };
    eprintln!("captured {}x{}", picture.width, picture.height);
    assert!(
        near(centre(&picture), COLOUR, 6),
        "the middle of the window is {:?}",
        centre(&picture)
    );

    let setup = EncoderSetup {
        width: 1280,
        height: 720,
        fps: 30,
        kbps: 4000,
    };
    let mut encoder = OhEncoder::open(setup).expect("Cisco's OpenH264 (ANYPLAY_OPENH264)");
    let mut decoder = OhDecoder::open().expect("the OpenH264 decoder");
    let mut shown = None;
    for n in 0..30 {
        let frame = source.next_frame().expect("frames keep coming");
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
    assert!(
        near(centre(&out), COLOUR, 14),
        "the middle after the codec is {:?}",
        centre(&out)
    );
}
