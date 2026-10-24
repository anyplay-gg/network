//! Hear yourself through the whole voice pipeline: the microphone is encoded, the frames are
//! "sent" (optionally with loss and delay) and decoded again, and the result plays on the speakers.
//!
//! Use headphones: there is no echo cancellation, and the speakers feed the microphone.
//!
//! ```text
//! cargo run -p anyplay-voice --features devices --example loopback --release
//! cargo run -p anyplay-voice --features devices --example loopback --release -- --loss 20 --jitter 40 --open
//! ```
//!
//! Options: `--open` opens the microphone by voice activity instead of holding Enter,
//! `--loss <percent>`, `--jitter <ms>`, `--no-ns` turns noise suppression off.

use std::io::BufRead;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyplay_voice::{Config, CpalDevice, Engine, PeerId, TalkMode};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut open = false;
    let mut loss = 0u32;
    let mut jitter = 0u64;
    let mut noise_suppression = true;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--open" => open = true,
            "--no-ns" => noise_suppression = false,
            "--loss" => loss = args.next().ok_or("--loss needs a percentage")?.parse()?,
            "--jitter" => jitter = args.next().ok_or("--jitter needs milliseconds")?.parse()?,
            other => return Err(format!("unknown option {other}").into()),
        }
    }

    let config = Config {
        mode: if open {
            TalkMode::VoiceActivity
        } else {
            TalkMode::PushToTalk
        },
        noise_suppression,
        expected_loss_percent: loss.min(100) as u8,
        ..Config::default()
    };
    let mut engine = Engine::new(config, Box::new(CpalDevice::open()?))?;

    // Push-to-talk on a terminal: Enter toggles the key.
    let talking = Arc::new(AtomicBool::new(false));
    if !open {
        let talking = talking.clone();
        std::thread::spawn(move || {
            for _ in std::io::stdin().lock().lines() {
                let now = !talking.fetch_xor(true, Ordering::Relaxed);
                println!("{}", if now { "talking" } else { "released" });
            }
        });
        println!("Press Enter to start talking, Enter again to stop. Ctrl+C ends.");
    } else {
        println!("Open microphone. Ctrl+C ends.");
    }

    let me = PeerId(1);
    let mut rng = 0x1234_5678u32;
    let mut random = move || {
        rng ^= rng << 13;
        rng ^= rng >> 17;
        rng ^= rng << 5;
        rng
    };
    let mut delayed: Vec<(Instant, Vec<u8>)> = Vec::new();
    let mut last_report = Instant::now();
    loop {
        engine.set_talking(talking.load(Ordering::Relaxed));
        engine.pump();
        for datagram in engine.outgoing() {
            if random() % 100 < loss {
                continue;
            }
            let wait = if jitter > 0 {
                u64::from(random()) % jitter
            } else {
                0
            };
            delayed.push((Instant::now() + Duration::from_millis(wait), datagram));
        }
        let now = Instant::now();
        let (due, later): (Vec<_>, Vec<_>) = delayed.drain(..).partition(|(t, _)| *t <= now);
        delayed = later;
        for (_, datagram) in due {
            engine.incoming(me, &datagram)?;
        }
        if last_report.elapsed() > Duration::from_secs(1) {
            last_report = now;
            let levels = engine.levels();
            let stats = engine.peer_stats(me);
            println!(
                "me {:>5.3}{}  |  heard {:>5.3}{}  {}",
                levels.me.rms,
                if levels.me.speaking { " *" } else { "  " },
                levels.peers.first().map_or(0.0, |p| p.1.rms),
                if levels.peers.first().is_some_and(|p| p.1.speaking) {
                    " *"
                } else {
                    "  "
                },
                stats.map_or(String::new(), |s| format!(
                    "recovered {} concealed {} late {} depth {}",
                    s.recovered, s.concealed, s.late, s.depth
                )),
            );
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}
