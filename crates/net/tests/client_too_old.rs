//! A cloud that retires this client: every command that reaches it fails with `client_too_old`
//! and the `minimum`, and the launcher is told once, with one event, however often it is asked.

use std::time::Duration;

use anyplay_api::path;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::get;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// A cloud that refuses everything with 426, and keeps the `X-AnyPlay-Client` it saw.
async fn retiring_cloud() -> (String, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let kept = seen.clone();
    let app = axum::Router::new().route(
        path::INFO,
        get(move |headers: HeaderMap| async move {
            let client = headers
                .get("x-anyplay-client")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_owned();
            kept.lock().unwrap().push(client);
            (
                StatusCode::UPGRADE_REQUIRED,
                axum::Json(json!({
                    "error": "client_too_old",
                    "message": "Update the launcher.",
                    "minimum": "0.3.0"
                })),
            )
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await });
    (base, seen)
}

#[tokio::test]
async fn the_launcher_is_told_once_and_every_failure_carries_the_minimum() {
    anyplay_net::install_crypto();
    let (base, seen) = retiring_cloud().await;
    let data = tempfile::tempdir().unwrap();
    let config = anyplay_net::Config {
        data: data.path().to_path_buf(),
        file_credentials: true,
        relay_roots: Vec::new(),
        bind: Some("127.0.0.1:0".parse().unwrap()),
        relay_only: false,
        device_name: "test".into(),
        audio: anyplay_net::voice::Audio::system(),
        workers: anyplay_net::screen::WorkerSet::new(
            anyplay_net::worker_process::ProcessWorkers::new(),
        ),
        discord: anyplay_discord::Backend::Off,
    };
    let (mut commands, input) = tokio::io::duplex(64 * 1024);
    let (output, answers) = tokio::io::duplex(64 * 1024);
    tokio::spawn(anyplay_net::run(
        config,
        input,
        output,
        std::future::pending(),
    ));
    let mut lines = BufReader::new(answers).lines();

    async fn send(commands: &mut tokio::io::DuplexStream, line: Value) {
        commands
            .write_all(format!("{line}\n").as_bytes())
            .await
            .unwrap();
    }
    // Reads lines until the answer to `id`; returns it and the events that came before.
    async fn answer_to<R: tokio::io::AsyncBufRead + Unpin>(
        lines: &mut tokio::io::Lines<R>,
        id: u64,
    ) -> (Value, Vec<Value>) {
        let mut events = Vec::new();
        loop {
            let line = tokio::time::timeout(Duration::from_secs(20), lines.next_line())
                .await
                .expect("no answer in time")
                .unwrap()
                .expect("the service closed");
            let value: Value = serde_json::from_str(&line).unwrap();
            if value["id"] == id {
                return (value, events);
            }
            events.push(value);
        }
    }
    let is_too_old = |v: &Value| v["event"] == "client_too_old";

    send(
        &mut commands,
        json!({"id": 1, "do": "hello", "launcher": "0.2.0", "cloud": base}),
    )
    .await;
    let (hello, _) = answer_to(&mut lines, 1).await;
    assert!(hello["ok"].is_object(), "{hello}");

    send(
        &mut commands,
        json!({"id": 2, "do": "browser_sign_in", "create": false}),
    )
    .await;
    let (first, events) = answer_to(&mut lines, 2).await;
    assert_eq!(first["error"]["code"], "client_too_old", "{first}");
    assert_eq!(first["error"]["minimum"], "0.3.0", "{first}");
    // The event comes before or after the answer; count what follows too.
    let mut told = events.iter().filter(|e| is_too_old(e)).count();

    send(
        &mut commands,
        json!({"id": 3, "do": "browser_sign_in", "create": false}),
    )
    .await;
    let (second, events) = answer_to(&mut lines, 3).await;
    assert_eq!(second["error"]["code"], "client_too_old", "{second}");
    told += events.iter().filter(|e| is_too_old(e)).count();
    // Anything still in flight arrives within a moment.
    while let Ok(Ok(Some(line))) =
        tokio::time::timeout(Duration::from_millis(500), lines.next_line()).await
    {
        let value: Value = serde_json::from_str(&line).unwrap();
        if is_too_old(&value) {
            told += 1;
            assert_eq!(value["minimum"], "0.3.0");
        }
    }
    assert_eq!(told, 1, "the launcher is told once per run");

    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 2);
    assert!(
        seen.iter()
            .all(|c| *c == concat!("net/", env!("CARGO_PKG_VERSION")) || c.starts_with("launcher/")),
        "{seen:?}"
    );
}
