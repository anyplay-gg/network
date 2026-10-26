//! A small RTMP publisher: the plain handshake, the chunk stream and the commands that open a
//! live stream (`connect`, `createStream`, `publish`), then audio, video and metadata messages.
//!
//! It speaks to a server over TCP (`rtmp://`). It does not read what the server sends beyond the
//! answers it waits for and the pings it must return. TLS (`rtmps://`) is not built.

use std::io::{self, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::amf::{self, Value};

/// The size of the chunks this side sends.
const OUT_CHUNK: usize = 4096;
/// How long the connection and each step of opening the stream may take.
const OPEN_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a write may stall before the link counts as lost.
const WRITE_TIMEOUT: Duration = Duration::from_secs(15);

// Message types.
pub const SET_CHUNK_SIZE: u8 = 1;
pub const ACKNOWLEDGEMENT: u8 = 3;
pub const USER_CONTROL: u8 = 4;
pub const WINDOW_ACK_SIZE: u8 = 5;
pub const AUDIO: u8 = 8;
pub const VIDEO: u8 = 9;
pub const DATA: u8 = 18;
pub const COMMAND: u8 = 20;

// Chunk stream ids.
const CS_CONTROL: u32 = 2;
const CS_COMMAND: u32 = 3;
const CS_AUDIO: u32 = 4;
const CS_DATA: u32 = 5;
const CS_VIDEO: u32 = 6;

/// Where to publish: the parts of `rtmp://host[:port]/app[/path]`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    pub host: String,
    pub port: u16,
    /// What the server calls the application: everything after the host.
    pub app: String,
}

impl Target {
    pub fn parse(url: &str) -> Result<Target, String> {
        let url = url.trim();
        let rest = if let Some(rest) = url.strip_prefix("rtmp://") {
            rest
        } else if url.starts_with("rtmps://") {
            return Err("rtmps:// is not supported yet, use an rtmp:// address".into());
        } else {
            return Err("the address must start with rtmp://".into());
        };
        let (authority, app) = rest.split_once('/').unwrap_or((rest, ""));
        let app = app.trim_matches('/');
        if app.is_empty() {
            return Err("the address has no application name after the server".into());
        }
        let (host, port) = match authority.rsplit_once(':') {
            Some((host, port)) if !port.ends_with(']') => (
                host.trim_matches(['[', ']']),
                port.parse::<u16>()
                    .map_err(|_| "the port in the address is not a number".to_owned())?,
            ),
            _ => (authority.trim_matches(['[', ']']), 1935),
        };
        if host.is_empty() {
            return Err("the address has no server".into());
        }
        Ok(Target {
            host: host.to_owned(),
            port,
            app: app.to_owned(),
        })
    }

    /// `rtmp://host[:port]/app`, as the server wants it in `connect`.
    pub fn tc_url(&self) -> String {
        if self.port == 1935 {
            format!("rtmp://{}/{}", self.host, self.app)
        } else {
            format!("rtmp://{}:{}/{}", self.host, self.port, self.app)
        }
    }
}

// --- writing chunks ---------------------------------------------------------------------------

/// Splits messages into chunks on a stream.
pub struct ChunkWriter<W: Write> {
    out: W,
    chunk_size: usize,
}

impl<W: Write> ChunkWriter<W> {
    pub fn new(out: W) -> Self {
        ChunkWriter {
            out,
            chunk_size: 128,
        }
    }

    pub fn set_chunk_size(&mut self, size: usize) {
        self.chunk_size = size;
    }

    /// One message; its first chunk carries the full header, the rest are continuations.
    pub fn send(
        &mut self,
        csid: u32,
        timestamp: u32,
        kind: u8,
        stream: u32,
        payload: &[u8],
    ) -> io::Result<()> {
        let mut buf = Vec::with_capacity(payload.len() + payload.len() / self.chunk_size * 5 + 32);
        let extended = timestamp >= 0x00FF_FFFF;
        let stamp = if extended { 0x00FF_FFFF } else { timestamp };
        push_basic(&mut buf, 0, csid);
        buf.extend_from_slice(&stamp.to_be_bytes()[1..]);
        buf.extend_from_slice(&(payload.len() as u32).to_be_bytes()[1..]);
        buf.push(kind);
        buf.extend_from_slice(&stream.to_le_bytes());
        if extended {
            buf.extend_from_slice(&timestamp.to_be_bytes());
        }
        let mut chunks = payload.chunks(self.chunk_size);
        if let Some(first) = chunks.next() {
            buf.extend_from_slice(first);
        }
        for chunk in chunks {
            push_basic(&mut buf, 3, csid);
            if extended {
                buf.extend_from_slice(&timestamp.to_be_bytes());
            }
            buf.extend_from_slice(chunk);
        }
        self.out.write_all(&buf)?;
        self.out.flush()
    }

    pub fn into_inner(self) -> W {
        self.out
    }
}

fn push_basic(buf: &mut Vec<u8>, format: u8, csid: u32) {
    match csid {
        2..=63 => buf.push(format << 6 | csid as u8),
        64..=319 => buf.extend_from_slice(&[format << 6, (csid - 64) as u8]),
        _ => {
            let n = csid - 64;
            buf.extend_from_slice(&[format << 6 | 1, (n & 0xFF) as u8, (n >> 8) as u8]);
        }
    }
}

// --- reading chunks ---------------------------------------------------------------------------

/// A whole message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Message {
    pub kind: u8,
    pub stream: u32,
    pub timestamp: u32,
    pub payload: Vec<u8>,
}

#[derive(Default)]
struct Partial {
    timestamp: u32,
    delta: u32,
    length: usize,
    kind: u8,
    stream: u32,
    extended: bool,
    payload: Vec<u8>,
}

/// Joins chunks back into messages. A "set chunk size" message is applied here and still given
/// to the caller.
pub struct ChunkReader<R: Read> {
    input: R,
    chunk_size: usize,
    streams: std::collections::HashMap<u32, Partial>,
    /// Bytes read so far, for acknowledgements.
    pub total: u64,
}

impl<R: Read> ChunkReader<R> {
    pub fn new(input: R) -> Self {
        ChunkReader {
            input,
            chunk_size: 128,
            streams: Default::default(),
            total: 0,
        }
    }

    fn byte(&mut self) -> io::Result<u8> {
        let mut b = [0u8; 1];
        self.input.read_exact(&mut b)?;
        self.total += 1;
        Ok(b[0])
    }

    fn take(&mut self, n: usize) -> io::Result<Vec<u8>> {
        let mut buf = vec![0u8; n];
        self.input.read_exact(&mut buf)?;
        self.total += n as u64;
        Ok(buf)
    }

    fn number(&mut self, n: usize) -> io::Result<u32> {
        let mut value = 0u32;
        for _ in 0..n {
            value = value << 8 | u32::from(self.byte()?);
        }
        Ok(value)
    }

    pub fn read_message(&mut self) -> io::Result<Message> {
        loop {
            let first = self.byte()?;
            let format = first >> 6;
            let csid = match first & 0x3F {
                0 => 64 + u32::from(self.byte()?),
                1 => {
                    let low = u32::from(self.byte()?);
                    64 + low + u32::from(self.byte()?) * 256
                }
                n => u32::from(n),
            };
            let mut partial = self.streams.remove(&csid).unwrap_or_default();
            let continuing = !partial.payload.is_empty();
            match format {
                0 => {
                    let stamp = self.number(3)?;
                    partial.length = self.number(3)? as usize;
                    partial.kind = self.byte()?;
                    let mut id = [0u8; 4];
                    id.copy_from_slice(&self.take(4)?);
                    partial.stream = u32::from_le_bytes(id);
                    partial.extended = stamp == 0x00FF_FFFF;
                    partial.timestamp = if partial.extended {
                        self.number(4)?
                    } else {
                        stamp
                    };
                    partial.delta = 0;
                }
                1 | 2 => {
                    let stamp = self.number(3)?;
                    if format == 1 {
                        partial.length = self.number(3)? as usize;
                        partial.kind = self.byte()?;
                    }
                    partial.extended = stamp == 0x00FF_FFFF;
                    partial.delta = if partial.extended {
                        self.number(4)?
                    } else {
                        stamp
                    };
                    if !continuing {
                        partial.timestamp = partial.timestamp.wrapping_add(partial.delta);
                    }
                }
                _ => {
                    if partial.extended {
                        self.number(4)?;
                    }
                    if !continuing {
                        partial.timestamp = partial.timestamp.wrapping_add(partial.delta);
                    }
                }
            }
            if partial.length > 16 * 1024 * 1024 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "the server sent a message that is too large",
                ));
            }
            let want = (partial.length - partial.payload.len()).min(self.chunk_size);
            let part = self.take(want)?;
            partial.payload.extend_from_slice(&part);
            if partial.payload.len() >= partial.length {
                let message = Message {
                    kind: partial.kind,
                    stream: partial.stream,
                    timestamp: partial.timestamp,
                    payload: std::mem::take(&mut partial.payload),
                };
                self.streams.insert(csid, partial);
                if message.kind == SET_CHUNK_SIZE && message.payload.len() >= 4 {
                    let size = u32::from_be_bytes(message.payload[..4].try_into().unwrap());
                    self.chunk_size = (size & 0x7FFF_FFFF).clamp(1, 16 * 1024 * 1024) as usize;
                }
                return Ok(message);
            }
            self.streams.insert(csid, partial);
        }
    }
}

// --- the handshake ----------------------------------------------------------------------------

/// The client's side of the plain handshake.
fn client_handshake<S: Read + Write>(stream: &mut S) -> io::Result<()> {
    let mut c1 = [0u8; 1536];
    // Time zero, four zero bytes, then bytes that only need to differ between runs.
    let seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(1, |d| d.as_nanos() as u64)
        | 1;
    let mut x = seed;
    for byte in c1[8..].iter_mut() {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        *byte = (x >> 24) as u8;
    }
    stream.write_all(&[3])?;
    stream.write_all(&c1)?;
    stream.flush()?;
    let mut s0 = [0u8; 1];
    stream.read_exact(&mut s0)?;
    if s0[0] != 3 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "the server does not speak RTMP",
        ));
    }
    let mut s1 = [0u8; 1536];
    stream.read_exact(&mut s1)?;
    let mut s2 = [0u8; 1536];
    stream.read_exact(&mut s2)?;
    stream.write_all(&s1)?;
    stream.flush()
}

// --- the publisher ----------------------------------------------------------------------------

type Shared = Arc<Mutex<ChunkWriter<TcpStream>>>;

/// A live stream being published.
pub struct Publisher {
    writer: Shared,
    stream: u32,
    lost: Arc<Mutex<Option<String>>>,
}

fn text_of(error: &io::Error) -> String {
    match error.kind() {
        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock => {
            "the server stopped answering".into()
        }
        io::ErrorKind::UnexpectedEof
        | io::ErrorKind::ConnectionReset
        | io::ErrorKind::ConnectionAborted
        | io::ErrorKind::BrokenPipe => "the server closed the connection".into(),
        _ => error.to_string(),
    }
}

impl Publisher {
    /// Connects, opens the stream named `key` and starts publishing. The key is only ever sent to
    /// the server, never written anywhere else.
    pub fn open(target: &Target, key: &str) -> Result<Publisher, String> {
        let address = (target.host.as_str(), target.port)
            .to_socket_addrs()
            .map_err(|_| format!("the server {} was not found", target.host))?
            .next()
            .ok_or_else(|| format!("the server {} was not found", target.host))?;
        let mut tcp = TcpStream::connect_timeout(&address, OPEN_TIMEOUT)
            .map_err(|e| format!("could not reach {}: {}", target.host, text_of(&e)))?;
        tcp.set_nodelay(true).ok();
        tcp.set_read_timeout(Some(OPEN_TIMEOUT)).ok();
        tcp.set_write_timeout(Some(WRITE_TIMEOUT)).ok();
        client_handshake(&mut tcp).map_err(|e| format!("the handshake failed: {}", text_of(&e)))?;
        let read_half = tcp.try_clone().map_err(|e| e.to_string())?;
        let mut writer = ChunkWriter::new(tcp);
        let mut reader = ChunkReader::new(read_half);
        let io_error = |e: io::Error| text_of(&e);

        writer
            .send(
                CS_CONTROL,
                0,
                SET_CHUNK_SIZE,
                0,
                &(OUT_CHUNK as u32).to_be_bytes(),
            )
            .map_err(io_error)?;
        writer.set_chunk_size(OUT_CHUNK);

        let connect = amf::command(
            "connect",
            1.0,
            &[amf::object(&[
                ("app", Value::Text(target.app.clone())),
                ("type", Value::text("nonprivate")),
                ("flashVer", Value::text("FMLE/3.0 (compatible; AnyPlay)")),
                ("tcUrl", Value::Text(target.tc_url())),
            ])],
        );
        writer
            .send(CS_COMMAND, 0, COMMAND, 0, &connect)
            .map_err(io_error)?;
        await_result(&mut reader, 1.0, "connect")?;

        for (name, txn) in [("releaseStream", 2.0), ("FCPublish", 3.0)] {
            let body = amf::command(name, txn, &[Value::Null, Value::text(key)]);
            writer
                .send(CS_COMMAND, 0, COMMAND, 0, &body)
                .map_err(io_error)?;
        }
        let create = amf::command("createStream", 4.0, &[Value::Null]);
        writer
            .send(CS_COMMAND, 0, COMMAND, 0, &create)
            .map_err(io_error)?;
        let values = await_result(&mut reader, 4.0, "createStream")?;
        let stream = values
            .get(3)
            .and_then(Value::as_number)
            .map_or(1, |n| n as u32);

        let publish = amf::command(
            "publish",
            5.0,
            &[Value::Null, Value::text(key), Value::text("live")],
        );
        writer
            .send(CS_COMMAND, 0, COMMAND, stream, &publish)
            .map_err(io_error)?;
        loop {
            let message = reader.read_message().map_err(io_error)?;
            if message.kind != COMMAND {
                continue;
            }
            let values = amf::read_all(&message.payload);
            if values.first().and_then(Value::as_text) != Some("onStatus") {
                continue;
            }
            let info = values.get(3).cloned().unwrap_or(Value::Null);
            let code = info.get("code").and_then(Value::as_text).unwrap_or("");
            if code == "NetStream.Publish.Start" {
                break;
            }
            let error_level = info.get("level").and_then(Value::as_text) == Some("error");
            if code.starts_with("NetStream.Publish") || error_level {
                return Err(rejected(code));
            }
        }

        let lost = Arc::new(Mutex::new(None));
        let writer: Shared = Arc::new(Mutex::new(writer));
        // From here on the connection is long-lived: reads wait as long as the server is quiet.
        if let Ok(w) = writer.lock() {
            let _ = w.out.set_read_timeout(None);
        }
        let (shared, flag) = (writer.clone(), lost.clone());
        std::thread::Builder::new()
            .name("broadcast-rtmp-in".into())
            .spawn(move || listen(reader, shared, flag))
            .map_err(|e| e.to_string())?;
        Ok(Publisher {
            writer,
            stream,
            lost,
        })
    }

    /// Why the connection ended, once it has.
    pub fn lost(&self) -> Option<String> {
        self.lost.lock().ok().and_then(|l| l.clone())
    }

    fn send(&self, csid: u32, timestamp: u32, kind: u8, payload: &[u8]) -> Result<(), String> {
        if let Some(why) = self.lost() {
            return Err(why);
        }
        let mut writer = self
            .writer
            .lock()
            .map_err(|_| "the connection broke".to_owned())?;
        writer
            .send(csid, timestamp, kind, self.stream, payload)
            .map_err(|e| text_of(&e))
    }

    /// `@setDataFrame` / `onMetaData`.
    pub fn metadata(&self, pairs: &[(&str, Value)]) -> Result<(), String> {
        let mut body = Vec::new();
        amf::write(&mut body, &Value::text("@setDataFrame"));
        amf::write(&mut body, &Value::text("onMetaData"));
        amf::write_array(&mut body, pairs);
        self.send(CS_DATA, 0, DATA, &body)
    }

    pub fn video(&self, timestamp_ms: u32, tag: &[u8]) -> Result<(), String> {
        self.send(CS_VIDEO, timestamp_ms, VIDEO, tag)
    }

    pub fn audio(&self, timestamp_ms: u32, tag: &[u8]) -> Result<(), String> {
        self.send(CS_AUDIO, timestamp_ms, AUDIO, tag)
    }

    /// Ends the stream the polite way.
    pub fn close(&self) {
        let body = amf::command(
            "deleteStream",
            6.0,
            &[Value::Null, Value::Number(f64::from(self.stream))],
        );
        let _ = self.send(CS_COMMAND, 0, COMMAND, &body);
        if let Ok(writer) = self.writer.lock() {
            let _ = writer.out.shutdown(std::net::Shutdown::Both);
        }
    }
}

fn rejected(code: &str) -> String {
    match code {
        "NetStream.Publish.BadName" => "the server does not accept this stream key".into(),
        "NetStream.Publish.Denied"
        | "NetStream.Publish.Rejected"
        | "NetStream.Publish.Unauthorized" => {
            "the server does not allow this stream key to publish".into()
        }
        "" => "the server refused to start the stream".into(),
        other => format!("the server refused to start the stream ({other})"),
    }
}

/// Waits for the `_result` of a transaction; an `_error` is the server saying no.
fn await_result<R: Read>(
    reader: &mut ChunkReader<R>,
    transaction: f64,
    step: &str,
) -> Result<Vec<Value>, String> {
    loop {
        let message = reader.read_message().map_err(|e| text_of(&e))?;
        if message.kind != COMMAND {
            continue;
        }
        let values = amf::read_all(&message.payload);
        let name = values.first().and_then(Value::as_text);
        let txn = values.get(1).and_then(Value::as_number);
        match name {
            Some("_result") if txn == Some(transaction) => return Ok(values),
            Some("_error") if txn == Some(transaction) => {
                return Err(format!("the server refused {step}"));
            }
            _ => {}
        }
    }
}

/// Answers pings and acknowledgements and notes when the server ends the stream.
fn listen(mut reader: ChunkReader<TcpStream>, writer: Shared, lost: Arc<Mutex<Option<String>>>) {
    let mut window: u64 = 0;
    let mut acknowledged: u64 = 0;
    let end = loop {
        let message = match reader.read_message() {
            Ok(message) => message,
            Err(error) => break text_of(&error),
        };
        match message.kind {
            WINDOW_ACK_SIZE if message.payload.len() >= 4 => {
                window = u64::from(u32::from_be_bytes(message.payload[..4].try_into().unwrap()));
            }
            USER_CONTROL if message.payload.len() >= 6 && message.payload[..2] == [0, 6] => {
                // PingRequest: answer with PingResponse and the same time.
                let mut reply = vec![0, 7];
                reply.extend_from_slice(&message.payload[2..6]);
                if let Ok(mut w) = writer.lock() {
                    let _ = w.send(CS_CONTROL, 0, USER_CONTROL, 0, &reply);
                }
            }
            COMMAND => {
                let values = amf::read_all(&message.payload);
                if values.first().and_then(Value::as_text) == Some("onStatus") {
                    let info = values.get(3).cloned().unwrap_or(Value::Null);
                    let code = info.get("code").and_then(Value::as_text).unwrap_or("");
                    let ends = (code.starts_with("NetStream.Publish.")
                        && code != "NetStream.Publish.Start")
                        || code == "NetStream.Play.Stop"
                        || code == "NetConnection.Connect.Closed";
                    if ends {
                        break rejected(code);
                    }
                }
            }
            _ => {}
        }
        if window > 0 && reader.total - acknowledged >= window {
            acknowledged = reader.total;
            if let Ok(mut w) = writer.lock() {
                let _ = w.send(
                    CS_CONTROL,
                    0,
                    ACKNOWLEDGEMENT,
                    0,
                    &(acknowledged as u32).to_be_bytes(),
                );
            }
        }
    };
    if let Ok(mut slot) = lost.lock() {
        slot.get_or_insert(end);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_are_split_into_server_port_and_application() {
        let t = Target::parse("rtmp://live.twitch.tv/app").unwrap();
        assert_eq!(
            (t.host.as_str(), t.port, t.app.as_str()),
            ("live.twitch.tv", 1935, "app")
        );
        assert_eq!(t.tc_url(), "rtmp://live.twitch.tv/app");
        let t = Target::parse(" rtmp://127.0.0.1:1936/live/ ").unwrap();
        assert_eq!(
            (t.host.as_str(), t.port, t.app.as_str()),
            ("127.0.0.1", 1936, "live")
        );
        assert_eq!(t.tc_url(), "rtmp://127.0.0.1:1936/live");
        let t = Target::parse("rtmp://a.rtmp.youtube.com/live2").unwrap();
        assert_eq!(t.app, "live2");
    }

    #[test]
    fn bad_addresses_say_what_is_wrong() {
        assert!(Target::parse("http://x/y").unwrap_err().contains("rtmp://"));
        assert!(
            Target::parse("rtmps://x/y")
                .unwrap_err()
                .contains("not supported")
        );
        assert!(
            Target::parse("rtmp://host")
                .unwrap_err()
                .contains("application")
        );
        assert!(
            Target::parse("rtmp://host:abc/app")
                .unwrap_err()
                .contains("port")
        );
        assert!(Target::parse("rtmp:///app").unwrap_err().contains("server"));
    }

    #[test]
    fn a_message_larger_than_a_chunk_survives_the_chunking() {
        let payload: Vec<u8> = (0..10_000u32).map(|n| n as u8).collect();
        for stamp in [0u32, 5_000, 0x00FF_FFFF + 7] {
            let mut writer = ChunkWriter::new(Vec::new());
            writer.set_chunk_size(4096);
            writer.send(6, stamp, VIDEO, 1, &payload).unwrap();
            writer.send(4, stamp, AUDIO, 1, &[1, 2, 3]).unwrap();
            let bytes = writer.into_inner();
            let mut reader = ChunkReader::new(&bytes[..]);
            reader.chunk_size = 4096;
            let first = reader.read_message().unwrap();
            assert_eq!(
                (first.kind, first.stream, first.timestamp),
                (VIDEO, 1, stamp)
            );
            assert_eq!(first.payload, payload);
            let second = reader.read_message().unwrap();
            assert_eq!(second.payload, [1, 2, 3]);
        }
    }

    #[test]
    fn the_reader_follows_a_set_chunk_size() {
        let mut writer = ChunkWriter::new(Vec::new());
        writer
            .send(2, 0, SET_CHUNK_SIZE, 0, &1024u32.to_be_bytes())
            .unwrap();
        writer.set_chunk_size(1024);
        writer.send(3, 0, COMMAND, 0, &vec![7u8; 3000]).unwrap();
        let bytes = writer.into_inner();
        let mut reader = ChunkReader::new(&bytes[..]);
        assert_eq!(reader.read_message().unwrap().kind, SET_CHUNK_SIZE);
        assert_eq!(reader.read_message().unwrap().payload.len(), 3000);
    }
}
