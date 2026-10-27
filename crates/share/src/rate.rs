//! The rate controller of the section "Congestion control and the rate" of
//! `spec/share-play.md`: a pure function of what the guest's `report`s and the connection tell
//! it, with the time passed in. No clock, no socket.
//!
//! One call to [`RateController::on_input`] per report (every 100 ms). It returns the
//! [`Target`] the encoder and the packetiser follow.
//!
//! How the numbers of the spec are read:
//!
//! * The **budget** is what the path may carry in all, video and parity together. It starts at
//!   4,000 kbit/s, rises 25% a step until a limit shows and 10% a step after that, falls to
//!   80% at once when a report shows loss, a rising delay trend or a queue, and never exceeds
//!   the player's maximum or the path maximum.
//! * The delivery rate (`0.85 x` of it) is applied when a report shows trouble, as a ceiling for
//!   the cut. Applied always, it would pin the budget to what was sent so far and nothing could
//!   rise.
//! * The ladder follows the budget. `Target::kbps` is the encoder's share of it: the budget
//!   divided by `1 + parity`.
//! * Every change of size or frame rate raises `Target::generation`: the caller starts a new
//!   epoch with a key frame.

use std::collections::VecDeque;

/// Which way the share travels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PathKind {
    Relay,
    Direct,
}

/// The numbers of the spec as settings. `Default` is the spec's starting values.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RateConfig {
    pub player_max_kbps: u32,
    pub relay_max_kbps: u32,
    pub direct_max_kbps: u32,
    pub start_kbps: u32,
    /// The budget never falls below this.
    pub floor_kbps: u32,
    /// Loss (percent of packets) above which the rate falls.
    pub loss_down_percent: f64,
    /// Loss above which the rate does not rise.
    pub loss_up_percent: f64,
    /// Delay trend (microseconds per 100 ms) above which the rate falls.
    pub trend_down_us: i32,
    /// Delay trend above which the rate does not rise ("flat" is at or below).
    pub trend_flat_us: i32,
    pub down_factor: f64,
    pub down_interval_us: u64,
    /// Quiet time before a rise.
    pub up_wait_us: u64,
    pub start_step: f64,
    pub up_step: f64,
    pub delivery_factor: f64,
    /// Time below a rung before the ladder moves down.
    pub rung_down_hold_us: u64,
    /// Time above `rung_up_margin` times the next rung before the ladder moves up.
    pub rung_up_hold_us: u64,
    pub rung_up_margin: f64,
    /// Loss is judged over this window to set parity.
    pub parity_window_us: u64,
    /// Parity stays 0 while loss over the window is below this percent.
    pub parity_off_percent: f64,
    pub parity_step: u8,
    pub parity_max: u8,
}

impl Default for RateConfig {
    fn default() -> RateConfig {
        RateConfig {
            player_max_kbps: 15_000,
            relay_max_kbps: 6_000,
            direct_max_kbps: 25_000,
            start_kbps: 4_000,
            floor_kbps: 300,
            loss_down_percent: 2.0,
            loss_up_percent: 0.5,
            trend_down_us: 4_000,
            trend_flat_us: 1_000,
            down_factor: 0.8,
            down_interval_us: 300_000,
            up_wait_us: 1_000_000,
            start_step: 1.25,
            up_step: 1.10,
            delivery_factor: 0.85,
            rung_down_hold_us: 1_000_000,
            rung_up_hold_us: 8_000_000,
            rung_up_margin: 1.2,
            parity_window_us: 2_000_000,
            parity_off_percent: 0.5,
            parity_step: 5,
            parity_max: 30,
        }
    }
}

/// One step of the ladder: from this budget up, this size and rate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rung {
    pub min_kbps: u32,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
}

/// 1080p60, 720p60, 720p30, 540p30; below the last, the video pauses.
pub const LADDER: [Rung; 4] = [
    Rung {
        min_kbps: 10_000,
        width: 1920,
        height: 1080,
        fps: 60,
    },
    Rung {
        min_kbps: 5_000,
        width: 1280,
        height: 720,
        fps: 60,
    },
    Rung {
        min_kbps: 2_500,
        width: 1280,
        height: 720,
        fps: 30,
    },
    Rung {
        min_kbps: 1_000,
        width: 960,
        height: 540,
        fps: 30,
    },
];

/// What the encoder and the packetiser follow.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Target {
    /// The encoder's bitrate; 0 while paused.
    pub kbps: u32,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    /// The packetiser's `r`, in percent.
    pub parity_percent: u8,
    /// The video is paused and the sound goes on.
    pub paused: bool,
    /// Goes up with every change of size, rate or pause: start a new epoch with a key frame.
    pub generation: u32,
}

/// What one report and the connection say.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RateInput {
    pub now_us: u64,
    /// Packets received and lost since the last report.
    pub received: u32,
    pub lost: u32,
    /// Growth of the one-way delay over the last 100 ms, in microseconds.
    pub delay_trend_us: i32,
    /// Frames waiting in the send queue.
    pub send_queue_frames: u32,
    /// What the connection delivered recently, if it knows.
    pub delivery_kbps: Option<u32>,
}

/// The controller. See the module documentation.
#[derive(Clone, Debug)]
pub struct RateController {
    cfg: RateConfig,
    path: PathKind,
    budget: f64,
    slow_start: bool,
    last_bad_us: u64,
    last_up_us: u64,
    last_down_us: Option<u64>,
    /// Index into [`LADDER`]; `LADDER.len()` is the pause.
    rung: usize,
    below_since: Option<u64>,
    above_since: Option<u64>,
    window: VecDeque<(u64, u32, u32)>,
    parity: u8,
    generation: u32,
    /// The first rung the player's picture-size limit allows (`share_quality`).
    top_rung: usize,
}

fn rung_for(budget: f64) -> usize {
    LADDER
        .iter()
        .position(|r| budget >= f64::from(r.min_kbps))
        .unwrap_or(LADDER.len())
}

impl RateController {
    /// Start at `cfg.start_kbps` (within the caps) at time `now_us`.
    pub fn new(cfg: RateConfig, path: PathKind, now_us: u64) -> RateController {
        let mut c = RateController {
            cfg,
            path,
            budget: f64::from(cfg.start_kbps),
            slow_start: true,
            last_bad_us: now_us,
            last_up_us: now_us,
            last_down_us: None,
            rung: 0,
            below_since: None,
            above_since: None,
            window: VecDeque::new(),
            parity: 0,
            generation: 0,
            top_rung: 0,
        };
        c.budget = c.budget.min(c.cap());
        c.rung = rung_for(c.budget);
        c
    }

    /// The tallest picture the player allows (`share_quality`); 0 lifts the limit. The ladder
    /// never goes above the first rung that fits, and this limit alone never pauses the video:
    /// a limit under 540 still gives 540p.
    pub fn set_max_height(&mut self, max_height: u32) {
        self.top_rung = if max_height == 0 {
            0
        } else {
            LADDER
                .iter()
                .position(|r| r.height <= max_height)
                .unwrap_or(LADDER.len() - 1)
        };
        if self.rung < self.top_rung {
            self.rung = self.top_rung;
            self.below_since = None;
            self.above_since = None;
            self.generation += 1;
        }
    }

    /// The path changed (direct to relayed or back).
    pub fn set_path(&mut self, path: PathKind) {
        self.path = path;
        self.budget = self.budget.min(self.cap());
    }

    /// The player's own maximum changed (`share_quality`).
    pub fn set_player_max(&mut self, kbps: u32) {
        self.cfg.player_max_kbps = kbps;
        self.budget = self.budget.min(self.cap());
    }

    fn cap(&self) -> f64 {
        let path = match self.path {
            PathKind::Relay => self.cfg.relay_max_kbps,
            PathKind::Direct => self.cfg.direct_max_kbps,
        };
        f64::from(self.cfg.player_max_kbps.min(path))
    }

    /// The target as it stands.
    pub fn target(&self) -> Target {
        let (kbps, width, height, fps, paused) = match LADDER.get(self.rung) {
            Some(r) => {
                let video = self.budget / (1.0 + f64::from(self.parity) / 100.0);
                (video as u32, r.width, r.height, r.fps, false)
            }
            None => {
                let last = LADDER[LADDER.len() - 1];
                (0, last.width, last.height, last.fps, true)
            }
        };
        Target {
            kbps,
            width,
            height,
            fps,
            parity_percent: self.parity,
            paused,
            generation: self.generation,
        }
    }

    /// The budget in kbit/s, video and parity together.
    pub fn budget_kbps(&self) -> u32 {
        self.budget as u32
    }

    /// Take in one report and return the new target.
    pub fn on_input(&mut self, input: &RateInput) -> Target {
        let now = input.now_us;
        let cfg = self.cfg;

        // Loss over the window, for parity, and over this report, for the rate.
        self.window.push_back((now, input.received, input.lost));
        while self
            .window
            .front()
            .is_some_and(|w| w.0 + cfg.parity_window_us < now)
        {
            self.window.pop_front();
        }
        let pct = |lost: u32, received: u32| {
            let total = u64::from(lost) + u64::from(received);
            if total == 0 {
                0.0
            } else {
                100.0 * f64::from(lost) / total as f64
            }
        };
        let report_loss = pct(input.lost, input.received);

        let congested = report_loss > cfg.loss_down_percent
            || input.delay_trend_us > cfg.trend_down_us
            || input.send_queue_frames > 1;
        let unsettled = congested
            || report_loss > cfg.loss_up_percent
            || input.delay_trend_us > cfg.trend_flat_us;

        if congested {
            self.last_bad_us = now;
            let due = self
                .last_down_us
                .is_none_or(|t| now >= t + cfg.down_interval_us);
            if due {
                let mut b = self.budget * cfg.down_factor;
                if let Some(d) = input.delivery_kbps.filter(|&d| d > 0) {
                    b = b.min(f64::from(d) * cfg.delivery_factor);
                }
                self.budget = b.max(f64::from(cfg.floor_kbps));
                self.slow_start = false;
                self.last_down_us = Some(now);
            }
        } else if unsettled {
            self.last_bad_us = now;
        } else if now >= self.last_bad_us.max(self.last_up_us) + cfg.up_wait_us {
            self.budget *= if self.slow_start {
                cfg.start_step
            } else {
                cfg.up_step
            };
            self.last_up_us = now;
        }
        self.budget = self.budget.min(self.cap());

        self.update_parity();
        self.update_rung(now);
        self.target()
    }

    fn update_parity(&mut self) {
        let (mut received, mut lost) = (0u64, 0u64);
        for w in &self.window {
            received += u64::from(w.1);
            lost += u64::from(w.2);
        }
        let total = received + lost;
        let loss = if total < 20 {
            0.0
        } else {
            100.0 * lost as f64 / total as f64
        };
        let step = f64::from(self.cfg.parity_step.max(1));
        let wanted = if loss < self.cfg.parity_off_percent {
            0
        } else {
            // Twice the loss, in whole steps, but at least one step.
            let w = ((2.0 * loss / step).ceil() * step).clamp(step, f64::from(self.cfg.parity_max));
            w as u8
        };
        self.parity = if wanted > self.parity {
            (self.parity + self.cfg.parity_step).min(wanted)
        } else {
            wanted
        };
    }

    fn update_rung(&mut self, now: u64) {
        let want = rung_for(self.budget).max(self.top_rung);
        let before = self.rung;
        if want > self.rung {
            // The budget is below the current rung.
            self.above_since = None;
            let since = *self.below_since.get_or_insert(now);
            if now >= since + self.cfg.rung_down_hold_us {
                self.rung = want;
            }
        } else if want < self.rung {
            // The budget reaches a higher rung; it must clear it by the margin for a while.
            self.below_since = None;
            let up = LADDER[self.rung - 1].min_kbps;
            if self.budget >= f64::from(up) * self.cfg.rung_up_margin - 1e-6 {
                let since = *self.above_since.get_or_insert(now);
                if now >= since + self.cfg.rung_up_hold_us {
                    self.rung -= 1;
                    self.above_since = None;
                }
            } else {
                self.above_since = None;
            }
        } else {
            self.below_since = None;
            self.above_since = None;
        }
        if self.rung != before {
            self.below_since = None;
            self.above_since = None;
            self.generation += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: u64 = 1000;

    fn quiet(now_us: u64) -> RateInput {
        RateInput {
            now_us,
            received: 50,
            lost: 0,
            delay_trend_us: 0,
            send_queue_frames: 0,
            delivery_kbps: None,
        }
    }

    fn run_quiet(c: &mut RateController, from_ms: u64, to_ms: u64) -> Target {
        let mut t = c.target();
        let mut ms = from_ms;
        while ms <= to_ms {
            t = c.on_input(&quiet(ms * MS));
            ms += 100;
        }
        t
    }

    #[test]
    fn starts_at_4000_and_720p30() {
        let c = RateController::new(RateConfig::default(), PathKind::Direct, 0);
        let t = c.target();
        assert_eq!((t.kbps, t.width, t.height, t.fps), (4000, 1280, 720, 30));
        assert_eq!((t.parity_percent, t.paused, t.generation), (0, false, 0));
    }

    #[test]
    fn rises_by_25_percent_a_second_at_the_start_then_10() {
        let mut c = RateController::new(RateConfig::default(), PathKind::Direct, 0);
        // nothing before a second of quiet
        for ms in (100..1000).step_by(100) {
            c.on_input(&quiet(ms * MS));
        }
        assert_eq!(c.budget_kbps(), 4000);
        c.on_input(&quiet(1000 * MS));
        assert_eq!(c.budget_kbps(), 5000);
        run_quiet(&mut c, 1100, 2000);
        assert_eq!(c.budget_kbps(), 6250);
        // a limit shows: down to 80%, then 10% steps
        let mut i = quiet(2100 * MS);
        i.lost = 5; // 9%
        c.on_input(&i);
        assert_eq!(c.budget_kbps(), 5000);
        run_quiet(&mut c, 2200, 3000);
        assert_eq!(
            c.budget_kbps(),
            5000,
            "a second must pass after the trouble"
        );
        run_quiet(&mut c, 3100, 3100);
        assert_eq!(c.budget_kbps(), 5500);
    }

    #[test]
    fn falls_at_once_but_only_every_300_ms() {
        let mut c = RateController::new(RateConfig::default(), PathKind::Direct, 0);
        let bad = |ms: u64| RateInput {
            lost: 10,
            ..quiet(ms * MS)
        };
        c.on_input(&bad(100));
        assert_eq!(c.budget_kbps(), 3200);
        c.on_input(&bad(200));
        c.on_input(&bad(300));
        assert_eq!(c.budget_kbps(), 3200, "once per 300 ms");
        c.on_input(&bad(400));
        assert_eq!(c.budget_kbps(), 2560);
    }

    #[test]
    fn each_trouble_signal_cuts() {
        let signals: [fn(&mut RateInput); 3] = [
            |i| i.lost = 3, // 3/53 = 5.7%
            |i| i.delay_trend_us = 4_001,
            |i| i.send_queue_frames = 2,
        ];
        for s in signals {
            let mut c = RateController::new(RateConfig::default(), PathKind::Direct, 0);
            let mut i = quiet(100 * MS);
            s(&mut i);
            c.on_input(&i);
            assert_eq!(c.budget_kbps(), 3200);
        }
        // exactly at the limits is not trouble
        let mut c = RateController::new(RateConfig::default(), PathKind::Direct, 0);
        let mut i = quiet(100 * MS);
        i.delay_trend_us = 4_000;
        i.send_queue_frames = 1;
        i.lost = 1; // 1/51 = 1.96%
        c.on_input(&i);
        assert_eq!(c.budget_kbps(), 4000);
    }

    #[test]
    fn the_delivery_rate_caps_the_cut() {
        let mut c = RateController::new(RateConfig::default(), PathKind::Direct, 0);
        let mut i = quiet(100 * MS);
        i.lost = 10;
        i.delivery_kbps = Some(2_000);
        c.on_input(&i);
        assert_eq!(c.budget_kbps(), 1700); // 0.85 x 2000 is below 0.8 x 4000
        // a delivery rate above the cut changes nothing
        let mut c = RateController::new(RateConfig::default(), PathKind::Direct, 0);
        let mut i = quiet(100 * MS);
        i.lost = 10;
        i.delivery_kbps = Some(9_000);
        c.on_input(&i);
        assert_eq!(c.budget_kbps(), 3200);
        // and without trouble it is ignored: a stream that sends less than it may is not a limit
        let mut c = RateController::new(RateConfig::default(), PathKind::Direct, 0);
        let mut i = quiet(1000 * MS);
        i.delivery_kbps = Some(1_000);
        c.on_input(&i);
        assert_eq!(c.budget_kbps(), 5000);
    }

    #[test]
    fn caps_for_relay_player_and_path() {
        let mut c = RateController::new(RateConfig::default(), PathKind::Relay, 0);
        run_quiet(&mut c, 100, 30_000);
        assert_eq!(c.budget_kbps(), 6000, "relay 6000");
        let t = c.target();
        assert_eq!(
            (t.width, t.height, t.fps),
            (1280, 720, 60),
            "relayed: up to 720p60"
        );

        let mut c = RateController::new(RateConfig::default(), PathKind::Direct, 0);
        run_quiet(&mut c, 100, 60_000);
        assert_eq!(c.budget_kbps(), 15_000, "player max 15000");
        assert_eq!(c.target().height, 1080);

        c.set_path(PathKind::Relay);
        assert_eq!(c.budget_kbps(), 6000);
        c.set_player_max(3_000);
        assert_eq!(c.budget_kbps(), 3000);

        let cfg = RateConfig {
            player_max_kbps: 40_000,
            ..RateConfig::default()
        };
        let mut c = RateController::new(cfg, PathKind::Direct, 0);
        run_quiet(&mut c, 100, 120_000);
        assert_eq!(c.budget_kbps(), 25_000, "direct 25000");
    }

    #[test]
    fn the_ladder_waits_one_second_down_and_eight_up() {
        let cfg = RateConfig {
            start_kbps: 10_000,
            ..RateConfig::default()
        };
        let mut c = RateController::new(cfg, PathKind::Direct, 0);
        assert_eq!(c.target().height, 1080);
        // trouble: 10000 -> 8000 (below the 1080p rung)
        let mut i = quiet(100 * MS);
        i.lost = 10;
        c.on_input(&i);
        assert_eq!(c.budget_kbps(), 8000);
        assert_eq!(c.target().height, 1080, "not yet");
        for ms in (200..1100).step_by(100) {
            c.on_input(&quiet(ms * MS));
        }
        assert_eq!(
            c.target().height,
            1080,
            "just under a second below the rung"
        );
        let t = c.on_input(&quiet(1100 * MS));
        assert_eq!((t.height, t.fps, t.generation), (720, 60, 1));

        // climbing back needs 1.2 x 10000 for 8 s
        let t = run_quiet(&mut c, 1200, 20_000);
        assert_eq!(c.budget_kbps(), 15_000);
        assert_eq!((t.height, t.fps), (1080, 60));
        assert_eq!(t.generation, 2);
    }

    #[test]
    fn a_short_dip_below_a_rung_does_not_move_it() {
        let cfg = RateConfig {
            start_kbps: 6_000,
            ..RateConfig::default()
        };
        let mut c = RateController::new(cfg, PathKind::Direct, 0);
        assert_eq!(c.target().height, 720);
        let mut i = quiet(100 * MS);
        i.lost = 10;
        c.on_input(&i); // 4800, below 5000
        assert_eq!(c.target().fps, 60);
        // back above within a second
        run_quiet(&mut c, 200, 1900);
        assert_eq!(c.target().fps, 60);
        assert_eq!(c.target().generation, 0);
    }

    #[test]
    fn pauses_below_1000_and_resumes() {
        let cfg = RateConfig {
            start_kbps: 1_100,
            ..RateConfig::default()
        };
        let mut c = RateController::new(cfg, PathKind::Direct, 0);
        assert_eq!(c.target().height, 540);
        let bad = |ms: u64| RateInput {
            lost: 20,
            ..quiet(ms * MS)
        };
        for ms in (100..=2000).step_by(100) {
            c.on_input(&bad(ms));
        }
        let t = c.target();
        assert!(t.paused);
        assert_eq!(t.kbps, 0);
        // the budget has no floor below 300 and no parity runs away
        assert!(c.budget_kbps() >= 300);
        // the link is fine again: the budget climbs, the picture returns after 8 s above 1200
        let t = run_quiet(&mut c, 2100, 40_000);
        assert!(!t.paused);
        assert!(t.generation >= 2);
    }

    #[test]
    fn parity_follows_loss_over_two_seconds() {
        let mut c = RateController::new(RateConfig::default(), PathKind::Direct, 0);
        // 1% loss: parity 5 after the first window fills
        let lossy = |ms: u64, lost: u32| RateInput {
            received: 100 - lost,
            lost,
            ..quiet(ms * MS)
        };
        let mut last = c.target();
        for ms in (100..=3000).step_by(100) {
            last = c.on_input(&lossy(ms, 1));
        }
        assert_eq!(last.parity_percent, 5);
        // 4% loss: 2x4 = 8, in steps of 5 -> 10 (the rate also falls, that is fine)
        for ms in (3100..=6000).step_by(100) {
            last = c.on_input(&lossy(ms, 4));
        }
        assert_eq!(last.parity_percent, 10);
        // very high loss: at most 30, rising 5 per report
        let first = c.on_input(&lossy(6100, 40));
        assert!(first.parity_percent <= 15);
        for ms in (6200..=9000).step_by(100) {
            last = c.on_input(&lossy(ms, 40));
        }
        assert_eq!(last.parity_percent, 30);
        // loss gone: back to 0 once the window is clean
        for ms in (9100..=11_300).step_by(100) {
            last = c.on_input(&quiet(ms * MS));
        }
        assert_eq!(last.parity_percent, 0);
    }

    #[test]
    fn the_video_rate_leaves_room_for_parity() {
        let mut c = RateController::new(RateConfig::default(), PathKind::Direct, 0);
        // 1.5% loss is not enough to cut the rate, but it turns parity on
        let lossy = |ms: u64| RateInput {
            received: 197,
            lost: 3,
            ..quiet(ms * MS)
        };
        let mut t = c.target();
        for ms in (100..=3000).step_by(100) {
            t = c.on_input(&lossy(ms));
        }
        assert_eq!(t.parity_percent, 5);
        assert_eq!(c.budget_kbps(), 4000);
        assert_eq!(t.kbps, 3809); // 4000 / 1.05
    }

    // ---- recorded-trace style: a bottleneck link that the controller has to find

    /// A bottleneck with a queue: what the guest would report for what is sent.
    struct Link {
        capacity_kbps: f64,
        queue_kbit: f64,
        /// Queue depth that drops packets, in milliseconds at the capacity.
        buffer_ms: f64,
    }

    impl Link {
        /// Feed 100 ms of `sent_kbps`; get the report inputs.
        fn step(&mut self, now_us: u64, sent_kbps: f64) -> RateInput {
            let packets = (sent_kbps * 0.1 / 9.6).max(1.0); // 1200-byte packets
            let queue_before = self.queue_kbit;
            self.queue_kbit += sent_kbps * 0.1 - self.capacity_kbps * 0.1;
            self.queue_kbit = self.queue_kbit.max(0.0);
            let max_queue = self.capacity_kbps * self.buffer_ms / 1000.0;
            let mut lost_share = 0.0;
            if self.queue_kbit > max_queue {
                let over = self.queue_kbit - max_queue;
                self.queue_kbit = max_queue;
                lost_share = (over / (sent_kbps * 0.1)).min(1.0);
            }
            let lost = (packets * lost_share).round() as u32;
            let trend = ((self.queue_kbit - queue_before) / self.capacity_kbps * 1e6) as i32;
            RateInput {
                now_us,
                received: (packets as u32).saturating_sub(lost),
                lost,
                delay_trend_us: trend,
                send_queue_frames: 0,
                delivery_kbps: Some(sent_kbps.min(self.capacity_kbps) as u32),
            }
        }
    }

    fn simulate(c: &mut RateController, link: &mut Link, from_s: u64, to_s: u64) -> Vec<Target> {
        let mut out = Vec::new();
        let mut ms = from_s * 1000;
        while ms < to_s * 1000 {
            ms += 100;
            let t = c.target();
            let sent = if t.paused {
                0.0
            } else {
                f64::from(t.kbps) * (1.0 + f64::from(t.parity_percent) / 100.0)
            };
            let input = link.step(ms * MS, sent);
            out.push(c.on_input(&input));
        }
        out
    }

    #[test]
    fn finds_a_bottleneck_and_stays_under_it() {
        let mut c = RateController::new(RateConfig::default(), PathKind::Direct, 0);
        let mut link = Link {
            capacity_kbps: 8_000.0,
            queue_kbit: 0.0,
            buffer_ms: 100.0,
        };
        let ts = simulate(&mut c, &mut link, 0, 60);
        // after a settling time the budget hovers around the capacity and never runs away
        let late = &ts[ts.len() / 2..];
        let max = late.iter().map(|t| t.kbps).max().unwrap();
        assert!(max <= 10_500, "ran away to {max}");
        let avg = late.iter().map(|t| f64::from(t.kbps)).sum::<f64>() / late.len() as f64;
        assert!(avg > 4_800.0, "stays too low: {avg}");
        // size and rate do not flap: few generations in a minute
        let gens = ts.last().unwrap().generation;
        assert!(gens <= 4, "{gens} changes");
    }

    #[test]
    fn follows_a_capacity_drop_and_recovery() {
        let mut c = RateController::new(RateConfig::default(), PathKind::Direct, 0);
        let mut link = Link {
            capacity_kbps: 12_000.0,
            queue_kbit: 0.0,
            buffer_ms: 100.0,
        };
        let a = simulate(&mut c, &mut link, 0, 40);
        let t = *a.last().unwrap();
        assert!(t.height >= 720 && !t.paused, "{t:?}");
        link.capacity_kbps = 1_800.0;
        let b = simulate(&mut c, &mut link, 40, 70);
        let t = *b.last().unwrap();
        assert_eq!((t.height, t.fps), (540, 30), "{t:?}");
        assert!(t.kbps <= 2_300, "{t:?}");
        link.capacity_kbps = 12_000.0;
        let d = simulate(&mut c, &mut link, 70, 200);
        let t = *d.last().unwrap();
        assert!(t.height >= 720 && t.kbps > 5_000, "{t:?}");
    }

    #[test]
    fn a_starved_link_pauses_the_video() {
        let mut c = RateController::new(RateConfig::default(), PathKind::Direct, 0);
        let mut link = Link {
            capacity_kbps: 600.0,
            queue_kbit: 0.0,
            buffer_ms: 100.0,
        };
        let ts = simulate(&mut c, &mut link, 0, 120);
        let paused = ts.iter().filter(|t| t.paused).count();
        assert!(
            paused * 10 > ts.len() * 7,
            "paused {paused} of {}",
            ts.len()
        );
    }

    #[test]
    fn a_relay_never_goes_above_720p60() {
        let mut c = RateController::new(RateConfig::default(), PathKind::Relay, 0);
        let mut link = Link {
            capacity_kbps: 50_000.0,
            queue_kbit: 0.0,
            buffer_ms: 100.0,
        };
        let ts = simulate(&mut c, &mut link, 0, 120);
        assert!(ts.iter().all(|t| t.height <= 720 && t.kbps <= 6_000));
        assert_eq!(ts.last().unwrap().fps, 60);
    }

    #[test]
    fn the_players_height_limit_holds_the_ladder_down_and_lifts_again() {
        let mut c = RateController::new(RateConfig::default(), PathKind::Direct, 0);
        let mut link = Link {
            capacity_kbps: 50_000.0,
            queue_kbit: 0.0,
            buffer_ms: 100.0,
        };
        let a = simulate(&mut c, &mut link, 0, 60);
        assert_eq!(a.last().unwrap().height, 1080);
        let before = a.last().unwrap().generation;
        c.set_max_height(720);
        let t = c.target();
        assert_eq!(t.height, 720, "the limit applies at once");
        assert!(t.generation > before, "a new epoch starts");
        let b = simulate(&mut c, &mut link, 60, 120);
        assert!(b.iter().all(|t| t.height <= 720), "stays under the limit");
        assert_eq!(b.last().unwrap().fps, 60);
        c.set_max_height(0);
        let d = simulate(&mut c, &mut link, 120, 200);
        assert_eq!(d.last().unwrap().height, 1080, "the limit is lifted");
    }

    #[test]
    fn a_height_limit_under_the_last_rung_gives_the_last_rung_not_a_pause() {
        let mut c = RateController::new(RateConfig::default(), PathKind::Direct, 0);
        c.set_max_height(240);
        let t = c.target();
        assert_eq!((t.height, t.paused), (540, false));
    }
}
