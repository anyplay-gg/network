//! The net service's observation log: one JSON line per web request it could not answer, in
//! `observations/<title ID>.jsonl` (see `spec/observation-log.md`).

use std::path::PathBuf;
use std::time::Instant;

use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;

const QUEUE: usize = 1024;

/// Hands lines to a writer task; drops lines rather than wait when it falls behind.
#[derive(Clone)]
pub struct Observer {
    lines: mpsc::Sender<(String, serde_json::Value)>,
    started: Instant,
    dropped: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

impl Observer {
    pub fn start(folder: PathBuf) -> Self {
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
                let _ = file.write_all(text.as_bytes()).await;
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
