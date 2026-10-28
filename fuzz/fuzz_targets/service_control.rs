//! The net service's control loop, receiving arbitrary chunks of text from the launcher over an
//! in-memory duplex stream.
//!
//! Verifies the invariants from docs/gaps.md and spec/control-protocol.md:
//! 1. Crash and panic freedom: arbitrary byte sequences (valid or invalid JSON, corrupted framing,
//!    unexpected commands, missing arguments) never crash or panic the service.
//! 2. Bounded line buffering: lines exceeding `control::MAX_LINE` (1 MiB) are discarded without
//!    unbounded memory growth.
//! 3. Clean termination on EOF: when the launcher's stream ends, the service loop terminates cleanly.
//! 4. Protocol output validity: every output emitted by the service is within `MAX_LINE`, ends with
//!    `\n`, is valid UTF-8 and valid JSON, and parses as a control `Output` (either `Answer` or `Event`).
//! 5. Request-reply correspondence: for unique command IDs, at most one answer is returned.

#![no_main]

use std::collections::HashSet;
use std::sync::LazyLock;
use std::time::Duration;

use anyplay_control::{MAX_LINE, Output, parse_output};
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
            let mut answer_ids = HashSet::new();

            while let Ok(Some(line)) = lines.next_line().await {
                assert!(line.len() <= MAX_LINE, "output line exceeds MAX_LINE");
                let output = parse_output(&line)
                    .unwrap_or_else(|| panic!("output line failed to parse: {line:?}"));

                if let Output::Answer(ref answer) = output {
                    assert!(
                        answer_ids.insert(answer.id),
                        "duplicate answer received for command id {}",
                        answer.id
                    );
                }
            }
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

        for chunk in wire.chunks(chunk_size) {
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

        // Await reader completion.
        match tokio::time::timeout(Duration::from_secs(2), reader).await {
            Ok(res) => {
                res.expect("reader task panicked");
            }
            Err(_) => {
                panic!("reader task failed to finish within timeout");
            }
        }
    });
});
