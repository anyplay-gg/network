//! The engine end to end, without hardware: a sender and a receiver joined by a simulated network,
//! each on an in-memory device. Time is simulated too, in steps of 20 ms.

use std::f32::consts::TAU;
use std::time::{Duration, Instant};

use anyplay_voice::wire::Frame;
use anyplay_voice::{
    Config, Engine, MemoryDevice, MemoryHandle, PeerId, Rejected, TalkMode, VadConfig,
};

const STEP_MS: u64 = 20;
const SENDER: PeerId = PeerId(1);

// ---------------------------------------------------------------- signals

fn sine(rate: u32, hz: f32, amp: f32, start: usize, n: usize) -> Vec<f32> {
    (start..start + n)
        .map(|i| amp * (TAU * hz * i as f32 / rate as f32).sin())
        .collect()
}

/// Something that moves like a voice: a 120 Hz pitch with harmonics shaped by two formants, a
/// syllable rhythm of about 4 per second, and a little breath noise.
fn voiced(rate: u32, secs: f32, amp: f32) -> Vec<f32> {
    let mut rng = Lcg(5);
    (0..(rate as f32 * secs) as usize)
        .map(|i| {
            let t = i as f32 / rate as f32;
            let f0 = 120.0 + 8.0 * (TAU * 0.7 * t).sin();
            let envelope = (0.5 + 0.5 * (TAU * 4.0 * t).sin()).powf(0.7);
            let mut v = 0.0;
            for h in 1..30 {
                let f = f0 * h as f32;
                if f > rate as f32 / 2.2 {
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

struct Lcg(u32);

impl Lcg {
    fn next(&mut self) -> u32 {
        self.0 = self.0.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        self.0 >> 8
    }
    fn signed(&mut self) -> f32 {
        (self.next() % 20001) as f32 / 10000.0 - 1.0
    }
    fn percent(&mut self) -> u32 {
        self.next() % 100
    }
}

fn rms(x: &[f32]) -> f32 {
    (x.iter().map(|v| v * v).sum::<f32>() / x.len().max(1) as f32).sqrt()
}

/// Signal-to-noise ratio of `output` against `reference` in dB, after finding the delay and the
/// gain that fit best. Both at 48 kHz.
fn snr_db(reference: &[f32], output: &[f32]) -> f32 {
    // Coarse delay on a signal decimated by 16, then refined at full rate.
    let dec = |x: &[f32]| x.iter().step_by(16).copied().collect::<Vec<_>>();
    let (r, o) = (dec(reference), dec(output));
    let window = (r.len() / 2).min(o.len() / 2);
    let from = r.len() / 4;
    let mut best = (0usize, f32::MIN);
    for lag in 0..(o.len() - from - window).min(1500) {
        let c: f32 = (0..window).map(|i| r[from + i] * o[from + lag + i]).sum();
        if c > best.1 {
            best = (lag, c);
        }
    }
    let coarse = best.0 * 16;
    let start = reference.len() / 4;
    let len = reference.len() / 2;
    let mut result = f32::MIN;
    for lag in coarse.saturating_sub(24)..coarse + 24 {
        if start + lag + len > output.len() {
            continue;
        }
        let r = &reference[start..start + len];
        let o = &output[start + lag..start + lag + len];
        let rr: f32 = r.iter().map(|v| v * v).sum();
        let ro: f32 = r.iter().zip(o).map(|(a, b)| a * b).sum();
        let g = ro / rr;
        let err: f32 = r.iter().zip(o).map(|(a, b)| (b - g * a).powi(2)).sum();
        result = result.max(10.0 * (g * g * rr / err.max(1e-12)).log10());
    }
    result
}

/// How much of the played sound is the tone, in dB: the signal is cut into 100 ms windows and a
/// sine of the given frequency (any phase, any level) is fitted to each. A loss that shifts the
/// timing by a few milliseconds does not matter to this; noise and holes do.
fn tone_snr_db(played: &[f32], hz: f32, from: usize, to: usize) -> f32 {
    let mut signal = 0.0f64;
    let mut error = 0.0f64;
    for w in played[from..to].as_chunks::<4800>().0 {
        let (mut ss, mut cc, mut sc, mut xs, mut xc, mut xx) = (0.0f64, 0.0, 0.0, 0.0, 0.0, 0.0);
        for (i, x) in w.iter().enumerate() {
            let p = f64::from(TAU) * f64::from(hz) * i as f64 / 48_000.0;
            let (s, c) = (p.sin(), p.cos());
            let x = f64::from(*x);
            ss += s * s;
            cc += c * c;
            sc += s * c;
            xs += x * s;
            xc += x * c;
            xx += x * x;
        }
        let det = ss * cc - sc * sc;
        let a = (xs * cc - xc * sc) / det;
        let b = (xc * ss - xs * sc) / det;
        let explained = a * xs + b * xc;
        signal += explained;
        error += (xx - explained).max(1e-12);
    }
    (10.0 * (signal / error).log10()) as f32
}

// ---------------------------------------------------------------- the rig

struct Rig {
    sender: Engine,
    sender_hw: MemoryHandle,
    receiver: Engine,
    receiver_hw: MemoryHandle,
    start: Instant,
    now_ms: u64,
    in_flight: Vec<(u64, Vec<u8>)>,
    capture_rate: u32,
    playback_rate: u32,
    /// What the receiver's speakers played.
    played: Vec<f32>,
    sent: usize,
    delivered: usize,
}

impl Rig {
    fn new(config: Config) -> Rig {
        Rig::with_rates(config, 48_000, 48_000)
    }

    fn with_rates(config: Config, capture_rate: u32, playback_rate: u32) -> Rig {
        let (sd, sender_hw) = MemoryDevice::new(capture_rate, 48_000);
        let (rd, receiver_hw) = MemoryDevice::new(48_000, playback_rate);
        Rig {
            sender: Engine::new(config.clone(), Box::new(sd)).unwrap(),
            sender_hw,
            receiver: Engine::new(config, Box::new(rd)).unwrap(),
            receiver_hw,
            start: Instant::now(),
            now_ms: 0,
            in_flight: Vec::new(),
            capture_rate,
            playback_rate,
            played: Vec::new(),
            sent: 0,
            delivered: 0,
        }
    }

    /// One 20 ms step. `mic` is the microphone's next 20 ms (at the capture rate); `net` says,
    /// for each datagram, after how many ms copies of it arrive (none: lost).
    fn step(&mut self, mic: &[f32], net: &mut dyn FnMut(usize, &[u8]) -> Vec<u64>) {
        assert_eq!(mic.len(), (self.capture_rate / 50) as usize);
        self.sender_hw.feed_capture(mic);
        self.sender.pump();
        for d in self.sender.outgoing() {
            for delay in net(self.sent, &d) {
                self.in_flight.push((self.now_ms + delay, d.clone()));
            }
            self.sent += 1;
        }
        self.now_ms += STEP_MS;
        let due = self.now_ms;
        let (ready, rest): (Vec<_>, Vec<_>) =
            self.in_flight.drain(..).partition(|(t, _)| *t <= due);
        self.in_flight = rest;
        let mut ready = ready;
        ready.sort_by_key(|(t, _)| *t);
        for (t, d) in ready {
            self.receiver
                .incoming_at(SENDER, &d, self.start + Duration::from_millis(t))
                .unwrap();
            self.delivered += 1;
        }
        self.receiver.pump();
        let n = (self.playback_rate / 50) as usize;
        self.played.extend(self.receiver_hw.play(n));
    }

    /// Run a whole signal through, plus a second of silence to let the tail play.
    fn run(
        &mut self,
        signal: &[f32],
        talking_until_end: bool,
        net: &mut dyn FnMut(usize, &[u8]) -> Vec<u64>,
    ) {
        let n = (self.capture_rate / 50) as usize;
        self.sender.set_talking(true);
        for chunk in signal.chunks_exact(n) {
            self.step(chunk, net);
        }
        if !talking_until_end {
            self.sender.set_talking(false);
        }
        for _ in 0..50 {
            self.step(&vec![0.0; n], net);
        }
    }
}

fn clean(_: usize, _: &[u8]) -> Vec<u64> {
    vec![0]
}

/// Tones are not speech, so the codec would treat them as silence with DTX on. The tests that use
/// them switch it off, and the ones about DTX use a voice.
fn no_ns() -> Config {
    Config {
        noise_suppression: false,
        dtx: false,
        ..Config::default()
    }
}

/// The reference at 48 kHz for a rate-converted run is the signal itself, resampled by the test.
fn ref48(signal: &[f32]) -> Vec<f32> {
    signal.to_vec()
}

// ---------------------------------------------------------------- codec round trip

#[test]
fn a_sine_survives_the_whole_pipeline() {
    let signal = sine(48_000, 440.0, 0.3, 0, 48_000 * 3);
    let mut rig = Rig::new(no_ns());
    rig.run(&signal, false, &mut clean);
    let snr = snr_db(&ref48(&signal), &rig.played);
    println!("sine SNR {snr:.1} dB");
    assert!(snr > 20.0, "SNR {snr:.1} dB");
}

#[test]
fn speech_like_audio_survives_the_whole_pipeline() {
    let signal = voiced(48_000, 4.0, 0.5);
    // With DTX on, as in a party.
    let mut rig = Rig::new(Config {
        noise_suppression: false,
        ..Config::default()
    });
    rig.run(&signal, false, &mut clean);
    let snr = snr_db(&ref48(&signal), &rig.played);
    println!("voiced SNR {snr:.1} dB");
    assert!(snr > 4.0, "SNR {snr:.1} dB");
    let level = rms(&rig.played[48_000..48_000 * 3]) / rms(&signal[..48_000 * 2]);
    assert!((0.5..1.5).contains(&level), "level ratio {level}");
}

#[test]
fn devices_at_other_rates_work() {
    let signal = sine(44_100, 440.0, 0.3, 0, 44_100 * 3);
    let mut rig = Rig::with_rates(no_ns(), 44_100, 44_100);
    rig.run(&signal, false, &mut clean);
    // Compare at 44.1 kHz: what the speakers played against what the microphone heard.
    let snr = snr_db(&signal, &rig.played);
    println!("44.1 kHz both ways, SNR {snr:.1} dB");
    assert!(snr > 18.0, "SNR {snr:.1} dB");
}

#[test]
fn noise_suppression_quiets_steady_noise() {
    let mut rng = Lcg(99);
    let noise: Vec<f32> = (0..48_000 * 3).map(|_| 0.05 * rng.signed()).collect();
    let mut loud = Rig::new(no_ns());
    loud.run(&noise, false, &mut clean);
    let mut quiet = Rig::new(Config::default());
    quiet.run(&noise, false, &mut clean);
    let span = 48_000..48_000 * 2;
    let (a, b) = (rms(&loud.played[span.clone()]), rms(&quiet.played[span]));
    println!("noise {a:.4} -> {b:.4}");
    assert!(b < a * 0.3, "{a} vs {b}");
}

// ---------------------------------------------------------------- the network

/// The longest run of near-silence inside the talk, in ms.
fn longest_gap_ms(played: &[f32], from: usize, to: usize) -> usize {
    let mut longest = 0;
    let mut run = 0;
    for chunk in played[from..to].chunks(48) {
        if rms(chunk) < 0.002 {
            run += 1;
            longest = longest.max(run);
        } else {
            run = 0;
        }
    }
    longest
}

fn loss_run(percent: u32) -> (Rig, f32) {
    loss_run_with(percent, no_ns())
}

fn loss_run_with(percent: u32, config: Config) -> (Rig, f32) {
    let signal = sine(48_000, 330.0, 0.3, 0, 48_000 * 6);
    let mut rig = Rig::new(config);
    let mut rng = Lcg(percent + 3);
    rig.run(&signal, false, &mut |_, _| {
        if rng.percent() < percent {
            vec![]
        } else {
            vec![0]
        }
    });
    let snr = tone_snr_db(&rig.played, 330.0, 48_000, 48_000 * 5);
    (rig, snr)
}

#[test]
fn ten_percent_loss_is_barely_heard() {
    let (rig, snr) = loss_run(10);
    let stats = rig.receiver.peer_stats(SENDER).unwrap();
    println!("10% loss: SNR {snr:.1} dB, {stats:?}");
    assert!(stats.recovered > 0);
    // Almost every loss is rebuilt from the next frame or concealed: no hole longer than 60 ms.
    let gap = longest_gap_ms(&rig.played, 48_000, 48_000 * 5);
    assert!(gap <= 3, "silent gap of {gap} ms");
    assert!(snr > 6.0, "SNR {snr:.1} dB");
}

#[test]
fn thirty_percent_loss_still_carries_the_tone() {
    let (rig, snr) = loss_run(30);
    let stats = rig.receiver.peer_stats(SENDER).unwrap();
    println!("30% loss: SNR {snr:.1} dB, {stats:?}");
    assert!(stats.recovered + stats.concealed > 0);
    let gap = longest_gap_ms(&rig.played, 48_000, 48_000 * 5);
    assert!(gap <= 6, "silent gap of {gap} ms");
    assert!(snr > 0.0, "SNR {snr:.1} dB");
}

#[test]
fn the_redundancy_in_the_frames_pays_off() {
    let without = Config {
        expected_loss_percent: 0,
        ..no_ns()
    };
    let with = Config {
        expected_loss_percent: 20,
        ..no_ns()
    };
    let (_, a) = loss_run_with(20, without);
    let (rig, b) = loss_run_with(20, with);
    println!("20% loss: {a:.1} dB without redundancy, {b:.1} dB with");
    assert!(rig.receiver.peer_stats(SENDER).unwrap().recovered > 0);
    assert!(b > a, "{a:.1} vs {b:.1}");
}

#[test]
fn reordering_and_duplicates_change_nothing_audible() {
    let signal = sine(48_000, 330.0, 0.3, 0, 48_000 * 5);
    let mut ideal = Rig::new(no_ns());
    ideal.run(&signal, false, &mut clean);
    let mut rig = Rig::new(no_ns());
    let mut rng = Lcg(21);
    rig.run(&signal, false, &mut |_, _| {
        // 0 to 35 ms of jitter, so neighbours swap, and one datagram in ten arrives twice.
        let mut v = vec![(rng.next() % 36) as u64];
        if rng.percent() < 10 {
            v.push((rng.next() % 36) as u64 + 5);
        }
        v
    });
    let stats = rig.receiver.peer_stats(SENDER).unwrap();
    let snr = snr_db(&ref48(&signal), &rig.played);
    println!("jitter and duplicates: SNR {snr:.1} dB, {stats:?}");
    assert!(stats.duplicates > 0);
    assert!(snr > 15.0, "SNR {snr:.1} dB");
    assert!(stats.depth >= 2);
}

// ---------------------------------------------------------------- talk gate, DTX, end of talk

#[test]
fn push_to_talk_sends_only_while_down_and_ends_with_the_flag() {
    let signal = voiced(48_000, 1.0, 0.5);
    let (device, hw) = MemoryDevice::new(48_000, 48_000);
    let mut e = Engine::new(no_ns(), Box::new(device)).unwrap();

    // Not talking: nothing is sent.
    hw.feed_capture(&signal);
    e.pump();
    assert!(e.outgoing().is_empty());

    e.set_talking(true);
    hw.feed_capture(&signal);
    e.pump();
    let talk = e.outgoing();
    assert_eq!(talk.len(), 50);

    e.set_talking(false);
    hw.feed_capture(&signal);
    e.pump();
    let tail = e.outgoing();
    assert_eq!(tail.len(), 1, "one closing frame, then nothing");
    let all: Vec<Frame> = talk
        .iter()
        .chain(&tail)
        .map(|d| Frame::parse(d).unwrap())
        .collect();
    assert!(all[..50].iter().all(|f| !f.end_of_talk));
    assert!(all[50].end_of_talk);
    // Sequence numbers count the frames sent; timestamps count the time passed.
    assert!(
        all.windows(2)
            .all(|w| w[1].sequence == w[0].sequence.wrapping_add(1))
    );
    assert_eq!(all[1].timestamp - all[0].timestamp, 960);

    // The next talk continues the sequence and the clock moved on through the silence.
    e.set_talking(true);
    hw.feed_capture(&signal[..960]);
    e.pump();
    let next = Frame::parse(&e.outgoing()[0]).unwrap().timestamp;
    assert!(next > all[50].timestamp + 49 * 960);
}

#[test]
fn a_muted_microphone_sends_nothing_whatever_the_gate_says() {
    let signal = voiced(48_000, 0.5, 0.5);
    let (device, hw) = MemoryDevice::new(48_000, 48_000);
    let mut e = Engine::new(no_ns(), Box::new(device)).unwrap();
    e.set_talking(true);
    e.set_mic_muted(true);
    hw.feed_capture(&signal);
    e.pump();
    assert!(e.outgoing().is_empty(), "muted: nothing leaves");
    assert!(!e.levels().me.speaking);

    e.set_mic_muted(false);
    hw.feed_capture(&signal);
    e.pump();
    assert_eq!(e.outgoing().len(), 25, "unmuted: the gate decides again");
    assert!(e.levels().me.speaking);
}

#[test]
fn deafened_silences_the_speakers_and_the_microphone_but_not_the_buffers() {
    let signal = voiced(48_000, 1.0, 0.5);
    let mut rig = Rig::new(no_ns());
    rig.receiver.set_deafened(true);
    rig.run(&signal, true, &mut clean);
    assert!(
        rig.played.iter().all(|s| s.abs() < 1e-6),
        "deafened: nothing is played"
    );
    // The frames still arrived, so hearing resumes at once.
    assert!(rig.delivered > 40);
    rig.receiver.set_deafened(false);
    let mut again = Rig::new(no_ns());
    again.run(&signal, true, &mut clean);
    assert!(again.played.iter().any(|s| s.abs() > 0.05));

    // A deafened player is not heard either.
    let (device, hw) = MemoryDevice::new(48_000, 48_000);
    let mut e = Engine::new(no_ns(), Box::new(device)).unwrap();
    e.set_talking(true);
    e.set_deafened(true);
    hw.feed_capture(&signal);
    e.pump();
    assert!(e.outgoing().is_empty());
    e.set_deafened(false);
    hw.feed_capture(&signal);
    e.pump();
    assert!(!e.outgoing().is_empty());
}

#[test]
fn silence_while_the_key_is_down_costs_almost_nothing() {
    let (device, hw) = MemoryDevice::new(48_000, 48_000);
    let mut e = Engine::new(
        Config {
            noise_suppression: false,
            ..Config::default()
        },
        Box::new(device),
    )
    .unwrap();
    e.set_talking(true);
    // Warm up on speech so the encoder is in its normal state, then two seconds of digital silence.
    hw.feed_capture(&voiced(48_000, 0.5, 0.5));
    e.pump();
    let _ = e.outgoing();
    hw.feed_capture(&vec![0.0; 96_000]);
    e.pump();
    let frames = e.outgoing();
    let bytes: usize = frames.iter().map(Vec::len).sum();
    println!(
        "2 s of silence with the key down: {} datagrams, {bytes} bytes",
        frames.len()
    );
    // 100 frames of silence: the encoder goes quiet within a few frames and then only keeps alive.
    assert!(frames.len() < 30, "{} datagrams", frames.len());
    assert!(bytes < 600);
}

#[test]
fn the_receiver_stops_at_the_end_of_talk_without_concealing() {
    let signal = sine(48_000, 330.0, 0.3, 0, 48_000 * 2);
    let mut rig = Rig::new(no_ns());
    rig.run(&signal, false, &mut clean);
    let stats = rig.receiver.peer_stats(SENDER).unwrap();
    // The device asks for audio ahead of the first datagram once, which costs one hold.
    assert!(stats.concealed <= 1, "{stats:?}");
    assert_eq!(stats.recovered, 0);
    assert!(!rig.receiver.levels().peers[0].1.speaking);
    // The last half second of the run is silence.
    let tail = &rig.played[rig.played.len() - 24_000..];
    assert!(rms(tail) < 1e-4, "{}", rms(tail));
}

#[test]
fn voice_activity_stays_closed_on_silence_and_opens_on_a_voice() {
    let (device, hw) = MemoryDevice::new(48_000, 48_000);
    let config = Config {
        mode: TalkMode::VoiceActivity,
        vad: VadConfig::default(),
        ..Config::default()
    };
    let mut e = Engine::new(config, Box::new(device)).unwrap();

    hw.feed_capture(&vec![0.0; 48_000]);
    e.pump();
    assert!(e.outgoing().is_empty(), "silence must not be sent");
    assert!(!e.levels().me.speaking);

    hw.feed_capture(&voiced(48_000, 3.0, 0.7));
    e.pump();
    let sent = e.outgoing();
    println!(
        "voice activity: {} datagrams for 3 s of a voice",
        sent.len()
    );
    assert!(sent.len() > 20, "{} datagrams", sent.len());

    hw.feed_capture(&vec![0.0; 48_000]);
    e.pump();
    let tail = e.outgoing();
    let last = tail.last().map(|d| Frame::parse(d).unwrap());
    assert!(
        last.is_some_and(|f| f.end_of_talk),
        "the talk must end with the flag"
    );
    assert!(!e.levels().me.speaking);
}

// ---------------------------------------------------------------- mixing, volume, mute

/// A sender's datagrams for a loud tone, recorded once and replayed to several peers.
fn recorded_tone(secs: usize, amp: f32) -> Vec<Vec<u8>> {
    let (device, hw) = MemoryDevice::new(48_000, 48_000);
    let mut e = Engine::new(no_ns(), Box::new(device)).unwrap();
    e.set_talking(true);
    hw.feed_capture(&sine(48_000, 500.0, amp, 0, 48_000 * secs));
    e.pump();
    e.set_talking(false);
    hw.feed_capture(&[0.0; 960]);
    e.pump();
    let mut v = e.outgoing();
    // `outgoing` keeps 100 datagrams at most: a shorter recording than that is enough here.
    assert!(!v.is_empty());
    v.truncate(100);
    v
}

/// Feed recorded datagrams to a receiver as peers `ids`, playing 20 ms per datagram.
fn play_back(
    receiver: &mut Engine,
    hw: &MemoryHandle,
    datagrams: &[Vec<u8>],
    ids: &[PeerId],
) -> Vec<f32> {
    let start = Instant::now();
    let mut played = Vec::new();
    for (i, d) in datagrams.iter().enumerate() {
        for id in ids {
            receiver
                .incoming_at(*id, d, start + Duration::from_millis(20 * i as u64))
                .unwrap();
        }
        receiver.pump();
        played.extend(hw.play(960));
    }
    for _ in 0..10 {
        receiver.pump();
        played.extend(hw.play(960));
    }
    played
}

fn receiver() -> (Engine, MemoryHandle) {
    let (device, hw) = MemoryDevice::new(48_000, 48_000);
    (Engine::new(no_ns(), Box::new(device)).unwrap(), hw)
}

#[test]
fn seven_loud_peers_are_limited_not_clipped() {
    let tone = recorded_tone(2, 0.9);
    let ids: Vec<PeerId> = (1..=7).map(PeerId).collect();
    let (mut r, hw) = receiver();
    let played = play_back(&mut r, &hw, &tone, &ids);
    let peak = played.iter().fold(0.0f32, |m, s| m.max(s.abs()));
    println!("7 peers at 0.9 each: peak {peak:.4}");
    assert!(peak <= 1.0, "peak {peak}");
    assert!(peak > 0.99, "the sum should reach the limiter, peak {peak}");
    assert!(played.iter().all(|s| s.is_finite()));

    // One peer alone passes through at its own level.
    let (mut one, hw1) = receiver();
    let solo = play_back(&mut one, &hw1, &tone, &ids[..1]);
    let solo_peak = solo.iter().fold(0.0f32, |m, s| m.max(s.abs()));
    assert!((0.8..1.0).contains(&solo_peak), "solo peak {solo_peak}");
}

#[test]
fn two_peers_add_up() {
    let tone = recorded_tone(2, 0.2);
    let (mut a, ha) = receiver();
    let one = play_back(&mut a, &ha, &tone, &[PeerId(1)]);
    let (mut b, hb) = receiver();
    let two = play_back(&mut b, &hb, &tone, &[PeerId(1), PeerId(2)]);
    let r = rms(&two[10_000..40_000]) / rms(&one[10_000..40_000]);
    assert!((1.9..2.1).contains(&r), "ratio {r}");
}

#[test]
fn volume_and_mute_apply_per_peer() {
    let tone = recorded_tone(2, 0.4);
    let ids = [PeerId(1), PeerId(2)];

    let (mut full, hf) = receiver();
    let both = play_back(&mut full, &hf, &tone, &ids);

    // Peer 2 at half volume: 1 + 0.5 of one peer instead of 2.
    let (mut half, hh) = receiver();
    half.set_peer_volume(PeerId(2), 0.5);
    let mixed = play_back(&mut half, &hh, &tone, &ids);
    let r = rms(&mixed[10_000..40_000]) / rms(&both[10_000..40_000]);
    assert!((0.72..0.78).contains(&r), "ratio {r}");

    // Peer 2 muted: only peer 1 is heard, and peer 2 does not show as speaking.
    let (mut muted, hm) = receiver();
    muted.mute(PeerId(2), true);
    let start = Instant::now();
    let mut played = Vec::new();
    for (i, d) in tone.iter().enumerate().take(60) {
        for id in ids {
            muted
                .incoming_at(id, d, start + Duration::from_millis(20 * i as u64))
                .unwrap();
        }
        muted.pump();
        played.extend(hm.play(960));
    }
    let levels = muted.levels();
    assert!(levels.peers[0].1.speaking, "{levels:?}");
    assert!(!levels.peers[1].1.speaking, "{levels:?}");
    assert_eq!(levels.peers[1].1.rms, 0.0);
    let (mut one, h1) = receiver();
    let solo = play_back(&mut one, &h1, &tone, &ids[..1]);
    let r = rms(&played[10_000..40_000]) / rms(&solo[10_000..40_000]);
    assert!((0.95..1.05).contains(&r), "ratio {r}");

    // Volume zero is silence; a volume out of range is held to 2.0 and NaN to the default.
    let (mut zero, hz) = receiver();
    zero.set_peer_volume(PeerId(1), 0.0);
    let silent = play_back(&mut zero, &hz, &tone, &ids[..1]);
    assert!(rms(&silent) < 1e-6);
    let (mut loud, hl) = receiver();
    loud.set_peer_volume(PeerId(1), f32::NAN);
    let normal = play_back(&mut loud, &hl, &tone, &ids[..1]);
    assert!((rms(&normal) / rms(&solo) - 1.0).abs() < 0.01);
}

#[test]
fn levels_show_who_is_speaking() {
    let tone = recorded_tone(1, 0.4);
    let (mut r, hw) = receiver();
    let start = Instant::now();
    for (i, d) in tone.iter().enumerate().take(40) {
        r.incoming_at(PeerId(3), d, start + Duration::from_millis(20 * i as u64))
            .unwrap();
        r.pump();
        let _ = hw.play(960);
    }
    let levels = r.levels();
    assert_eq!(levels.peers.len(), 1);
    assert_eq!(levels.peers[0].0, PeerId(3));
    assert!(levels.peers[0].1.speaking);
    assert!(levels.peers[0].1.rms > 0.05);
    r.remove_peer(PeerId(3));
    assert!(r.levels().peers.is_empty());
}

// ---------------------------------------------------------------- what it refuses

#[test]
fn datagrams_that_are_not_voice_are_refused() {
    let (mut r, _hw) = receiver();
    assert!(matches!(
        r.incoming(PeerId(1), &[]),
        Err(Rejected::Malformed(_))
    ));
    assert!(matches!(
        r.incoming(PeerId(1), &[9; 40]),
        Err(Rejected::Malformed(_))
    ));

    // A well-formed frame whose Opus packet is 40 ms long.
    let mut enc =
        opus::Encoder::new(48_000, opus::Channels::Mono, opus::Application::Voip).unwrap();
    let mut packet = [0u8; 400];
    let n = enc.encode_float(&[0.1; 1920], &mut packet).unwrap();
    let frame = Frame {
        sequence: 0,
        timestamp: 0,
        end_of_talk: false,
        payload: &packet[..n],
    }
    .encode();
    assert_eq!(
        r.incoming(PeerId(1), &frame),
        Err(Rejected::NotTwentyMilliseconds)
    );

    // Not Opus at all.
    let junk = Frame {
        sequence: 0,
        timestamp: 0,
        end_of_talk: false,
        payload: &[0xFF, 0xFF, 0xFF],
    }
    .encode();
    assert!(r.incoming(PeerId(1), &junk).is_err());
    assert!(r.levels().peers.is_empty() || r.peer_stats(PeerId(1)).unwrap().received == 0);
}

#[test]
fn an_eighth_peer_is_refused_and_a_known_one_still_works() {
    let tone = recorded_tone(1, 0.3);
    let (mut r, _hw) = receiver();
    for id in 1..=7 {
        r.incoming(PeerId(id), &tone[0]).unwrap();
    }
    assert_eq!(r.incoming(PeerId(8), &tone[0]), Err(Rejected::TooManyPeers));
    r.incoming(PeerId(3), &tone[1]).unwrap();
    // Making room lets the eighth in.
    r.remove_peer(PeerId(1));
    r.incoming(PeerId(8), &tone[0]).unwrap();
}

#[test]
fn settings_out_of_range_are_refused_at_the_start() {
    let bad = |config: Config| {
        let (device, _hw) = MemoryDevice::new(48_000, 48_000);
        Engine::new(config, Box::new(device)).is_err()
    };
    assert!(bad(Config {
        bitrate: 1_000,
        ..Config::default()
    }));
    assert!(bad(Config {
        bitrate: 500_000,
        ..Config::default()
    }));
    assert!(bad(Config {
        expected_loss_percent: 101,
        ..Config::default()
    }));
    let (device, _hw) = MemoryDevice::new(100, 48_000);
    assert!(Engine::new(Config::default(), Box::new(device)).is_err());
}

#[test]
fn bit_rate_is_in_the_planned_range() {
    let signal = voiced(48_000, 4.0, 0.5);
    let (device, hw) = MemoryDevice::new(48_000, 48_000);
    let mut e = Engine::new(Config::default(), Box::new(device)).unwrap();
    e.set_talking(true);
    let mut bytes = 0;
    let mut frames = 0;
    for chunk in signal.chunks(48_000) {
        hw.feed_capture(chunk);
        e.pump();
        for d in e.outgoing() {
            bytes += d.len();
            frames += 1;
        }
    }
    let kbps = bytes as f32 * 8.0 / 4.0 / 1000.0;
    println!("{frames} datagrams, {kbps:.1} kbit/s including the 8 header bytes");
    assert!((15.0..40.0).contains(&kbps), "{kbps}");
}

// ---------------------------------------------------------------- CPU

/// Encoding for ourselves and decoding seven peers in real time must stay far below one core.
/// Run it with `cargo test -p anyplay-voice --release -- --ignored --nocapture`.
#[test]
#[ignore = "timing is only meaningful in release builds"]
fn seven_peers_and_a_microphone_take_a_small_share_of_a_core() {
    let seconds = 20usize;
    let tone = {
        let (device, hw) = MemoryDevice::new(48_000, 48_000);
        let mut e = Engine::new(Config::default(), Box::new(device)).unwrap();
        e.set_talking(true);
        let voice = voiced(48_000, seconds as f32, 0.5);
        let mut all = Vec::new();
        for chunk in voice.chunks(960) {
            hw.feed_capture(chunk);
            e.pump();
            all.extend(e.outgoing());
        }
        all
    };
    let mic = voiced(48_000, seconds as f32, 0.5);
    let (device, hw) = MemoryDevice::new(48_000, 48_000);
    let mut e = Engine::new(Config::default(), Box::new(device)).unwrap();
    e.set_talking(true);
    let start = Instant::now();
    let t0 = Instant::now();
    for (i, chunk) in mic.chunks(960).enumerate() {
        if let Some(d) = tone.get(i) {
            for id in 1..=7 {
                e.incoming_at(PeerId(id), d, t0 + Duration::from_millis(20 * i as u64))
                    .unwrap();
            }
        }
        hw.feed_capture(chunk);
        e.pump();
        let _ = e.outgoing();
        let _ = hw.play(960);
    }
    let used = start.elapsed().as_secs_f64();
    let share = used / seconds as f64;
    println!(
        "{seconds} s of a party of eight took {used:.3} s: {:.2} % of one core",
        share * 100.0
    );
    assert!(share < 0.10, "{:.2} % of a core", share * 100.0);
}
