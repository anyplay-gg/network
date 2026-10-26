//! The broadcaster against a small RTMP server of its own, on loopback.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyplay_share::codec::{CodecError, EncodedFrame, Frame, VideoEncoder};

use super::amf::{self, Value};
use super::rtmp::{AUDIO, COMMAND, ChunkReader, ChunkWriter, USER_CONTROL, VIDEO, WINDOW_ACK_SIZE};
use super::{Command, Event, Setup, run};
use crate::host::PatternSource;

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
        let Ok((mut stream, _)) = listener.accept() else {
            return;
        };
        serve(&mut stream, reject, &shared);
    });
    (port, seen)
}

fn serve(stream: &mut TcpStream, reject: bool, seen: &Arc<Mutex<Seen>>) {
    let mut c0c1 = [0u8; 1537];
    if stream.read_exact(&mut c0c1).is_err() {
        return;
    }
    let mut reply = vec![3u8];
    reply.extend_from_slice(&[0u8; 1536]);
    reply.extend_from_slice(&[0u8; 1536]);
    stream.write_all(&reply).unwrap();
    let mut c2 = [0u8; 1536];
    if stream.read_exact(&mut c2).is_err() {
        return;
    }
    let mut reader = ChunkReader::new(stream.try_clone().unwrap());
    let mut writer = ChunkWriter::new(stream.try_clone().unwrap());
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
    let (command_tx, command_rx) = mpsc::channel();
    let (event_tx, event_rx) = mpsc::channel();
    let key = key.to_owned();
    let thread = std::thread::spawn(move || {
        run(
            setup(),
            &url,
            &key,
            Box::new(PatternSource::default()),
            Box::new(FakeEncoder { count: 0 }),
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
