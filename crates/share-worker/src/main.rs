//! `anyplay-share`: the share worker program.
//!
//! The net service starts it for a stream and ends it with the stream:
//!
//! ```text
//! anyplay-share --capabilities
//! anyplay-share --role host|guest [--link 127.0.0.1:PORT] [--token TOKEN]
//!               [--source game|test_pattern] [--title TITLE]
//! ```
//!
//! `--link` and `--token` default to `ANYPLAY_SHARE_ENDPOINT` and `ANYPLAY_SHARE_TOKEN`. `--title`
//! is the game's title: the streamer captures the window whose title contains it, and the
//! viewer names its window after it.

use std::process::ExitCode;

use anyplay_share::codec::VideoDecoder;
use anyplay_share::control::Role;
use anyplay_share::link::FromWorker;
use anyplay_share::testcodec::{TestDecoder, TestEncoder};
use anyplay_share_worker::host::{self, FrameSource, PatternSource};
use anyplay_share_worker::viewer::{self, DecoderFactory, FrameSink};
use anyplay_share_worker::{Port, capabilities, socket};

/// A key frame at least this often, so that a viewer who joins late waits at most 2 seconds.
const KEY_INTERVAL: u32 = 120;

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let mut role = None;
    let mut link = std::env::var("ANYPLAY_SHARE_ENDPOINT").ok();
    let mut token = std::env::var("ANYPLAY_SHARE_TOKEN").ok();
    let mut source = "game".to_owned();
    let mut title = String::new();
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
        Role::Host => match frame_source(&source, &title) {
            Ok(frames) => {
                host::run(frames, Box::new(TestEncoder::new(KEY_INTERVAL)), port);
            }
            Err(reason) => return fail(port, reason),
        },
        Role::Guest => {
            let decoders: DecoderFactory = Box::new(|codec| match codec {
                "test" => Some(Box::new(TestDecoder::new()) as Box<dyn VideoDecoder>),
                _ => None,
            });
            viewer::run(decoders, sink(&title), port);
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

fn frame_source(source: &str, title: &str) -> Result<Box<dyn FrameSource>, String> {
    match source {
        "test_pattern" => Ok(Box::new(PatternSource::default())),
        #[cfg(feature = "capture")]
        "game" => {
            anyplay_share_worker::capture::open(&anyplay_share_worker::capture::WindowTarget {
                title_contains: title.to_owned(),
            })
        }
        _ => {
            let _ = title;
            Err("this build of the share worker cannot capture a window".into())
        }
    }
}

#[cfg(feature = "window")]
fn sink(title: &str) -> Box<dyn FrameSink> {
    Box::new(anyplay_share_worker::window::WindowSink::new(title))
}

#[cfg(not(feature = "window"))]
fn sink(_title: &str) -> Box<dyn FrameSink> {
    Box::new(anyplay_share_worker::viewer::NullSink)
}
