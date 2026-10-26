//! The broadcaster against a small RTMP server of its own, on loopback.

use std::cell::RefCell;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::rc::Rc;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyplay_share::codec::{CodecError, EncodedFrame, Frame, VideoEncoder};

use super::amf::{self, Value};
use super::audio::{AudioEncoder, SoundFactory};
use super::rtmp::{AUDIO, COMMAND, ChunkReader, ChunkWriter, USER_CONTROL, VIDEO, WINDOW_ACK_SIZE};
use super::{Command, Event, Setup, run, run_with_sound};
use crate::host::PatternSource;
use crate::sound::{SoundSource, ToneSource};

/// What the server saw.
#[derive(Default)]
struct Seen {
    commands: Vec<String>,
    video: Vec<(u32, Vec<u8>)>,
    audio: Vec<(u32, Vec<u8>)>,
    keys: Vec<String>,
    data: usize,
}

/// A server that accepts one publisher. With `reject` it answers `publish` with a refusal.
fn server(reject: bool) -> (u16, Arc<Mutex<Seen>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen = Arc::new(Mutex::new(Seen::default()));
    let shared = seen.clone();
    std::thread::spawn(move || {
        let Ok((stream, _)) = listener.accept() else {
            return;
        };
        serve(stream.try_clone().unwrap(), stream, reject, &shared);
    });
    (port, seen)
}

/// One TLS stream that a reader and a writer on the same thread share.
#[derive(Clone)]
struct Both(Rc<RefCell<rustls::StreamOwned<rustls::ServerConnection, TcpStream>>>);

impl Read for Both {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.0.borrow_mut().read(buf)
    }
}

impl Write for Both {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.borrow_mut().write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.0.borrow_mut().flush()
    }
}

/// A self-signed certificate for `localhost`, the server's settings for it and the client's
/// settings that trust only it.
fn tls_pair() -> (Arc<rustls::ServerConfig>, Arc<rustls::ClientConfig>) {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let der = cert.cert.der().clone();
    let key = rustls::pki_types::PrivateKeyDer::from(rustls::pki_types::PrivatePkcs8KeyDer::from(
        cert.signing_key.serialize_der(),
    ));
    let server = rustls::ServerConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![der.clone()], key)
        .unwrap();
    let mut roots = rustls::RootCertStore::empty();
    roots.add(der).unwrap();
    let client = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
    (Arc::new(server), Arc::new(client))
}

/// Like [`server`], over TLS.
fn tls_server(config: Arc<rustls::ServerConfig>) -> (u16, Arc<Mutex<Seen>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen = Arc::new(Mutex::new(Seen::default()));
    let shared = seen.clone();
    std::thread::spawn(move || {
        let Ok((tcp, _)) = listener.accept() else {
            return;
        };
        let conn = rustls::ServerConnection::new(config).unwrap();
        let both = Both(Rc::new(RefCell::new(rustls::StreamOwned::new(conn, tcp))));
        serve(both.clone(), both, false, &shared);
    });
    (port, seen)
}

fn serve(mut input: impl Read, mut output: impl Write, reject: bool, seen: &Arc<Mutex<Seen>>) {
    let mut c0c1 = [0u8; 1537];
    if input.read_exact(&mut c0c1).is_err() {
        return;
    }
    let mut reply = vec![3u8];
    reply.extend_from_slice(&[0u8; 1536]);
    reply.extend_from_slice(&[0u8; 1536]);
    output.write_all(&reply).unwrap();
    output.flush().unwrap();
    let mut c2 = [0u8; 1536];
    if input.read_exact(&mut c2).is_err() {
        return;
    }
    let mut reader = ChunkReader::new(input);
    let mut writer = ChunkWriter::new(output);
    while let Ok(message) = reader.read_message() {
        match message.kind {
            COMMAND => {
                let values = amf::read_all(&message.payload);
                let name = values[0].as_text().unwrap_or("").to_owned();
                seen.lock().unwrap().commands.push(name.clone());
                if name == "publish"
                    && let Some(key) = values.get(3).and_then(Value::as_text)
                {
                    seen.lock().unwrap().keys.push(key.to_owned());
                }
                match name.as_str() {
                    "connect" => {
                        writer
                            .send(2, 0, WINDOW_ACK_SIZE, 0, &2_500_000u32.to_be_bytes())
                            .unwrap();
                        // A ping before the answer: the client must not trip over it.
                        writer
                            .send(2, 0, USER_CONTROL, 0, &[0, 6, 0, 0, 0, 1])
                            .unwrap();
                        let body = amf::command(
                            "_result",
                            1.0,
                            &[Value::Null, amf::object(&[("code", Value::text("ok"))])],
                        );
                        writer.send(3, 0, COMMAND, 0, &body).unwrap();
                    }
                    "createStream" => {
                        let body = amf::command("_result", 4.0, &[Value::Null, Value::Number(1.0)]);
                        writer.send(3, 0, COMMAND, 0, &body).unwrap();
                    }
                    "publish" => {
                        let code = if reject {
                            "NetStream.Publish.BadName"
                        } else {
                            "NetStream.Publish.Start"
                        };
                        let body = amf::command(
                            "onStatus",
                            0.0,
                            &[
                                Value::Null,
                                amf::object(&[
                                    (
                                        "level",
                                        Value::text(if reject { "error" } else { "status" }),
                                    ),
                                    ("code", Value::text(code)),
                                ]),
                            ],
                        );
                        writer.send(5, 0, COMMAND, 1, &body).unwrap();
                        if reject {
                            return;
                        }
                    }
                    _ => {}
                }
            }
            VIDEO => seen
                .lock()
                .unwrap()
                .video
                .push((message.timestamp, message.payload)),
            AUDIO => seen
                .lock()
                .unwrap()
                .audio
                .push((message.timestamp, message.payload)),
            super::rtmp::DATA => seen.lock().unwrap().data += 1,
            _ => {}
        }
    }
}

/// Fakes H.264: a key frame carries SPS and PPS, every picture one slice.
struct FakeEncoder {
    count: u8,
}

impl VideoEncoder for FakeEncoder {
    fn codec(&self) -> &str {
        "h264"
    }
    fn encode(&mut self, frame: &Frame, force_key: bool) -> Result<EncodedFrame, CodecError> {
        self.count = self.count.wrapping_add(1);
        let mut data = Vec::new();
        if force_key {
            data.extend_from_slice(&[0, 0, 0, 1, 0x67, 0x42, 0xC0, 0x1F, 0xAA]);
            data.extend_from_slice(&[0, 0, 0, 1, 0x68, 0xCE, 0x3C]);
            data.extend_from_slice(&[0, 0, 1, 0x65, self.count]);
        } else {
            data.extend_from_slice(&[0, 0, 1, 0x41, self.count]);
        }
        Ok(EncodedFrame {
            key: force_key,
            timestamp_us: frame.timestamp_us,
            data,
        })
    }
}

fn setup() -> Setup {
    Setup {
        width: 320,
        height: 180,
        fps: 30,
        kbps: 1000,
    }
}

type Started = (
    mpsc::Sender<Command>,
    mpsc::Receiver<Event>,
    std::thread::JoinHandle<()>,
);

fn start(url: String, key: &str) -> Started {
    start_with(url, key, None, None)
}

fn start_with(
    url: String,
    key: &str,
    sound: Option<SoundFactory>,
    tls: Option<Arc<rustls::ClientConfig>>,
) -> Started {
    let (command_tx, command_rx) = mpsc::channel();
    let (event_tx, event_rx) = mpsc::channel();
    let key = key.to_owned();
    let thread = std::thread::spawn(move || {
        run_with_sound(
            setup(),
            &url,
            &key,
            Box::new(PatternSource::default()),
            Box::new(FakeEncoder { count: 0 }),
            sound,
            tls,
            command_rx,
            event_tx,
        )
    });
    (command_tx, event_rx, thread)
}

fn wait_for(events: &mpsc::Receiver<Event>, pick: impl Fn(&Event) -> bool) -> Event {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        let event = events.recv_timeout(left).expect("an event in time");
        if pick(&event) {
            return event;
        }
    }
}

#[test]
fn a_broadcast_opens_the_stream_and_sends_configuration_video_and_silence() {
    let (port, seen) = server(false);
    let (commands, events, thread) = start(format!("rtmp://127.0.0.1:{port}/live"), "secret-key");
    assert_eq!(wait_for(&events, |_| true), Event::Connecting);
    assert!(matches!(
        wait_for(&events, |_| true),
        Event::Live { width: 320, .. }
    ));
    std::thread::sleep(Duration::from_millis(2600));
    commands.send(Command::Quit).unwrap();
    assert_eq!(
        wait_for(&events, |e| matches!(e, Event::Ended { .. })),
        Event::Ended {
            reason: "quit".into()
        }
    );
    thread.join().unwrap();
    std::thread::sleep(Duration::from_millis(200));

    let seen = seen.lock().unwrap();
    assert_eq!(
        &seen.commands[..4],
        ["connect", "releaseStream", "FCPublish", "createStream"]
    );
    assert!(seen.commands.contains(&"publish".to_owned()));
    assert_eq!(seen.keys, ["secret-key"]);
    assert_eq!(seen.data, 1, "the metadata");
    // The configuration comes first, then pictures that start with a key frame.
    assert_eq!(&seen.video[0].1[..5], [0x17, 0, 0, 0, 0]);
    assert_eq!(seen.video[1].1[0], 0x17);
    assert_eq!(seen.video[1].1[1], 1);
    assert!(seen.video.len() > 40, "{} pictures", seen.video.len());
    // Another key frame after about 2 seconds.
    let keys = seen
        .video
        .iter()
        .filter(|v| v.1[0] == 0x17 && v.1[1] == 1)
        .count();
    assert!(keys >= 2, "{keys} key frames");
    // Time never goes backwards.
    assert!(seen.video.windows(2).all(|w| w[0].0 <= w[1].0));
    // Silence: a configuration, then frames about 21 ms apart.
    assert_eq!(seen.audio[0].1, [0xAF, 0, 0x11, 0x90]);
    assert!(seen.audio.len() > 80, "{} audio frames", seen.audio.len());
    assert_eq!(
        seen.audio[1].1,
        [0xAF, 1, 0x21, 0x10, 0x04, 0x60, 0x8C, 0x1C]
    );
}

#[test]
fn a_refused_key_ends_the_broadcast_in_plain_words() {
    let (port, _) = server(true);
    let (_commands, events, thread) = start(format!("rtmp://127.0.0.1:{port}/live"), "wrong");
    let Event::Ended { reason } = wait_for(&events, |e| matches!(e, Event::Ended { .. })) else {
        unreachable!()
    };
    assert!(reason.starts_with("connect_failed: "), "{reason}");
    assert!(
        reason.contains("does not accept this stream key"),
        "{reason}"
    );
    thread.join().unwrap();
}

#[test]
fn a_server_that_is_not_there_is_reported_and_the_key_stays_out_of_the_words() {
    // Nothing listens on a port that was just free.
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let (_commands, events, thread) = start(format!("rtmp://127.0.0.1:{port}/live"), "hunter2");
    let Event::Ended { reason } = wait_for(&events, |e| matches!(e, Event::Ended { .. })) else {
        unreachable!()
    };
    assert!(reason.starts_with("connect_failed: "), "{reason}");
    assert!(!reason.contains("hunter2"));
    thread.join().unwrap();
}

#[test]
fn an_address_without_rtmp_is_refused_before_any_connection() {
    let (_commands, events, thread) = start("https://example.com/live".into(), "k");
    let Event::Ended { reason } = wait_for(&events, |e| matches!(e, Event::Ended { .. })) else {
        unreachable!()
    };
    assert!(reason.contains("rtmp://"), "{reason}");
    thread.join().unwrap();
}

#[test]
fn the_key_never_shows_in_a_debug_print() {
    let command: Command =
        serde_json::from_str(r#"{"do":"publish","url":"rtmp://h/app","key":"live_123_abc"}"#)
            .unwrap();
    let printed = format!("{command:?}");
    assert!(printed.contains("rtmp://h/app"));
    assert!(!printed.contains("live_123_abc"));
    assert_eq!(
        serde_json::from_str::<Command>(r#"{"do":"quit"}"#).unwrap(),
        Command::Quit
    );
}

#[test]
fn the_events_are_json_lines() {
    assert_eq!(Event::Connecting.to_line(), r#"{"event":"connecting"}"#);
    assert_eq!(
        Event::Stats {
            seconds: 4,
            kbps: 5900,
            dropped: 0
        }
        .to_line(),
        r#"{"event":"stats","seconds":4,"kbps":5900,"dropped":0}"#
    );
}

/// Fakes AAC: one two-byte frame per 1024 samples, `0xAB` and the block's peak level in percent.
struct FakeAac {
    pending: Vec<f32>,
}

impl AudioEncoder for FakeAac {
    fn encode(&mut self, samples: &[f32]) -> Result<Vec<Vec<u8>>, String> {
        self.pending.extend_from_slice(samples);
        let mut frames = Vec::new();
        while self.pending.len() >= 2048 {
            let block: Vec<f32> = self.pending.drain(..2048).collect();
            let peak = block.iter().fold(0.0f32, |m, s| m.max(s.abs()));
            frames.push(vec![0xAB, (peak * 100.0) as u8]);
        }
        Ok(frames)
    }
}

fn tone_factory() -> SoundFactory {
    Box::new(|| {
        Ok((
            Box::new(ToneSource::default()) as Box<dyn SoundSource>,
            Box::new(FakeAac {
                pending: Vec::new(),
            }) as Box<dyn AudioEncoder>,
        ))
    })
}

/// Waits for `Live`, lets the stream run for `seconds`, ends it and returns the reason.
fn run_for(
    commands: &mpsc::Sender<Command>,
    events: &mpsc::Receiver<Event>,
    seconds: f64,
) -> String {
    assert_eq!(wait_for(events, |_| true), Event::Connecting);
    assert!(matches!(wait_for(events, |_| true), Event::Live { .. }));
    std::thread::sleep(Duration::from_secs_f64(seconds));
    commands.send(Command::Quit).unwrap();
    let Event::Ended { reason } = wait_for(events, |e| matches!(e, Event::Ended { .. })) else {
        unreachable!()
    };
    reason
}

fn ended(events: &mpsc::Receiver<Event>) -> String {
    let Event::Ended { reason } = wait_for(events, |e| matches!(e, Event::Ended { .. })) else {
        unreachable!()
    };
    reason
}

#[test]
fn the_games_sound_goes_out_as_aac_in_step_with_the_clock() {
    let (port, seen) = server(false);
    let (commands, events, thread) = start_with(
        format!("rtmp://127.0.0.1:{port}/live"),
        "k",
        Some(tone_factory()),
        None,
    );
    assert_eq!(run_for(&commands, &events, 2.6), "quit");
    thread.join().unwrap();
    std::thread::sleep(Duration::from_millis(200));
    let seen = seen.lock().unwrap();
    assert_eq!(
        seen.audio[0].1,
        [0xAF, 0, 0x11, 0x90],
        "the configuration first"
    );
    let frames = &seen.audio[1..];
    assert!(frames.len() > 80, "{} audio frames", frames.len());
    // Real sound, not the silence of version 0: every frame is the fake encoder's, with the
    // level of the tone (a quarter of full scale) once it settled.
    assert!(
        frames.iter().all(|f| f.1[..3] == [0xAF, 1, 0xAB]),
        "silence in between"
    );
    let loud = frames.iter().filter(|f| f.1[3] >= 20).count();
    assert!(
        loud * 10 >= frames.len() * 9,
        "{loud} of {} loud",
        frames.len()
    );
    // Times never go back, start near zero and follow the clock to the end.
    assert!(frames.windows(2).all(|w| w[0].0 <= w[1].0));
    assert!(frames[0].0 < 200, "{}", frames[0].0);
    let last = frames.last().unwrap().0;
    assert!((1800..=2900).contains(&last), "last frame at {last} ms");
    // Pictures and sound share one clock: the last of each is within a quarter of a second.
    let video_last = seen.video.last().unwrap().0;
    assert!(video_last.abs_diff(last) < 250, "{video_last} vs {last}");
}

#[test]
fn a_sound_that_cannot_start_leaves_the_stream_with_silence() {
    let (port, seen) = server(false);
    let failing: SoundFactory = Box::new(|| Err("no game sound here".to_owned()));
    let (commands, events, thread) = start_with(
        format!("rtmp://127.0.0.1:{port}/live"),
        "k",
        Some(failing),
        None,
    );
    assert_eq!(run_for(&commands, &events, 1.2), "quit");
    thread.join().unwrap();
    std::thread::sleep(Duration::from_millis(200));
    let seen = seen.lock().unwrap();
    assert_eq!(seen.audio[0].1, [0xAF, 0, 0x11, 0x90]);
    assert!(seen.audio.len() > 30, "{} audio frames", seen.audio.len());
    assert_eq!(
        seen.audio[1].1,
        [0xAF, 1, 0x21, 0x10, 0x04, 0x60, 0x8C, 0x1C]
    );
}

#[test]
fn a_secure_address_streams_over_tls() {
    let (server_config, client_config) = tls_pair();
    let (port, seen) = tls_server(server_config);
    let (commands, events, thread) = start_with(
        format!("rtmps://localhost:{port}/live"),
        "tls-key",
        Some(tone_factory()),
        Some(client_config),
    );
    assert_eq!(run_for(&commands, &events, 1.5), "quit");
    thread.join().unwrap();
    std::thread::sleep(Duration::from_millis(300));
    let seen = seen.lock().unwrap();
    assert_eq!(
        &seen.commands[..4],
        ["connect", "releaseStream", "FCPublish", "createStream"]
    );
    assert_eq!(seen.keys, ["tls-key"]);
    assert!(seen.video.len() > 20, "{} pictures", seen.video.len());
    assert!(seen.audio.len() > 40, "{} audio frames", seen.audio.len());
}

#[test]
fn a_certificate_the_pc_does_not_trust_is_said_in_plain_words() {
    // The server's certificate is not the one the client trusts.
    let (server_config, _) = tls_pair();
    let (_, other_client) = tls_pair();
    let (port, _) = tls_server(server_config);
    let (_commands, events, thread) = start_with(
        format!("rtmps://localhost:{port}/live"),
        "secret-key-1",
        None,
        Some(other_client),
    );
    let reason = ended(&events);
    assert!(
        reason.starts_with("connect_failed: tls_untrusted: "),
        "{reason}"
    );
    assert!(reason.contains("certificate"), "{reason}");
    assert!(!reason.contains("secret-key-1"));
    thread.join().unwrap();
}

#[test]
fn a_certificate_for_another_name_is_said_in_plain_words() {
    // The certificate is for `localhost`; the address names the loopback IP.
    let (server_config, client_config) = tls_pair();
    let (port, _) = tls_server(server_config);
    let (_commands, events, thread) = start_with(
        format!("rtmps://127.0.0.1:{port}/live"),
        "k",
        None,
        Some(client_config),
    );
    let reason = ended(&events);
    assert!(
        reason.starts_with("connect_failed: tls_untrusted: "),
        "{reason}"
    );
    assert!(reason.contains("different server name"), "{reason}");
    thread.join().unwrap();
}

#[test]
fn a_server_that_hangs_up_during_the_handshake_is_a_secure_connection_failure() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        let _ = listener.accept();
    });
    let (_, client_config) = tls_pair();
    let (_commands, events, thread) = start_with(
        format!("rtmps://localhost:{port}/live"),
        "k",
        None,
        Some(client_config),
    );
    let reason = ended(&events);
    assert!(
        reason.starts_with("connect_failed: tls_failed: "),
        "{reason}"
    );
    thread.join().unwrap();
}

/// Replays real H.264 (Annex B, one access unit after the other, each starting with a delimiter)
/// from a file, looping.
struct Replay {
    units: Vec<Vec<u8>>,
    at: usize,
}

impl Replay {
    fn new(data: &[u8]) -> Replay {
        let mut units: Vec<Vec<u8>> = Vec::new();
        let mut starts = Vec::new();
        let mut i = 0;
        while i + 4 < data.len() {
            // An access unit delimiter: 00 00 00 01 09.
            if data[i..i + 5] == [0, 0, 0, 1, 0x09] {
                starts.push(i);
                i += 5;
            } else {
                i += 1;
            }
        }
        for (n, start) in starts.iter().enumerate() {
            let end = starts.get(n + 1).copied().unwrap_or(data.len());
            units.push(data[*start..end].to_vec());
        }
        Replay { units, at: 0 }
    }
}

impl VideoEncoder for Replay {
    fn codec(&self) -> &str {
        "h264"
    }
    fn encode(&mut self, frame: &Frame, _force_key: bool) -> Result<EncodedFrame, CodecError> {
        let data = self.units[self.at % self.units.len()].clone();
        self.at += 1;
        let key = crate::annexb::nal_types(&data).contains(&5);
        Ok(EncodedFrame {
            key,
            timestamp_us: frame.timestamp_us,
            data,
        })
    }
}

/// ffmpeg's own RTMP server takes the stream, and what it saves is a playable H.264 and AAC
/// file. `ANYPLAY_FFMPEG` and `ANYPLAY_FFPROBE` name the programs.
#[test]
#[ignore = "needs ffmpeg and ffprobe (ANYPLAY_FFMPEG, ANYPLAY_FFPROBE)"]
fn ffmpeg_accepts_the_stream_and_the_file_it_saves_plays() {
    use std::process::{Command as Process, Stdio};
    let ffmpeg = std::env::var("ANYPLAY_FFMPEG").expect("ANYPLAY_FFMPEG");
    let ffprobe = std::env::var("ANYPLAY_FFPROBE").expect("ANYPLAY_FFPROBE");
    let folder = std::env::temp_dir().join(format!("anyplay-broadcast-{}", std::process::id()));
    std::fs::create_dir_all(&folder).unwrap();
    let source = folder.join("source.h264");
    let made = Process::new(&ffmpeg)
        .args([
            "-v",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "testsrc=size=320x180:rate=30",
        ])
        .args([
            "-t",
            "6",
            "-c:v",
            "libx264",
            "-pix_fmt",
            "yuv420p",
            "-profile:v",
            "baseline",
            "-bf",
            "0",
        ])
        .args(["-g", "60", "-x264-params", "aud=1", "-f", "h264"])
        .arg(&source)
        .status()
        .unwrap();
    assert!(made.success());
    let replay = Replay::new(&std::fs::read(&source).unwrap());
    assert!(replay.units.len() >= 150);

    let port = {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let saved = folder.join("saved.flv");
    let mut server = Process::new(&ffmpeg)
        .args(["-v", "info", "-y", "-listen", "1", "-i"])
        .arg(format!("rtmp://127.0.0.1:{port}/live/test-key"))
        .args(["-c", "copy", "-f", "flv"])
        .arg(&saved)
        .stdin(Stdio::null())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(1500));

    let (command_tx, command_rx) = mpsc::channel();
    let (event_tx, event_rx) = mpsc::channel();
    let url = format!("rtmp://127.0.0.1:{port}/live");
    let thread = std::thread::spawn(move || {
        run(
            setup(),
            &url,
            "test-key",
            Box::new(PatternSource::default()),
            Box::new(replay),
            command_rx,
            event_tx,
        )
    });
    assert_eq!(wait_for(&event_rx, |_| true), Event::Connecting);
    let live = wait_for(&event_rx, |_| true);
    assert!(matches!(live, Event::Live { .. }), "{live:?}");
    std::thread::sleep(Duration::from_millis(5000));
    command_tx.send(Command::Quit).unwrap();
    thread.join().unwrap();
    let _ = server.wait();

    let probe = Process::new(&ffprobe)
        .args([
            "-v",
            "error",
            "-show_entries",
            "stream=codec_name,codec_type,width,height",
        ])
        .args(["-of", "csv=p=0"])
        .arg(&saved)
        .output()
        .unwrap();
    let report = String::from_utf8_lossy(&probe.stdout).into_owned();
    eprintln!("ffprobe: {report}");
    assert!(
        report.contains("h264,video,320,180") || report.contains("h264,320,180,video"),
        "{report}"
    );
    assert!(report.contains("aac"), "{report}");
    let decoded = Process::new(&ffmpeg)
        .args(["-v", "error", "-i"])
        .arg(&saved)
        .args(["-f", "null", "-"])
        .output()
        .unwrap();
    let complaints = String::from_utf8_lossy(&decoded.stderr).into_owned();
    assert!(decoded.status.success(), "{complaints}");
    assert!(complaints.trim().is_empty(), "{complaints}");
    let _ = std::fs::remove_dir_all(&folder);
}
