//! `anyplay-share`: the share worker program.
//!
//! The net service starts it for a stream and ends it with the stream:
//!
//! ```text
//! anyplay-share --capabilities
//! anyplay-share --role host|guest [--link 127.0.0.1:PORT] [--token TOKEN]
//!               [--source game|test_pattern] [--title TITLE] [--window HWND] [--pid PID]...
//!               [--headless]
//! ```
//!
//! `--link` and `--token` default to `ANYPLAY_SHARE_ENDPOINT` and `ANYPLAY_SHARE_TOKEN`.
//! `--title` is the game's title: the viewer names its window after it. The streamer captures
//! the window the launcher named (`--window`), else the largest window of the game's processes
//! (`--pid`, repeated), and only without any process a window whose title contains `--title`.
//! `--headless` draws no window on the viewer's side (tests on a machine without a desktop).

use std::process::ExitCode;

use anyplay_share::control::Role;
use anyplay_share::link::FromWorker;
use anyplay_share_worker::host::{self, FrameSource, PatternSource};
use anyplay_share_worker::viewer::{self, DecoderFactory, FrameSink};
use anyplay_share_worker::{Port, capabilities, decoder_for, encoder_for, socket};

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let mut role = None;
    let mut link = std::env::var("ANYPLAY_SHARE_ENDPOINT").ok();
    let mut token = std::env::var("ANYPLAY_SHARE_TOKEN").ok();
    let mut source = "game".to_owned();
    let mut title = String::new();
    let mut window: Option<i64> = None;
    let mut headless = false;
    let mut pids: Vec<u32> = Vec::new();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--capabilities" => {
                println!(
                    "{}",
                    serde_json::to_string(&capabilities()).unwrap_or_default()
                );
                return ExitCode::SUCCESS;
            }
            "--role" => {
                role = match args.next().as_deref() {
                    Some("host") => Some(Role::Host),
                    Some("guest") => Some(Role::Guest),
                    _ => None,
                }
            }
            "--link" => link = args.next(),
            "--token" => token = args.next(),
            "--source" => source = args.next().unwrap_or_default(),
            "--title" => title = args.next().unwrap_or_default(),
            "--headless" => headless = true,
            "--window" => window = args.next().and_then(|v| v.parse().ok()),
            "--pid" => pids.extend(args.next().and_then(|v| v.parse::<u32>().ok())),
            _ => {}
        }
    }
    let (Some(role), Some(link), Some(token)) = (role, link, token) else {
        eprintln!("usage: anyplay-share --role host|guest --link ADDR --token TOKEN");
        return ExitCode::FAILURE;
    };
    let port = match socket::connect(&link, &token, role) {
        Ok(port) => port,
        Err(error) => {
            eprintln!("the share link failed: {error}");
            return ExitCode::FAILURE;
        }
    };
    match role {
        Role::Host => match frame_source(&source, &title, window, pids) {
            Ok(frames) => host::run_with(frames, Box::new(encoder_for), port),
            Err(reason) => return fail(port, reason),
        },
        Role::Guest => {
            let decoders: DecoderFactory = Box::new(decoder_for);
            viewer::run(decoders, sink(&title, headless), port);
        }
    }
    ExitCode::SUCCESS
}

/// Tells the net service why the worker cannot do what it was started for.
fn fail(port: Port, reason: String) -> ExitCode {
    let _ = port.to_service.send(FromWorker::Ended(reason));
    // The net service reads the message before it sees the link close.
    std::thread::sleep(std::time::Duration::from_millis(200));
    ExitCode::FAILURE
}

fn frame_source(
    source: &str,
    title: &str,
    window: Option<i64>,
    pids: Vec<u32>,
) -> Result<Box<dyn FrameSource>, String> {
    match source {
        "test_pattern" => Ok(Box::new(PatternSource::default())),
        "game" => {
            anyplay_share_worker::capture::open(&anyplay_share_worker::capture::WindowTarget {
                window,
                pids,
                title_contains: title.to_owned(),
            })
        }
        _ => Err("this build of the share worker cannot capture a window".into()),
    }
}

/// The viewer's window: "title" and, until the game's sound is carried, a note that there is
/// none.
#[cfg(windows)]
fn sink(title: &str, headless: bool) -> Box<dyn FrameSink> {
    if headless {
        return Box::new(anyplay_share_worker::viewer::NullSink);
    }
    let name = if title.is_empty() { "AnyPlay" } else { title };
    Box::new(anyplay_share_worker::window::WindowSink::new(&format!(
        "{name} - AnyPlay Share screen (no sound)"
    )))
}

#[cfg(not(windows))]
fn sink(_title: &str, _headless: bool) -> Box<dyn FrameSink> {
    Box::new(anyplay_share_worker::viewer::NullSink)
}
