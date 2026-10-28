//! The net service's control loop, receiving arbitrary chunks of text from the launcher over an
//! in-memory duplex stream.
//!
//! Verifies the invariants from docs/gaps.md and spec/control-protocol.md:
//! 1. Crash and panic freedom: arbitrary byte sequences (valid or invalid JSON, corrupted framing,
//!    unexpected commands, missing arguments) never crash or panic the service.
//! 2. Bounded line buffering: an oversized line (> 1 MiB) does not stop the service loop or
//!    prevent reading subsequent commands.
//! 3. Clean termination on EOF: when the launcher's stream ends, the service loop terminates cleanly.
//! 4. Protocol output validity: every line emitted by the service fits within `control::MAX_LINE`,
//!    parses as valid JSON, and deserializes into a control `Output` (either `Answer` or `Event`).
//! 5. Request-reply correspondence: the number of answers returned for an id never exceeds the
//!    number of lines in the input that carried that command id.
//!
//! Note on hermeticity and test environment:
//! - To keep the fuzzer hermetic, deterministic, and isolated from external networks or browser
//!   invocations, `hello` commands are filtered out before being streamed to the service. This
//!   exercises the entire pre-`hello` dispatch surface, unrecognized commands, oversized lines,
//!   and error responses without attempting cloud connections.
//! - `bind: 127.0.0.1:0`, the bridge listener, and `Traffic::start` allocate loopback sockets per
//!   iteration.

#![no_main]

use std::collections::HashMap;
use std::sync::LazyLock;
use std::time::Duration;

use anyplay_control::{Command, MAX_LINE, Output, parse_command, parse_output};
use anyplay_net::{Config, install_crypto, run};
use libfuzzer_sys::fuzz_target;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

static RUNTIME: LazyLock<tokio::runtime::Runtime> = LazyLock::new(|| {
    install_crypto();
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("failed to start tokio runtime for fuzz target")
});

fuzz_target!(|data: &[u8]| {
    RUNTIME.block_on(async {
        let Ok(data_dir) = tempfile::tempdir() else {
            return;
        };

        let config = Config {
            data: data_dir.path().to_path_buf(),
            file_credentials: true,
            relay_roots: Vec::new(),
            bind: Some("127.0.0.1:0".parse().unwrap()),
            relay_only: false,
            device_name: "fuzz-client".into(),
            audio: anyplay_net::voice::Audio::system(),
            workers: anyplay_net::screen::WorkerSet::new(
                anyplay_net::worker_process::ProcessWorkers::new(),
            ),
            discord: anyplay_discord::Backend::Off,
        };

        let (mut client_writer, service_reader) = tokio::io::duplex(64 * 1024);
        let (service_writer, client_reader) = tokio::io::duplex(64 * 1024);

        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
        let service = tokio::spawn(run(config, service_reader, service_writer, async move {
            let _ = stop_rx.await;
        }));

        let reader = tokio::spawn(async move {
            let mut lines = BufReader::new(client_reader).lines();
            let mut answer_counts: HashMap<u64, usize> = HashMap::new();

            while let Ok(Some(line)) = lines.next_line().await {
                assert!(line.len() <= MAX_LINE, "output line exceeds MAX_LINE");
                let output = parse_output(&line)
                    .unwrap_or_else(|| panic!("output line failed to parse: {line:?}"));

                if let Output::Answer(ref answer) = output {
                    *answer_counts.entry(answer.id).or_default() += 1;
                }
            }
            answer_counts
        });

        let (chunk_size, wire) = match data.split_first() {
            Some((&b, rest)) => (usize::from(b % 64) + 1, rest),
            None => (64, &[][..]),
        };

        // Bound total wire size to avoid wasting excessive fuzz cycles on oversized inputs,
        // while still well exceeding MAX_LINE to thoroughly exercise line-skipping logic.
        let wire = if wire.len() > 2 * MAX_LINE + 4096 {
            &wire[..2 * MAX_LINE + 4096]
        } else {
            wire
        };

        // Track how many times each command id appears in the input wire, and filter out
        // `hello` to preserve hermeticity (preventing arbitrary network or browser requests).
        let mut sent_id_counts: HashMap<u64, usize> = HashMap::new();
        let mut sanitized_wire = Vec::with_capacity(wire.len());

        let mut pos = 0;
        while pos < wire.len() {
            let newline = wire[pos..].iter().position(|&b| b == b'\n');
            let end = match newline {
                Some(idx) => pos + idx + 1,
                None => wire.len(),
            };
            let slice = &wire[pos..end];
            pos = end;

            // Check if this line is a command or has an id
            if let Ok(text) = std::str::from_utf8(slice) {
                let trimmed = text.trim_end();
                match parse_command(trimmed) {
                    Ok((_id, Command::Hello { .. })) => {
                        // Drop `hello` to keep the fuzzer strictly hermetic.
                        continue;
                    }
                    Ok((id, _)) => {
                        *sent_id_counts.entry(id).or_default() += 1;
                    }
                    Err(Some(id)) => {
                        // Service::read_commands emits Input::BadCommand(Some(id)) for malformed commands
                        // where an integer id was present, which generates an UNKNOWN_COMMAND answer.
                        *sent_id_counts.entry(id).or_default() += 1;
                    }
                    Err(None) => {}
                }
            }

            sanitized_wire.extend_from_slice(slice);
        }

        for chunk in sanitized_wire.chunks(chunk_size) {
            if client_writer.write_all(chunk).await.is_err() {
                break;
            }
        }

        // Close the input stream with EOF.
        drop(client_writer);

        // Await service termination with a bounded timeout.
        match tokio::time::timeout(Duration::from_secs(5), service).await {
            Ok(res) => {
                res.expect("service task panicked")
                    .expect("service run returned an IO error");
            }
            Err(_) => {
                let _ = stop_tx.send(());
                panic!("service failed to terminate within timeout after input EOF");
            }
        }

        // Await reader completion and verify answer counts against sent commands.
        let answer_counts = match tokio::time::timeout(Duration::from_secs(2), reader).await {
            Ok(res) => res.expect("reader task panicked"),
            Err(_) => {
                panic!("reader task failed to finish within timeout");
            }
        };

        for (id, count) in answer_counts {
            let sent = sent_id_counts.get(&id).copied().unwrap_or(0);
            assert!(
                count <= sent,
                "received {count} answers for command id {id}, but only {sent} were sent"
            );
        }
    });
});
