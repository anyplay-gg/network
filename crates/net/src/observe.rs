//! The net service's observation log: one JSON line per web request it could not answer, in
//! `observations/<title ID>.jsonl` (see `spec/observation-log.md`).

use std::path::PathBuf;
use std::time::Instant;

use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;

const QUEUE: usize = 1024;

/// How large one title's log file may grow; further lines are dropped.
pub const MAX_FILE: u64 = 16 * 1024 * 1024;

/// Hands lines to a writer task; drops lines rather than wait when it falls behind.
#[derive(Clone)]
pub struct Observer {
    lines: mpsc::Sender<(String, serde_json::Value)>,
    started: Instant,
    dropped: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

impl Observer {
    pub fn start(folder: PathBuf) -> Self {
        Self::start_capped(folder, MAX_FILE)
    }

    pub fn start_capped(folder: PathBuf, max_file: u64) -> Self {
        let (lines, mut queue) = mpsc::channel::<(String, serde_json::Value)>(QUEUE);
        tokio::spawn(async move {
            while let Some((title, line)) = queue.recv().await {
                let name: String = title
                    .chars()
                    .filter(|c| c.is_ascii_alphanumeric())
                    .take(16)
                    .collect();
                let name = if name.is_empty() {
                    "unknown".into()
                } else {
                    name
                };
                if tokio::fs::create_dir_all(&folder).await.is_err() {
                    continue;
                }
                let path = folder.join(format!("{name}.jsonl"));
                let Ok(mut file) = tokio::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&path)
                    .await
                else {
                    continue;
                };
                let mut text = line.to_string();
                text.push('\n');
                let size = file.metadata().await.map_or(u64::MAX, |m| m.len());
                if size.saturating_add(text.len() as u64) > max_file {
                    continue;
                }
                // tokio hands the write to a thread; without the flush the next line may measure
                // the file before this one landed and write past the cap.
                if file.write_all(text.as_bytes()).await.is_ok() {
                    let _ = file.flush().await;
                }
            }
        });
        Self {
            lines,
            started: Instant::now(),
            dropped: Default::default(),
        }
    }

    /// Records what the net service did with a call of the game's.
    pub fn record(
        &self,
        title: &str,
        what: &str,
        input: serde_json::Value,
        out: serde_json::Value,
    ) {
        use std::sync::atomic::Ordering;
        let t_ns = self.started.elapsed().as_nanos() as u64;
        let dropped = self.dropped.swap(0, Ordering::Relaxed);
        if dropped > 0 {
            let line = serde_json::json!({"v": 1, "t_ns": t_ns, "src": "service", "fn": "dropped", "dropped": dropped});
            if self.lines.try_send((title.to_owned(), line)).is_err() {
                self.dropped.fetch_add(dropped, Ordering::Relaxed);
            }
        }
        let line = serde_json::json!({"v": 1, "t_ns": t_ns, "src": "service", "fn": what, "in": input, "out": out});
        if self.lines.try_send((title.to_owned(), line)).is_err() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Header names whose values carry credentials; the log keeps the name and not the value.
const SECRET_HEADERS: [&str; 6] = [
    "authorization",
    "proxy-authorization",
    "cookie",
    "set-cookie",
    "x-api-key",
    "x-auth-token",
];

/// The headers of a game's request as the log writes them, with credential values replaced.
pub fn redact_headers(headers: &[(String, String)]) -> serde_json::Map<String, serde_json::Value> {
    headers
        .iter()
        .map(|(name, value)| {
            let lower = name.to_ascii_lowercase();
            let secret = SECRET_HEADERS.contains(&lower.as_str())
                || lower.contains("token")
                || lower.contains("secret")
                || lower.contains("auth");
            let value = if secret { "[redacted]" } else { value.as_str() };
            (name.clone(), serde_json::Value::String(value.to_owned()))
        })
        .collect()
}

/// Where the observation log cuts a body from the game.
pub const CUT: usize = 64 * 1024;

/// Bytes from the game as the observation log writes them: as text when they are UTF-8, else as
/// base64, cut at [`CUT`].
pub fn game_bytes(bytes: &[u8]) -> serde_json::Value {
    let mut shown = &bytes[..bytes.len().min(CUT)];
    if let Err(error) = std::str::from_utf8(shown)
        && bytes.len() > CUT
        && error.error_len().is_none()
    {
        shown = &shown[..error.valid_up_to()];
    }
    match std::str::from_utf8(shown) {
        Ok(text) => serde_json::Value::String(text.to_owned()),
        Err(_) => serde_json::json!({"b64": data_encoding::BASE64.encode(shown)}),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credentials_in_headers_are_not_logged() {
        let headers = vec![
            ("Authorization".to_owned(), "Bearer secret-1".to_owned()),
            ("Cookie".to_owned(), "sid=secret-2".to_owned()),
            ("X-Session-Token".to_owned(), "secret-3".to_owned()),
            ("Content-Type".to_owned(), "application/json".to_owned()),
        ];
        let text = serde_json::Value::Object(redact_headers(&headers)).to_string();
        assert!(!text.contains("secret"), "{text}");
        assert!(text.contains("application/json"));
        assert!(text.contains("Authorization"), "the name stays");
    }

    #[tokio::test]
    async fn a_title_log_stops_growing_at_its_cap() {
        let dir = tempfile::tempdir().unwrap();
        let observer = Observer::start_capped(dir.path().to_path_buf(), 2048);
        for n in 0..200 {
            observer.record(
                "T1",
                "web",
                serde_json::json!({"n": n}),
                serde_json::Value::Null,
            );
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let size = std::fs::metadata(dir.path().join("T1.jsonl"))
            .unwrap()
            .len();
        assert!(size > 0 && size <= 2048, "{size}");
    }

    #[test]
    fn text_stays_text_and_the_rest_is_base64() {
        assert_eq!(game_bytes(b"{\"a\":1}"), serde_json::json!("{\"a\":1}"));
        assert_eq!(
            game_bytes(&[0xFF, 0x00]),
            serde_json::json!({"b64": "/wA="})
        );
        let mut long = "a".repeat(CUT - 1).into_bytes();
        long.extend_from_slice("ä".as_bytes());
        assert_eq!(game_bytes(&long), serde_json::json!("a".repeat(CUT - 1)));
    }
}
