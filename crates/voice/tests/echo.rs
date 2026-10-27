//! Echo cancellation, measured. No audio devices: a far-end voice is played into a simulated room
//! (a delay and a few reflections), and what the microphone hears of it goes back through the
//! canceller. The figure is ERLE, echo return loss enhancement: how many dB quieter the echo got.
//! Every signal is generated from fixed seeds, so the numbers are the same on every run.

use std::f32::consts::TAU;

use anyplay_voice::wire::Frame;
use anyplay_voice::{Config, EchoCanceller, Engine, MemoryDevice, PeerId, TalkMode};
use opus::{Channels, Decoder};

const RATE: usize = 48_000;
const BLOCK: usize = 480;

struct Lcg(u32);

impl Lcg {
    fn signed(&mut self) -> f32 {
        self.0 = self.0.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        ((self.0 >> 8) % 20001) as f32 / 10000.0 - 1.0
    }
}

/// Something that moves like a voice: a pitch with harmonics shaped by two formants, a syllable
/// rhythm of about 4 per second, and a little breath. `pitch` and `seed` tell two voices apart.
fn voiced(secs: f32, amp: f32, pitch: f32, seed: u32) -> Vec<f32> {
    let mut rng = Lcg(seed);
    (0..(RATE as f32 * secs) as usize)
        .map(|i| {
            let t = i as f32 / RATE as f32;
            let f0 = pitch + 8.0 * (TAU * 0.7 * t).sin();
            let envelope = (0.5 + 0.5 * (TAU * 4.0 * t + seed as f32).sin()).powf(0.7);
            let mut v = 0.0;
            for h in 1..30 {
                let f = f0 * h as f32;
                if f > RATE as f32 / 2.2 {
                    break;
                }
                let formant = 1.0 / (1.0 + ((f - 700.0) / 250.0).powi(2))
                    + 0.6 / (1.0 + ((f - 1800.0) / 350.0).powi(2));
                v += formant / h as f32 * (TAU * f0 * h as f32 * t).sin();
            }
            amp * (envelope * v * 0.9 + 0.01 * rng.signed())
        })
        .collect()
}

/// A room: what the speakers played, delayed and reflected, as the microphone hears it at
/// sample `n`. The taps are (delay in samples, gain).
struct Room {
    taps: Vec<(usize, f32)>,
}

impl Room {
    fn living_room() -> Room {
        Room {
            taps: [(40, 0.45), (48, 0.20), (63, 0.12), (91, 0.06)]
                .iter()
                .map(|(ms, g)| (ms * RATE / 1000, *g))
                .collect(),
        }
    }

    fn hear(&self, played: &[f32], n: usize) -> f32 {
        self.taps
            .iter()
            .filter(|(d, _)| n >= *d && n - d < played.len())
            .map(|(d, g)| g * played[n - d])
            .sum()
    }
}

fn energy(x: &[f32]) -> f64 {
    x.iter().map(|v| f64::from(*v).powi(2)).sum::<f64>() / x.len().max(1) as f64
}

fn db(ratio: f64) -> f64 {
    10.0 * ratio.max(1e-30).log10()
}

/// The canceller on its own: `far` plays, the microphone hears `room(far) + near`. Returns the
/// echo at the microphone and what came out of the canceller.
fn run_canceller(far: &[f32], near: &[f32], room: &Room) -> (Vec<f32>, Vec<f32>) {
    let mut canceller = EchoCanceller::new();
    let mut echo = Vec::new();
    let mut out = Vec::new();
    for (b, block) in far.as_chunks::<BLOCK>().0.iter().enumerate() {
        // The reference goes in first, as the engine releases it: the speakers play it before
        // the microphone can hear it.
        canceller.render(block);
        let start = b * BLOCK;
        let heard: Vec<f32> = (0..BLOCK).map(|i| room.hear(far, start + i)).collect();
        let mut mic: Vec<f32> = heard
            .iter()
            .enumerate()
            .map(|(i, e)| e + near.get(start + i).copied().unwrap_or(0.0))
            .collect();
        echo.extend(heard);
        canceller.capture(&mut mic);
        out.extend(mic);
    }
    (echo, out)
}

#[test]
fn the_canceller_takes_the_echo_out_of_a_quiet_microphone() {
    let far = voiced(12.0, 0.5, 120.0, 5);
    let (echo, out) = run_canceller(&far, &[], &Room::living_room());
    // The last four seconds, after it has found the room.
    let tail = out.len() - 4 * RATE..;
    let erle = db(energy(&echo[tail.clone()]) / energy(&out[tail]));
    println!("canceller ERLE {erle:.1} dB");
    assert!(erle > 20.0, "ERLE {erle:.1} dB");
}

#[test]
fn the_canceller_is_transparent_when_nothing_plays() {
    // A headset: the speakers are silent, the microphone hears only the player.
    let near = voiced(6.0, 0.4, 150.0, 9);
    let silent_far = vec![0.0f32; near.len()];
    let (_, out) = run_canceller(&silent_far, &near, &Room::living_room());
    let tail = out.len() - 3 * RATE..;
    let kept = db(energy(&out[tail.clone()]) / energy(&near[tail]));
    println!("voice kept {kept:.1} dB");
    assert!(kept > -1.5, "the voice lost {kept:.1} dB");
}

#[test]
fn the_player_is_still_heard_while_the_far_end_talks() {
    // Double talk: both voices at once. The echo goes down; the player's voice stays.
    let far = voiced(12.0, 0.5, 120.0, 5);
    // The player is not periodic like the far voice: shaped noise with a syllable rhythm, so the
    // two cannot be confused by their harmonics.
    let mut rng = Lcg(77);
    let mut low = 0.0f32;
    let near: Vec<f32> = (0..12 * RATE)
        .map(|i| {
            low += 0.35 * (rng.signed() - low);
            let t = i as f32 / RATE as f32;
            0.5 * low * (0.5 + 0.5 * (TAU * 3.0 * t).sin())
        })
        .collect();
    let (echo, out) = run_canceller(&far, &near, &Room::living_room());
    let tail = out.len() - 4 * RATE..;
    // The two voices are unrelated, so the share of the output that follows the echo (its
    // projection on the echo) is the echo that is left. The player's voice is followed by its
    // correlation with the output; the canceller delays the signal by a few milliseconds, so the
    // best lag within 100 ms is taken.
    let (o, n, e) = (&out[tail.clone()], &near[tail.clone()], &echo[tail]);
    let dot = |a: &[f32], b: &[f32]| a.iter().zip(b).map(|(a, b)| f64::from(a * b)).sum::<f64>();
    let erle = db(dot(e, e) / (dot(o, e).powi(2) / dot(e, e)));
    let mut corr = 0.0f64;
    for lag in (0..4800).step_by(8) {
        let (o, n) = (&o[lag..], &n[..n.len() - lag]);
        corr = corr.max(dot(o, n) / (dot(o, o) * dot(n, n)).sqrt());
    }
    println!("double talk: ERLE {erle:.1} dB, correlation with the player's voice {corr:.2}");
    assert!(erle > 20.0, "ERLE {erle:.1} dB");
    assert!(
        corr > 0.2,
        "the player's voice is gone: correlation {corr:.2}"
    );
}

// ---------------------------------------------------------------- the whole engine

/// A far-end engine talks to a near-end one; the near-end engine's speakers feed its own
/// microphone through the room. Returns the energy the near-end engine would send (decoded) and
/// the energy of the echo at its microphone, both over the last `tail` seconds.
fn engine_run(echo_cancellation: bool, secs: usize, tail: usize) -> (f64, f64) {
    let (far_dev, far_hw) = MemoryDevice::new(48_000, 48_000);
    let (near_dev, near_hw) = MemoryDevice::new(48_000, 48_000);
    let config = Config {
        mode: TalkMode::PushToTalk,
        noise_suppression: false,
        echo_cancellation,
        dtx: false,
        ..Config::default()
    };
    let mut far = Engine::new(config.clone(), Box::new(far_dev)).unwrap();
    let mut near = Engine::new(config, Box::new(near_dev)).unwrap();
    far.set_talking(true);
    near.set_talking(true);

    let voice = voiced(secs as f32, 0.5, 120.0, 5);
    let room = Room::living_room();
    let mut decoder = Decoder::new(48_000, Channels::Mono).unwrap();
    let mut played: Vec<f32> = Vec::new();
    let mut echo: Vec<f32> = Vec::new();
    let mut sent: Vec<f32> = Vec::new();
    let step = RATE / 50;
    for (s, chunk) in voice.chunks_exact(step).enumerate() {
        far_hw.feed_capture(chunk);
        far.pump();
        for datagram in far.outgoing() {
            near.incoming(PeerId(1), &datagram).unwrap();
        }
        let mic: Vec<f32> = (0..step)
            .map(|i| room.hear(&played, s * step + i))
            .collect();
        echo.extend(&mic);
        near_hw.feed_capture(&mic);
        near.pump();
        for datagram in near.outgoing() {
            let frame = Frame::parse(&datagram).unwrap();
            let mut pcm = vec![0.0f32; 960];
            let n = decoder
                .decode_float(frame.payload, &mut pcm, false)
                .unwrap();
            sent.extend(&pcm[..n]);
        }
        played.extend(near_hw.play(step));
        let _ = far_hw.play(step);
    }
    assert!(
        sent.len() > (secs - 1) * RATE,
        "sent {} samples",
        sent.len()
    );
    let sent_tail = &sent[sent.len() - tail * RATE..];
    let echo_tail = &echo[echo.len() - tail * RATE..];
    (energy(sent_tail), energy(echo_tail))
}

#[test]
fn the_engine_does_not_send_the_echo_of_its_own_speakers() {
    let (off_sent, echo) = engine_run(false, 14, 4);
    let (on_sent, _) = engine_run(true, 14, 4);
    let off = db(echo / off_sent);
    let on = db(echo / on_sent);
    println!(
        "engine ERLE: off {off:.1} dB, on {on:.1} dB (echo {:.1} dBFS)",
        db(echo)
    );
    assert!(on - off > 15.0, "off {off:.1} dB, on {on:.1} dB");
    assert!(on > 15.0, "ERLE with the canceller on is {on:.1} dB");
}

#[test]
fn echo_cancellation_can_be_switched_while_running() {
    let (dev, hw) = MemoryDevice::new(48_000, 48_000);
    let mut engine = Engine::new(Config::default(), Box::new(dev)).unwrap();
    hw.feed_capture(&[0.0; 960]);
    engine.set_echo_cancellation(false);
    engine.pump();
    engine.set_echo_cancellation(true);
    engine.set_echo_cancellation(true);
    engine.pump();
    let _ = hw.play(960);
}
