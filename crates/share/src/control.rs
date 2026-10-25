//! The control stream of `spec/share-play.md`: one JSON object per line, in UTF-8, at most
//! 4 KiB. A message the receiver does not know is ignored, and so is a field it does not know.
//!
//! Additions for watch-only screen sharing: the viewer's [`Message::Watch`] (begin sending to
//! me) and [`Message::Unwatch`], and a `title` in the host's [`Hello`]. No message has a field called `id`.

use std::fmt;

use serde::{Deserialize, Serialize};

/// The longest line, terminator included.
pub const MAX_LINE: usize = 4096;
/// The protocol version in `hello`.
pub const VERSION: u32 = 0;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Host,
    Guest,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    Watch,
    HandOver,
    Together,
}

/// What a worker can encode or decode.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodecCaps {
    pub codec: String,
    pub hardware: bool,
    pub max_width: u32,
    pub max_height: u32,
    pub max_fps: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    pub version: u32,
    pub role: Role,
    pub encode: Vec<CodecCaps>,
    pub decode: Vec<CodecCaps>,
    /// What the host shows (the window or game being shared). Empty from a guest.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub title: String,
}

/// A new stream setup, from the host.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    pub epoch: u8,
    pub codec: String,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub kbps: u32,
    pub mode: Mode,
}

/// The guest would like this mode.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Request {
    pub mode: Mode,
}

/// What the rate controller needs, every 100 ms. The packet counts are those since the last
/// report.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Report {
    /// The highest frame received.
    pub frame: u32,
    pub received: u32,
    pub lost: u32,
    pub recovered: u32,
    pub delay_trend_us: i32,
    pub queue_ms: u32,
    pub decoded_fps: u32,
    pub shown_fps: u32,
}

/// A key frame is needed from `frame` on.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Key {
    pub epoch: u8,
    pub frame: u32,
}

/// First of the four timestamps: the sender's clock when it sent.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ping {
    pub t0: u64,
}

/// The answer: `t0` echoed, `t1` the answerer's clock on receipt, `t2` on sending.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pong {
    pub t0: u64,
    pub t1: u64,
    pub t2: u64,
}

/// The sender is ending the share.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stop {
    pub reason: String,
}

/// One line of the control stream.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "msg", rename_all = "snake_case")]
pub enum Message {
    Hello(Hello),
    Config(Config),
    Request(Request),
    Report(Report),
    Key(Key),
    Ping(Ping),
    Pong(Pong),
    Stop(Stop),
    /// Viewer to host: begin sending to me.
    Watch {},
    /// Viewer to host: stop sending to me; the offer stays (`spec/screen-share.md`).
    Unwatch {},
}

const KNOWN: [&str; 10] = [
    "hello", "config", "request", "report", "key", "ping", "pong", "stop", "watch", "unwatch",
];

/// Why a line is not a message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ControlError {
    /// More than 4 KiB.
    TooLong,
    NotUtf8,
    /// A newline inside the line.
    MoreThanOneLine,
    /// Not JSON, not an object with a `msg` string, or a known message with wrong fields.
    Invalid(String),
}

impl fmt::Display for ControlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ControlError::TooLong => write!(f, "control line longer than {MAX_LINE} bytes"),
            ControlError::NotUtf8 => write!(f, "control line is not UTF-8"),
            ControlError::MoreThanOneLine => write!(f, "more than one line"),
            ControlError::Invalid(why) => write!(f, "control line is not a message: {why}"),
        }
    }
}

impl std::error::Error for ControlError {}

/// Read one line, with or without its closing `\n`. `Ok(None)` is a message this version does
/// not know: ignore it. Unknown fields of known messages are ignored.
pub fn parse_line(line: &[u8]) -> Result<Option<Message>, ControlError> {
    if line.len() > MAX_LINE {
        return Err(ControlError::TooLong);
    }
    let line = line.strip_suffix(b"\n").unwrap_or(line);
    if line.contains(&b'\n') {
        return Err(ControlError::MoreThanOneLine);
    }
    let text = std::str::from_utf8(line).map_err(|_| ControlError::NotUtf8)?;
    let value: serde_json::Value =
        serde_json::from_str(text).map_err(|e| ControlError::Invalid(e.to_string()))?;
    let name = value
        .as_object()
        .and_then(|o| o.get("msg"))
        .and_then(|m| m.as_str())
        .ok_or_else(|| ControlError::Invalid("no \"msg\" string".into()))?;
    if !KNOWN.contains(&name) {
        return Ok(None);
    }
    serde_json::from_value(value)
        .map(Some)
        .map_err(|e| ControlError::Invalid(e.to_string()))
}

/// Write one line, closed by `\n`. Refused when it would be over 4 KiB.
pub fn encode_line(message: &Message) -> Result<Vec<u8>, ControlError> {
    let mut out = serde_json::to_vec(message).map_err(|e| ControlError::Invalid(e.to_string()))?;
    out.push(b'\n');
    if out.len() > MAX_LINE {
        return Err(ControlError::TooLong);
    }
    Ok(out)
}

/// The round trip and the other clock's offset from the four timestamps of a `ping`/`pong`
/// exchange: `t0` ping sent, `t1` pong's clock on receipt, `t2` its clock on sending, `t3` the
/// pinger's clock on receipt. The offset is the answerer's clock minus the pinger's, with an
/// error of at most half the round trip when the path is not symmetric.
pub fn clock_estimate(t0: u64, t1: u64, t2: u64, t3: u64) -> (i64, i64) {
    let rtt = (t3 as i64 - t0 as i64) - (t2 as i64 - t1 as i64);
    let offset = ((t1 as i64 - t0 as i64) + (t2 as i64 - t3 as i64)) / 2;
    (rtt, offset)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps() -> CodecCaps {
        CodecCaps {
            codec: "h264".into(),
            hardware: true,
            max_width: 1920,
            max_height: 1080,
            max_fps: 60,
        }
    }

    fn all() -> Vec<Message> {
        vec![
            Message::Hello(Hello {
                version: VERSION,
                role: Role::Host,
                encode: vec![caps()],
                decode: vec![],
                title: "Some Game".into(),
            }),
            Message::Hello(Hello {
                version: VERSION,
                role: Role::Guest,
                encode: vec![],
                decode: vec![caps()],
                title: String::new(),
            }),
            Message::Config(Config {
                epoch: 2,
                codec: "test".into(),
                width: 320,
                height: 180,
                fps: 30,
                kbps: 4000,
                mode: Mode::HandOver,
            }),
            Message::Request(Request {
                mode: Mode::Together,
            }),
            Message::Report(Report {
                frame: 99,
                received: 50,
                lost: 1,
                recovered: 1,
                delay_trend_us: -300,
                queue_ms: 4,
                decoded_fps: 60,
                shown_fps: 59,
            }),
            Message::Key(Key { epoch: 1, frame: 7 }),
            Message::Ping(Ping { t0: 5 }),
            Message::Pong(Pong {
                t0: 5,
                t1: 9,
                t2: 10,
            }),
            Message::Stop(Stop {
                reason: "host_stopped".into(),
            }),
            Message::Watch {},
            Message::Unwatch {},
        ]
    }

    #[test]
    fn every_message_round_trips_as_one_line() {
        for m in all() {
            let line = encode_line(&m).unwrap();
            assert_eq!(*line.last().unwrap(), b'\n');
            assert_eq!(line.iter().filter(|&&b| b == b'\n').count(), 1);
            assert_eq!(parse_line(&line).unwrap(), Some(m.clone()));
            // also without the terminator
            assert_eq!(parse_line(&line[..line.len() - 1]).unwrap(), Some(m));
        }
    }

    #[test]
    fn the_wire_shape() {
        let line = encode_line(&Message::Key(Key { epoch: 1, frame: 7 })).unwrap();
        assert_eq!(line, b"{\"msg\":\"key\",\"epoch\":1,\"frame\":7}\n");
        let line = encode_line(&Message::Watch {}).unwrap();
        assert_eq!(line, b"{\"msg\":\"watch\"}\n");
        let line = encode_line(&Message::Request(Request {
            mode: Mode::HandOver,
        }))
        .unwrap();
        assert_eq!(line, b"{\"msg\":\"request\",\"mode\":\"hand_over\"}\n");
    }

    #[test]
    fn no_message_has_an_id_field() {
        for m in all() {
            let v: serde_json::Value = serde_json::from_slice(&encode_line(&m).unwrap()).unwrap();
            assert!(v.get("id").is_none(), "{v}");
        }
    }

    #[test]
    fn unknown_messages_and_fields_are_ignored() {
        assert_eq!(parse_line(b"{\"msg\":\"teleport\",\"x\":1}\n"), Ok(None));
        let m = parse_line(b"{\"msg\":\"key\",\"epoch\":1,\"frame\":7,\"future\":[1,2]}\n");
        assert_eq!(m, Ok(Some(Message::Key(Key { epoch: 1, frame: 7 }))));
        let w = parse_line(b"{\"msg\":\"watch\",\"extra\":true}\n");
        assert_eq!(w, Ok(Some(Message::Watch {})));
        // a hello from an older host has no title
        let h = parse_line(
            b"{\"msg\":\"hello\",\"version\":0,\"role\":\"host\",\"encode\":[],\"decode\":[]}",
        );
        assert!(matches!(h, Ok(Some(Message::Hello(h))) if h.title.is_empty()));
    }

    #[test]
    fn bad_lines_are_refused() {
        assert_eq!(
            parse_line(&[b'a'; MAX_LINE + 1]),
            Err(ControlError::TooLong)
        );
        assert_eq!(parse_line(&[0xff, 0xfe]), Err(ControlError::NotUtf8));
        assert_eq!(
            parse_line(b"{\"msg\":\"stop\",\n\"reason\":\"x\"}\n"),
            Err(ControlError::MoreThanOneLine)
        );
        assert!(matches!(parse_line(b"[1]"), Err(ControlError::Invalid(_))));
        assert!(matches!(parse_line(b"{}"), Err(ControlError::Invalid(_))));
        assert!(matches!(parse_line(b"nope"), Err(ControlError::Invalid(_))));
        // a known message with a wrong field
        assert!(matches!(
            parse_line(b"{\"msg\":\"key\",\"epoch\":\"x\",\"frame\":1}"),
            Err(ControlError::Invalid(_))
        ));
        // exactly 4096 bytes is fine, one more is not
        let pad = |n: usize| {
            let mut l = b"{\"msg\":\"stop\",\"reason\":\"".to_vec();
            l.extend(std::iter::repeat_n(b'a', n));
            l.extend_from_slice(b"\"}\n");
            l
        };
        let base = pad(0).len();
        assert!(matches!(parse_line(&pad(MAX_LINE - base)), Ok(Some(_))));
        assert_eq!(
            parse_line(&pad(MAX_LINE - base + 1)),
            Err(ControlError::TooLong)
        );
    }

    #[test]
    fn a_line_over_4_kib_is_not_written() {
        let m = Message::Stop(Stop {
            reason: "x".repeat(MAX_LINE),
        });
        assert_eq!(encode_line(&m), Err(ControlError::TooLong));
    }

    #[test]
    fn clock_estimate_finds_rtt_and_offset() {
        // the answerer's clock runs 1000 ahead; 30 each way; 5 to answer
        let (t0, t1, t2, t3) = (100, 100 + 30 + 1000, 100 + 30 + 1000 + 5, 100 + 30 + 5 + 30);
        assert_eq!(clock_estimate(t0, t1, t2, t3), (60, 1000));
    }
}
