//! The game's sound from a streamer's worker to a viewer's, on channels: a tone in, the same
//! tone out, next to the pictures.

use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyplay_share::link::{FromWorker, Start, ToWorker};
use anyplay_share::testcodec::TestEncoder;
use anyplay_share_worker::host::{self, PatternSource};
use anyplay_share_worker::sound::{ClockOut, ToneSource};
use anyplay_share_worker::viewer::{self, NullSink};
use anyplay_share_worker::{Port, decoder_for};

fn start() -> Start {
    Start {
        epoch: 0,
        codec: "test".into(),
        width: 320,
        height: 180,
        fps: 30,
        kbps: 1000,
        parity_percent: 0,
        max_payload: 1200,
    }
}

#[test]
fn the_streamer_s_sound_is_played_by_the_viewer() {
    let (to_host, host_from_service) = mpsc::channel();
    let (host_to_service, from_host) = mpsc::channel();
    let (to_viewer, viewer_from_service) = mpsc::channel();
    let (viewer_to_service, _from_viewer) = mpsc::channel();
    let out = ClockOut::default();
    let played = out.played.clone();
    let streamer = std::thread::spawn(move || {
        let port = Port {
            from_service: host_from_service,
            to_service: host_to_service,
        };
        let mut encoder = Some(TestEncoder::new(30));
        host::run_with_sound(
            Box::new(PatternSource::default()),
            Box::new(move |_| {
                encoder
                    .take()
                    .map(|e| Box::new(e) as _)
                    .ok_or_else(|| "one encoder".to_owned())
            }),
            Some(Box::new(ToneSource::default())),
            port,
        );
    });
    let watcher = std::thread::spawn(move || {
        let port = Port {
            from_service: viewer_from_service,
            to_service: viewer_to_service,
        };
        viewer::run_with_sound(
            Box::new(decoder_for),
            Box::new(NullSink),
            Some(Box::new(out)),
            port,
        );
    });
    to_host.send(ToWorker::Start(start())).unwrap();
    to_viewer.send(ToWorker::Start(start())).unwrap();
    // The net service's part: every datagram of the streamer to the viewer, until 80 sound
    // datagrams (0.8 s of sound) went through. A slow machine only takes longer.
    let mut sound_datagrams = 0;
    let began = Instant::now();
    let deadline = began + Duration::from_secs(10);
    while sound_datagrams < 80 && Instant::now() < deadline {
        if let Ok(FromWorker::Datagram(bytes)) = from_host.recv_timeout(Duration::from_millis(10)) {
            if bytes[1] == anyplay_share::wire::TYPE_AUDIO {
                sound_datagrams += 1;
            }
            to_viewer.send(ToWorker::Datagram(bytes)).unwrap();
        }
    }
    let took = began.elapsed();
    // The viewer plays on its own clock: give it the time to play what it has buffered.
    let heard =
        |played: &[f32]| played.iter().filter(|s| s.abs() > 0.05).count() as f32 / 2.0 / 48_000.0;
    let deadline = Instant::now() + Duration::from_secs(5);
    while heard(&played.lock().unwrap()) <= 0.4 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    to_host.send(ToWorker::Stop).unwrap();
    to_viewer.send(ToWorker::Stop).unwrap();
    streamer.join().unwrap();
    watcher.join().unwrap();
    assert_eq!(sound_datagrams, 80, "sound datagrams in {took:?}");
    // One per 10 ms: 80 of them never come faster than the sound they carry.
    assert!(
        took >= Duration::from_millis(600),
        "80 sound datagrams in {took:?}"
    );
    let played = played.lock().unwrap();
    // The tone's peak is 0.25; what the viewer played reaches it once the buffer filled.
    let peak = played.iter().fold(0.0f32, |m, s| m.max(s.abs()));
    assert!(peak > 0.15 && peak < 0.35, "peak {peak}");
    let heard = heard(&played);
    assert!(heard > 0.4, "only {heard:.2} s of the tone were heard");
}
