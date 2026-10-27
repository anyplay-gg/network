//! `anyplay-share`: the share worker program.
//!
//! The net service starts it for a stream and ends it with the stream:
//!
//! ```text
//! anyplay-share --capabilities
//! anyplay-share --role host|guest [--link 127.0.0.1:PORT] [--token TOKEN]
//!               [--source game|test_pattern|ring] [--ring NAME] [--title TITLE]
//!               [--window HWND] [--pid PID]... [--headless] [--pad]
//! anyplay-share --clips SECONDS [--clips-dir DIR] [--title TITLE] [--window HWND] [--pid PID]...
//! anyplay-share --screen-access | --request-screen-access | --open-screen-settings
//! ```
//!
//! `--link` and `--token` default to `ANYPLAY_SHARE_ENDPOINT` and `ANYPLAY_SHARE_TOKEN`.
//! `--title` is the game's title: the viewer names its window after it. The streamer captures
//! the window the launcher named (`--window`), else the largest window of the game's processes
//! (`--pid`, repeated), and only without any process a window whose title contains `--title`.
//! `--clips-dir` confines the saves to that folder (the launcher's clips folder).
//! `--clips` keeps the last SECONDS of the game's window and saves them on command (no link,
//! no network; JSON lines on standard input and output, see spec/clips.md).
//! `--broadcast` publishes the game's window live to an RTMP server (no link; JSON lines on
//! standard input and output, see spec/broadcast.md); it takes `--width`, `--height`, `--fps`
//! and `--kbps`, and the server address and stream key come as the first line on standard input.
//! `--screen-access` prints where the Screen Recording permission stands (macOS; other systems
//! need none), `--request-screen-access` shows the system's prompt first and prints the same, and
//! `--open-screen-settings` opens the page of System Settings where the player switches it on.
//! Clips, broadcast and a stream end with `no_permission` while it is off.
//! `--headless` draws no window on the viewer's side (tests on a machine without a desktop).
//! `--source ring` is share play: the worker makes the frame ring `--ring NAME` (sized by
//! `--width`, `--height` and `--fps`), says so on the link and streams what the game's engine
//! writes into it. `--pad` lets the viewer's window read the guest's pad from the keyboard.

use std::path::PathBuf;
use std::process::ExitCode;

use anyplay_share::control::Role;
use anyplay_share::link::FromWorker;
use anyplay_share_worker::access;
use anyplay_share_worker::host::{self, FrameSource, PatternSource};
use anyplay_share_worker::sound::{SoundOut, SoundSource, ToneSource};
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
    let mut pad = false;
    let mut ring: Option<String> = None;
    let mut pids: Vec<u32> = Vec::new();
    let mut clips: Option<u32> = None;
    let mut clips_dir: Option<PathBuf> = None;
    let mut broadcast = false;
    let mut shape = (1280u32, 720u32, 30u32, 4500u32);
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
            "--screen-access" => {
                println!("{}", anyplay_share_worker::access::state().to_line());
                return ExitCode::SUCCESS;
            }
            "--request-screen-access" => {
                println!("{}", anyplay_share_worker::access::request().to_line());
                return ExitCode::SUCCESS;
            }
            "--open-screen-settings" => {
                return match anyplay_share_worker::access::open_settings() {
                    Ok(()) => ExitCode::SUCCESS,
                    Err(why) => {
                        eprintln!("{why}");
                        ExitCode::FAILURE
                    }
                };
            }
            "--clips" => clips = args.next().and_then(|v| v.parse().ok()),
            "--clips-dir" => clips_dir = args.next().map(PathBuf::from),
            "--broadcast" => broadcast = true,
            "--width" => shape.0 = args.next().and_then(|v| v.parse().ok()).unwrap_or(shape.0),
            "--height" => shape.1 = args.next().and_then(|v| v.parse().ok()).unwrap_or(shape.1),
            "--fps" => shape.2 = args.next().and_then(|v| v.parse().ok()).unwrap_or(shape.2),
            "--kbps" => shape.3 = args.next().and_then(|v| v.parse().ok()).unwrap_or(shape.3),
            "--link" => link = args.next(),
            "--token" => token = args.next(),
            "--source" => source = args.next().unwrap_or_default(),
            "--title" => title = args.next().unwrap_or_default(),
            "--headless" => headless = true,
            "--pad" => pad = true,
            "--ring" => ring = args.next(),
            "--window" => window = args.next().and_then(|v| v.parse().ok()),
            "--pid" => pids.extend(args.next().and_then(|v| v.parse::<u32>().ok())),
            _ => {}
        }
    }
    if let Some(seconds) = clips {
        return run_clips(seconds, clips_dir, &title, window, pids);
    }
    if broadcast {
        return run_broadcast(shape, &title, window, pids);
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
        Role::Host if source == "ring" => {
            let Some(name) = ring else {
                return fail(port, "share play needs --ring NAME".into());
            };
            match anyplay_share_worker::ring_source::create(&name, shape.0, shape.1, shape.2) {
                Ok((frames, sound, ready)) => {
                    // The net service hands the ring to the game when it reads this.
                    let _ = port.to_service.send(FromWorker::RingReady(ready));
                    host::run_with_sound(
                        Box::new(frames),
                        Box::new(encoder_for),
                        Some(Box::new(sound)),
                        port,
                    );
                }
                Err(reason) => return fail(port, format!("the frame ring: {reason}")),
            }
        }
        Role::Host => {
            if source == "game" && !access::state().allows_capture() {
                return fail(port, access::EXPLANATION.into());
            }
            let sound = sound_source(&source, &pids);
            match frame_source(&source, &title, window, pids) {
                Ok(frames) => host::run_with_sound(frames, Box::new(encoder_for), sound, port),
                Err(reason) => return fail(port, reason),
            }
        }
        Role::Guest => {
            let decoders: DecoderFactory = Box::new(decoder_for);
            let speakers = if headless { None } else { speakers() };
            let sink = sink(&title, headless, speakers.is_some(), pad);
            viewer::run_with_sound(decoders, sink, speakers, port);
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

/// Where the streamer's sound comes from: the game's processes on Windows, the game's
/// applications on macOS, a tone for the test pattern. None where this build cannot capture it: the stream then has no sound.
fn sound_source(source: &str, pids: &[u32]) -> Option<Box<dyn SoundSource>> {
    match source {
        "test_pattern" => Some(Box::new(ToneSource::default())),
        #[cfg(windows)]
        "game" => {
            let pid = *pids.first()?;
            match anyplay_share_worker::loopback::LoopbackSource::open(pid) {
                Ok(source) => Some(Box::new(source)),
                Err(why) => {
                    eprintln!("share worker: no sound: {why}");
                    None
                }
            }
        }
        #[cfg(target_os = "macos")]
        "game" => match anyplay_share_worker::mac::sck::SckSound::open(pids) {
            Ok(source) => Some(Box::new(source)),
            Err(why) => {
                eprintln!("share worker: no sound: {why}");
                None
            }
        },
        _ => {
            let _ = pids;
            None
        }
    }
}

/// The viewer's speakers, where this build plays sound (Windows).
fn speakers() -> Option<Box<dyn SoundOut>> {
    #[cfg(windows)]
    match anyplay_share_worker::speakers::Speakers::open() {
        Ok(speakers) => return Some(Box::new(speakers)),
        Err(why) => eprintln!("share worker: no sound: {why}"),
    }
    None
}

/// The viewer's window: "title" and, without speakers, a note that there is no sound.
#[cfg(any(windows, target_os = "linux"))]
fn sink(title: &str, headless: bool, sound: bool, pad: bool) -> Box<dyn FrameSink> {
    if headless {
        return Box::new(anyplay_share_worker::viewer::NullSink);
    }
    let name = if title.is_empty() { "AnyPlay" } else { title };
    let note = if sound { "" } else { " (no sound)" };
    let kind = if pad { "Share play" } else { "Share screen" };
    let window =
        anyplay_share_worker::window::WindowSink::new(&format!("{name} - AnyPlay {kind}{note}"));
    Box::new(if pad { window.with_pad() } else { window })
}

#[cfg(not(any(windows, target_os = "linux")))]
fn sink(_title: &str, _headless: bool, _sound: bool, _pad: bool) -> Box<dyn FrameSink> {
    Box::new(anyplay_share_worker::viewer::NullSink)
}

/// Clips mode: records the game's window into a ring and answers the launcher's commands.
fn run_clips(
    seconds: u32,
    folder: Option<PathBuf>,
    title: &str,
    window: Option<i64>,
    pids: Vec<u32>,
) -> ExitCode {
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
    if !access::state().allows_capture() {
        end(access::NO_PERMISSION, access::EXPLANATION.into());
        drop(event_tx);
        let _ = printer.join();
        return ExitCode::FAILURE;
    }
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
            end(access::open_failure_reason(&why), why);
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
    recorder::run_in(folder, setup, source, encoder, command_rx, event_tx);
    let _ = printer.join();
    ExitCode::SUCCESS
}

/// Broadcast mode: publishes the game's window to the server named on the first input line.
fn run_broadcast(
    shape: (u32, u32, u32, u32),
    title: &str,
    window: Option<i64>,
    pids: Vec<u32>,
) -> ExitCode {
    use anyplay_share_worker::broadcast::{self, Command, Event};
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
        eprintln!("broadcast: {detail}");
        let _ = event_tx.send(Event::Ended {
            reason: reason.to_owned(),
        });
    };
    // The first line says where to publish. The key stays off the command line.
    let mut first = String::new();
    let _ = std::io::stdin().lock().read_line(&mut first);
    let Ok(Command::Publish { url, key }) = serde_json::from_str::<Command>(first.trim()) else {
        end("quit", "no address came on the input".into());
        drop(event_tx);
        let _ = printer.join();
        return ExitCode::FAILURE;
    };
    if !access::state().allows_capture() {
        end(access::NO_PERMISSION, access::EXPLANATION.into());
        drop(event_tx);
        let _ = printer.join();
        return ExitCode::FAILURE;
    }
    let mut setup = broadcast::Setup {
        width: shape.0,
        height: shape.1,
        fps: shape.2,
        kbps: shape.3,
    };
    let encoder = match anyplay_share_worker::broadcast_encoder(&mut setup) {
        Ok(encoder) => encoder,
        Err(why) => {
            end("no_encoder", why);
            drop(event_tx);
            let _ = printer.join();
            return ExitCode::FAILURE;
        }
    };
    let sound = broadcast_sound(pids.first().copied());
    let source = match frame_source("game", title, window, pids) {
        Ok(source) => source,
        Err(why) => {
            end(access::open_failure_reason(&why), why);
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
    broadcast::run_with_sound(
        setup, &url, &key, source, encoder, sound, None, command_rx, event_tx,
    );
    let _ = printer.join();
    ExitCode::SUCCESS
}

/// The game's sound for a broadcast: the game's process tree on Windows (never the microphone or
/// other programs), AAC from the system's encoder. Made on the sound thread; where it cannot be
/// made, or on a system without it, the broadcast carries silence.
fn broadcast_sound(
    pid: Option<u32>,
) -> Option<anyplay_share_worker::broadcast::audio::SoundFactory> {
    #[cfg(windows)]
    {
        use anyplay_share_worker::broadcast::aac::MfAac;
        use anyplay_share_worker::broadcast::audio::AudioEncoder;
        use anyplay_share_worker::loopback::LoopbackSource;
        let pid = pid?;
        Some(Box::new(move || {
            let source = LoopbackSource::open(pid)?;
            let encoder = MfAac::open()?;
            Ok((
                Box::new(source) as Box<dyn SoundSource>,
                Box::new(encoder) as Box<dyn AudioEncoder>,
            ))
        }))
    }
    #[cfg(not(windows))]
    {
        let _ = pid;
        None
    }
}
