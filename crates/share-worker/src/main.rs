//! `anyplay-share`: the share worker program.
//!
//! The net service starts it for a stream and ends it with the stream:
//!
//! ```text
//! anyplay-share --capabilities
//! anyplay-share --role host|guest [--link 127.0.0.1:PORT] [--token TOKEN]
//!               [--source game|test_pattern] [--title TITLE] [--window HWND] [--pid PID]...
//!               [--headless]
//! anyplay-share --clips SECONDS [--title TITLE] [--window HWND] [--pid PID]...
//! ```
//!
//! `--link` and `--token` default to `ANYPLAY_SHARE_ENDPOINT` and `ANYPLAY_SHARE_TOKEN`.
//! `--title` is the game's title: the viewer names its window after it. The streamer captures
//! the window the launcher named (`--window`), else the largest window of the game's processes
//! (`--pid`, repeated), and only without any process a window whose title contains `--title`.
//! `--clips` keeps the last SECONDS of the game's window and saves them on command (no link,
//! no network; JSON lines on standard input and output, see spec/clips.md).
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
    let mut clips: Option<u32> = None;
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
            "--clips" => clips = args.next().and_then(|v| v.parse().ok()),
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
    if let Some(seconds) = clips {
        return run_clips(seconds, &title, window, pids);
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
#[cfg(any(windows, target_os = "linux"))]
fn sink(title: &str, headless: bool) -> Box<dyn FrameSink> {
    if headless {
        return Box::new(anyplay_share_worker::viewer::NullSink);
    }
    let name = if title.is_empty() { "AnyPlay" } else { title };
    Box::new(anyplay_share_worker::window::WindowSink::new(&format!(
        "{name} - AnyPlay Share screen (no sound)"
    )))
}

#[cfg(not(any(windows, target_os = "linux")))]
fn sink(_title: &str, _headless: bool) -> Box<dyn FrameSink> {
    Box::new(anyplay_share_worker::viewer::NullSink)
}

/// Clips mode: records the game's window into a ring and answers the launcher's commands.
fn run_clips(seconds: u32, title: &str, window: Option<i64>, pids: Vec<u32>) -> ExitCode {
    use anyplay_share_worker::clips::{Command, Event, recorder};
    use std::io::{BufRead, Write};
    use std::sync::mpsc;

    let (event_tx, event_rx) = mpsc::channel::<Event>();
    let printer = std::thread::spawn(move || {
        let mut out = std::io::stdout();
        for event in event_rx {
            let _ = writeln!(out, "{}", event.to_line());
            let _ = out.flush();
        }
    });
    let end = |reason: &str, detail: String| {
        eprintln!("clips: {detail}");
        let _ = event_tx.send(Event::Ended {
            reason: reason.to_owned(),
        });
    };
    let mut setup = recorder::Setup::new(seconds);
    let encoder = match anyplay_share_worker::clips_encoder(&mut setup) {
        Ok(encoder) => encoder,
        Err(why) => {
            end("no_encoder", why);
            drop(event_tx);
            let _ = printer.join();
            return ExitCode::FAILURE;
        }
    };
    let source = match frame_source("game", title, window, pids) {
        Ok(source) => source,
        Err(why) => {
            end("no_window", why);
            drop(event_tx);
            let _ = printer.join();
            return ExitCode::FAILURE;
        }
    };
    let (command_tx, command_rx) = mpsc::channel::<Command>();
    // Standard input closing (the launcher ended) is a quit.
    std::thread::spawn(move || {
        for line in std::io::stdin().lock().lines().map_while(Result::ok) {
            if let Ok(command) = serde_json::from_str::<Command>(&line)
                && command_tx.send(command).is_err()
            {
                break;
            }
        }
    });
    recorder::run(setup, source, encoder, command_rx, event_tx);
    let _ = printer.join();
    ExitCode::SUCCESS
}
