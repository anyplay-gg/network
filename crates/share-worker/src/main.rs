//! `anyplay-share`: the share worker program.
//!
//! The net service starts it for a stream and ends it with the stream:
//!
//! ```text
//! anyplay-share --capabilities
//! anyplay-share --role host|guest [--link 127.0.0.1:PORT] [--token TOKEN] [--source test_pattern]
//! ```
//!
//! `--link` and `--token` default to `ANYPLAY_SHARE_ENDPOINT` and `ANYPLAY_SHARE_TOKEN`.

use std::process::ExitCode;

use anyplay_share::codec::VideoDecoder;
use anyplay_share::control::Role;
use anyplay_share::testcodec::{TestDecoder, TestEncoder};
use anyplay_share_worker::host::{self, PatternSource};
use anyplay_share_worker::viewer::{self, DecoderFactory, NullSink};
use anyplay_share_worker::{capabilities, socket};

/// A key frame at least this often, so that a viewer who joins late waits at most 2 seconds.
const KEY_INTERVAL: u32 = 120;

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let mut role = None;
    let mut link = std::env::var("ANYPLAY_SHARE_ENDPOINT").ok();
    let mut token = std::env::var("ANYPLAY_SHARE_TOKEN").ok();
    let mut source = "game".to_owned();
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
            "--title" => {
                args.next();
            }
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
        Role::Host => {
            if source != "test_pattern" {
                // Window capture is not part of this build; say so instead of sending nothing.
                let _ = port.to_service.send(anyplay_share::link::FromWorker::Ended(
                    "this build of the share worker cannot capture a window".into(),
                ));
                return ExitCode::FAILURE;
            }
            host::run(
                Box::new(PatternSource::default()),
                Box::new(TestEncoder::new(KEY_INTERVAL)),
                port,
            );
        }
        Role::Guest => {
            let decoders: DecoderFactory = Box::new(|codec| match codec {
                "test" => Some(Box::new(TestDecoder::new()) as Box<dyn VideoDecoder>),
                _ => None,
            });
            viewer::run(decoders, Box::new(NullSink), port);
        }
    }
    ExitCode::SUCCESS
}
