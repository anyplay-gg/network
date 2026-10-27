//! Echo cancellation on real speech. The clips are LibriSpeech (CC BY 4.0, see
//! `tests/fixtures/librispeech/README.md`): one speaker is the far end, another is the player.
//! Prints a table of ERLE and of how much of the player's voice survives; the asserts are the
//! bounds the default settings must keep.

use anyplay_voice::EchoCanceller;

const RATE: usize = 48_000;
const BLOCK: usize = 480;
const SECS: usize = 28;

/// A 16 kHz 16-bit mono WAV, as the fixtures are, resampled to 48 kHz by linear interpolation.
fn wav(name: &str) -> Vec<f32> {
    let path = format!(
        "{}/tests/fixtures/librispeech/{name}.wav",
        env!("CARGO_MANIFEST_DIR")
    );
    let bytes = std::fs::read(path).unwrap();
    let data = bytes.windows(4).position(|w| w == b"data").unwrap() + 8;
    let pcm: Vec<f32> = bytes[data..]
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| f32::from(i16::from_le_bytes(*c)) / 32768.0)
        .collect();
    (0..pcm.len() * 3)
        .map(|i| {
            let (k, f) = (i / 3, (i % 3) as f32 / 3.0);
            pcm[k] * (1.0 - f) + pcm.get(k + 1).copied().unwrap_or(0.0) * f
        })
        .collect()
}

fn energy(x: &[f32]) -> f64 {
    x.iter().map(|v| f64::from(*v).powi(2)).sum::<f64>() / x.len().max(1) as f64
}

fn db(ratio: f64) -> f64 {
    10.0 * ratio.max(1e-30).log10()
}

fn dot(a: &[f32], b: &[f32]) -> f64 {
    a.iter().zip(b).map(|(a, b)| f64::from(a * b)).sum()
}

fn normalised(x: &mut [f32], rms: f32) {
    let g = rms / energy(x).sqrt() as f32;
    x.iter_mut().for_each(|v| *v *= g);
}

/// The room: delayed, reflected speakers. `extra_ms` is a fixed extra delay (a device with more
/// buffering than the canceller assumed); `drift_ms` makes the delay grow by that much over the
/// whole run, as it does when the speakers' clock runs slower than the microphone's.
struct Room {
    taps: Vec<(f64, f32)>,
    extra_samples: f64,
    drift_samples: f64,
    total: usize,
}

impl Room {
    fn new(extra_ms: f64, drift_ms: f64, total: usize) -> Room {
        Room {
            taps: [(40.0, 0.45), (48.0, 0.20), (63.0, 0.12), (91.0, 0.06)]
                .iter()
                .map(|(ms, g)| (ms * RATE as f64 / 1000.0, *g))
                .collect(),
            extra_samples: extra_ms * RATE as f64 / 1000.0,
            drift_samples: drift_ms * RATE as f64 / 1000.0,
            total,
        }
    }

    fn hear(&self, played: &[f32], n: usize) -> f32 {
        let drift = self.drift_samples * n as f64 / self.total as f64;
        self.taps
            .iter()
            .map(|(d, g)| {
                let pos = n as f64 - d - self.extra_samples - drift;
                if pos < 0.0 {
                    return 0.0;
                }
                let (i, f) = (pos as usize, pos.fract() as f32);
                if i + 1 >= played.len() {
                    return 0.0;
                }
                g * (played[i] * (1.0 - f) + played[i + 1] * f)
            })
            .sum()
    }
}

struct Run {
    echo: Vec<f32>,
    out: Vec<f32>,
}

/// `far` plays; the microphone hears the room's echo of it plus `near`.
fn run(far: &[f32], near: &[f32], room: &Room) -> Run {
    let mut canceller = EchoCanceller::new();
    let (mut echo, mut out) = (Vec::new(), Vec::new());
    for (b, block) in far.as_chunks::<BLOCK>().0.iter().enumerate() {
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
    Run { echo, out }
}

/// The best lag within 100 ms and the normalised correlation of `out` with `near` there.
fn follows(out: &[f32], near: &[f32]) -> (usize, f64) {
    // Decimated by 8 (the voice is in the first kilohertz that matters), which keeps it quick.
    let thin = |x: &[f32]| x.iter().step_by(8).copied().collect::<Vec<_>>();
    let (out, near) = (thin(out), thin(near));
    let mut best = (0, 0.0f64);
    for lag in 0..600 {
        let (o, n) = (&out[lag..], &near[..near.len() - lag]);
        let c = dot(o, n) / (dot(o, o) * dot(n, n)).sqrt().max(1e-12);
        if c > best.1 {
            best = (lag * 8, c);
        }
    }
    best
}

struct Scores {
    /// ERLE while only the far end talks, seconds 4 to 8.
    erle_far_only: f64,
    /// Echo against everything that is not the player in the output, during double talk.
    erle_double_talk: f64,
    /// Correlation of the output with the player during double talk, against the same with the
    /// far end silent (what the canceller does to the voice when it has nothing to cancel).
    kept: f64,
}

fn signals() -> (Vec<f32>, Vec<f32>) {
    let mut far: Vec<f32> = ["far-a", "far-b", "far-a", "far-b", "far-a"]
        .iter()
        .flat_map(|n| wav(n))
        .take(SECS * RATE)
        .collect();
    normalised(&mut far, 0.1);
    // The player is quiet until second 8, then talks over the far end.
    let mut near = vec![0.0f32; 8 * RATE];
    near.extend(["near-a", "near-b", "near-a"].iter().flat_map(|n| wav(n)));
    near.resize(far.len(), 0.0);
    normalised(&mut near[8 * RATE..26 * RATE], 0.1);
    (far, near)
}

fn measure(extra_ms: f64, drift_ms: f64) -> Scores {
    let (far, near) = signals();
    let room = Room::new(extra_ms, drift_ms, far.len());
    let with_far = run(&far, &near, &room);
    let near_only = run(&vec![0.0f32; far.len()], &near, &room);

    let far_only = 4 * RATE..8 * RATE;
    let erle_far_only =
        db(energy(&with_far.echo[far_only.clone()]) / energy(&with_far.out[far_only]));

    let dt = 10 * RATE..20 * RATE;
    let (o, n, e) = (
        &with_far.out[dt.clone()],
        &near[dt.clone()],
        &with_far.echo[dt.clone()],
    );
    // The echo that is left, read in the player's own pauses (frames of 20 ms where the player
    // is quiet and the far end is not): there the output can only be echo and what the canceller
    // left of the voice just before.
    let frame = RATE / 50;
    let (mut echo_energy, mut out_energy) = (0.0, 0.0);
    for f in 0..(n.len() / frame) {
        let r = f * frame..(f + 1) * frame;
        if energy(&n[r.clone()]).sqrt() < 0.01 && energy(&e[r.clone()]).sqrt() > 0.01 {
            echo_energy += energy(&e[r.clone()]);
            out_energy += energy(&o[r]);
        }
    }
    let erle_double_talk = db(echo_energy / out_energy.max(1e-12));
    let (_, together) = follows(o, n);
    let (_, alone) = follows(&near_only.out[dt.clone()], &near[dt]);
    Scores {
        erle_far_only,
        erle_double_talk,
        kept: together / alone,
    }
}

#[test]
fn speech_double_talk_and_drift() {
    println!("extra  drift   ERLE far only  ERLE in pauses  voice kept");
    for (extra, drift) in [
        (0.0, 0.0),
        (20.0, 0.0),
        (40.0, 0.0),
        (0.0, 20.0),
        (0.0, 40.0),
    ] {
        let s = measure(extra, drift);
        println!(
            "{extra:>3} ms {drift:>3} ms  {:>9.1} dB  {:>11.1} dB  {:>10.2}",
            s.erle_far_only, s.erle_double_talk, s.kept
        );
        let what = format!("extra {extra} ms, drift {drift} ms");
        assert!(
            s.erle_far_only > 15.0,
            "{what}: ERLE {:.1} dB",
            s.erle_far_only
        );
        assert!(s.kept > 0.5, "{what}: the voice kept only {:.2}", s.kept);
    }
}
