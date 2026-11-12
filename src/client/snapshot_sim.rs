//! Deterministic link simulation of the snapshots coming down (`server/link_sim.rs` covers the
//! commands going up).
//!
//! Runs the real client code (`RemoteClock`, the player's `History`, `pose_at`: Hermite, held
//! ticks, capped dead reckoning) against a stream model (in order; a loss is a head-of-line stall
//! of about one RTT) and a datagram model (holes, reorder), while simulated players bhop, jump and
//! reverse. The question is the visible one: does a drawn player ever stop while the real one was
//! moving?
//!
//! Every tick of a burst lands in the history, so a stall shorter than the interpolation delay is
//! backfilled before playback gets there. [`Delivery::Component`] is the alternative, for
//! comparison: a replicated component ends at the newest value, and the stall plays as a gap.
//!
//! Metrics (the bhopper and the jumper assert; the reversing player reports apart, because a
//! reversal inside a gap is a slow-down no algorithm can avoid):
//! - `hitches`: frames where the render clock advanced under 75% of real time. The main one.
//! - `max_lag`: closest approach to the extrapolation cap (1.0 = a hitch).
//! - `stalled`/`freezes`: drawn motion under 15% of the true speed, and runs of 3+ such frames.
//! - `max_step`: the biggest one-frame step (a snap shows here).
//! - `err`: drawn position against the truth at the drawn server time.
//! - `gaps`: fixed ticks drawn between two samples more than a tick apart.
//! - `xtrap`/`capped`/`snaps`: the clock's own counts.
//!
//! `snapshot_sim_unreliable_frames` compares an ordered stream with unreliable, unordered frames,
//! on lag compensation too: at every fixed tick, the history knots the step samples at the seen
//! time (three ticks around it) must be held by the time the server judges that command.
//! - `miss%`: knots the client doesn't hold.

use bevy::math::Vec3;

use super::{RemoteClock, RemoteClockStats};
use crate::{
    history::History,
    poses::{REMOTE_EXTRAPOLATION_SECONDS, RemotePose, pose_at},
    protocol::{PlayerSnapshot, TickRate},
};

const RATE: TickRate = TickRate::new(20);
const TICK_HZ: f64 = RATE.hz() as f64;
const RENDER_HZ: f64 = 60.0;
const SIM_SECONDS: f64 = 240.0;
const WARMUP_SECONDS: f64 = 3.0;
/// Stalled frames in a row that count as a freeze.
const FREEZE_FRAMES: u32 = 3;
/// Rendered displacement below this fraction of true displacement = stalled.
const STALL_FRACTION: f64 = 0.15;

struct Rng(u64);

impl Rng {
    fn next_f64(&mut self) -> f64 {
        // xorshift64*
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        (self.0.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 11) as f64 / (1u64 << 53) as f64
    }

    fn chance(&mut self, p: f64) -> bool {
        self.next_f64() < p
    }
}

#[derive(Clone, Copy)]
enum Link {
    /// WebSocket: never lossy or reordered at the app layer; a wire loss
    /// head-of-line stalls delivery for ~one RTT (retransmit), then the
    /// backlog arrives as one burst.
    Stream,
    /// UDP-like: a loss is a hole, jitter reorders freely.
    Datagram,
}

/// How a burst of server ticks reaches the interpolation buffer.
#[derive(Clone, Copy)]
enum Delivery {
    /// What the library does: the history is a `need_history` receive marker, so every tick
    /// that landed this frame is recorded.
    Messages,
    /// A replicated component: replicon applies every update that landed
    /// this frame, the component ends at the NEWEST value and `Changed`
    /// fires once, so the buffer gets exactly one sample.
    Component,
}

#[derive(Clone, Copy)]
struct Cfg {
    label: &'static str,
    link: Link,
    delivery: Delivery,
    one_way_ms: f64,
    jitter_ms: f64,
    loss: f64,
    /// The interpolation delay (ticks); `None`: the library's.
    delay_ticks: Option<u64>,
    /// Force one stream retransmit in the middle of the run.
    forced_single: bool,
    /// Server ticks force-lost on top of random loss — deterministic stall
    /// events (single retransmit, a full outage).
    forced: &'static [(u64, u64)],
    /// Mean length of a loss burst in packets (Gilbert-Elliott); 1 = each
    /// packet lost independently.
    burst: f64,
}

#[derive(Clone, Copy)]
enum Motion {
    /// 6 m/s run + ~9 m/s sine strafe + 0.8 s hop cycle: the bhopper.
    BhopStrafe,
    /// 3 m/s walk + constant jump spam.
    JumpSpam,
    /// Hard 180s every 0.7 s: worst case for gap lerp and dead-reckoning.
    Reversal,
}

/// Piecewise hop arc that lands exactly at `period`: v0 = g*period/2.
fn hop(t: f64, period: f64, v0: f64) -> (f64, f64) {
    let g = 2.0 * v0 / period;
    let tau = t.rem_euclid(period);
    (v0 * tau - 0.5 * g * tau * tau, v0 - g * tau)
}

/// Analytic ground truth on the server timeline: (position, velocity).
fn truth(motion: Motion, t: f64) -> (Vec3, Vec3) {
    match motion {
        Motion::BhopStrafe => {
            let (y, vy) = hop(t, 0.8, 8.0);
            let z = 3.0 * (std::f64::consts::PI * t).sin();
            let vz = 3.0 * std::f64::consts::PI * (std::f64::consts::PI * t).cos();
            (
                Vec3::new((6.0 * t) as f32, y as f32, z as f32),
                Vec3::new(6.0, vy as f32, vz as f32),
            )
        }
        Motion::JumpSpam => {
            let (y, vy) = hop(t, 0.7, 7.0);
            (
                Vec3::new((3.0 * t) as f32, y as f32, 0.0),
                Vec3::new(3.0, vy as f32, 0.0),
            )
        }
        Motion::Reversal => {
            let p = t.rem_euclid(1.4);
            let (x, vx) = if p < 0.7 {
                (6.0 * p, 6.0)
            } else {
                (6.0 * 0.7 - 6.0 * (p - 0.7), -6.0)
            };
            (
                Vec3::new(x as f32, 0.0, 0.0),
                Vec3::new(vx as f32, 0.0, 0.0),
            )
        }
    }
}

fn hermite_render_position(previous: RemotePose, current: RemotePose, t: f32) -> Vec3 {
    let t2 = t * t;
    let t3 = t2 * t;
    let dt = 1.0 / TICK_HZ as f32;
    (2.0 * t3 - 3.0 * t2 + 1.0) * previous.position
        + (t3 - 2.0 * t2 + t) * dt * previous.velocity
        + (-2.0 * t3 + 3.0 * t2) * current.position
        + (t3 - t2) * dt * current.velocity
}

#[derive(Default, Clone, Copy)]
struct Report {
    /// Frames where the render clock advanced under 75% of real time: a one-frame hitch on every
    /// remote at once, and the only kind (position is otherwise continuous).
    hitches: u32,
    /// The hitches that came with the clock at the extrapolation cap: the
    /// hole outlasted delay + extrapolation, and the clock held by design.
    held: u32,
    /// Worst single-frame clock advance as a fraction of real time (1.0 =
    /// nominal speed, 0.0 = fully parked).
    min_advance: f64,
    /// Closest approach to the extrapolation cap, as a fraction of its allowance (1.0 = cap
    /// contact = a hitch): the margin the link leaves.
    max_lag: f64,
    stalled: u32,
    freezes: u32,
    max_run: u32,
    rev_max_run: u32,
    jumps: u32,
    max_step: f32,
    err_p95: f32,
    err_max: f32,
    gaps: u32,
    extrapolated: u32,
    capped: u32,
    snaps: u32,
    knots: u32,
    missing: u32,
}

impl Report {
    fn percent(&self, count: u32) -> f64 {
        100.0 * f64::from(count) / f64::from(self.knots.max(1))
    }
}

fn note_run(report: &mut Report, run: u32, reversal: bool) {
    if reversal {
        report.rev_max_run = report.rev_max_run.max(run);
    } else {
        report.max_run = report.max_run.max(run);
        if run >= FREEZE_FRAMES {
            report.freezes += 1;
        }
    }
}

fn simulate(cfg: Cfg, seed: u64) -> Report {
    let arrivals = wire_arrivals(&cfg, &mut Rng(seed));
    client_through(&cfg, &arrivals)
}

/// When each server tick's packet lands, in arrival order: one packet a tick, carrying every
/// player, over `cfg`'s link.
fn wire_arrivals(cfg: &Cfg, rng: &mut Rng) -> Vec<(f64, u64)> {
    let rtt_s = 2.0 * cfg.one_way_ms / 1000.0;
    let total_ticks = (SIM_SECONDS * TICK_HZ) as u64;
    let mut arrivals: Vec<(f64, u64)> = Vec::with_capacity(total_ticks as usize);
    let mut last_arrival = 0.0_f64;
    let mut in_burst = false;
    for tick in 1..=total_ticks {
        let send = tick as f64 / TICK_HZ;
        let lost = if cfg.burst > 1.0 {
            in_burst = if in_burst {
                !rng.chance(1.0 / cfg.burst)
            } else {
                rng.chance(cfg.loss / (cfg.burst * (1.0 - cfg.loss)))
            };
            in_burst
        } else {
            rng.chance(cfg.loss)
        } || (cfg.forced_single && tick == total_ticks / 2);
        // A forced range models the link being DOWN: nothing (including
        // retransmits) lands until the range ends, then the backlog bursts.
        let outage_until = cfg
            .forced
            .iter()
            .find(|&&(a, b)| tick >= a && tick <= b)
            .map(|&(_, b)| b as f64 / TICK_HZ);
        let jitter = rng.next_f64() * cfg.jitter_ms / 1000.0;
        match cfg.link {
            Link::Stream => {
                let mut arrival = send + cfg.one_way_ms / 1000.0 + jitter;
                if lost {
                    // Retransmit: this packet, and everything behind it, lands 1.5-2x RTT late
                    // (loss detection plus the return trip), a pessimistic recovery.
                    arrival += rtt_s * (1.5 + 0.5 * rng.next_f64());
                }
                if let Some(end) = outage_until {
                    arrival = arrival.max(end + cfg.one_way_ms / 1000.0 + rtt_s);
                }
                arrival = arrival.max(last_arrival);
                last_arrival = arrival;
                arrivals.push((arrival, tick));
            }
            Link::Datagram => {
                if lost || outage_until.is_some() {
                    continue;
                }
                arrivals.push((send + cfg.one_way_ms / 1000.0 + jitter, tick));
            }
        }
    }
    arrivals.sort_by(|a, b| a.0.total_cmp(&b.0));
    arrivals
}

/// The real client's remote clock and pose histories, fed `arrivals`: what it drew of each
/// player.
fn client_through(cfg: &Cfg, arrivals: &[(f64, u64)]) -> Report {
    let motions = [Motion::BhopStrafe, Motion::JumpSpam, Motion::Reversal];
    let mut clock = RemoteClock::new(RATE);
    if let Some(ticks) = cfg.delay_ticks {
        clock.interpolation_delay_seconds = ticks as f64 / TICK_HZ;
    }
    // One history per player: what remote rendering and lag compensation
    // both sample. The bhopper's (index 0) is checked for parity.
    let mut buffers: Vec<History<PlayerSnapshot>> =
        motions.iter().map(|_| History::new(RATE)).collect();
    // The ticks that arrived: a history holds every tick, so what the wire
    // delivered is kept here, for the knots and gaps metrics.
    let mut arrived = std::collections::BTreeSet::new();
    let mut applied = 0u64;
    let mut last_pos: [Option<Vec3>; 3] = [None; 3];
    let mut runs = [0u32; 3];
    let mut report = Report {
        min_advance: 1.0,
        ..Report::default()
    };
    let mut errs: Vec<f32> = Vec::new();
    let mut prev_render_secs: Option<f64> = None;
    let mut held_last_frame = false;
    let mut endpoints: [Option<(f64, f64, RemotePose, RemotePose)>; 3] = [None; 3];

    let dt = 1.0 / RENDER_HZ;
    let frames = (SIM_SECONDS * RENDER_HZ) as u64;
    let mut next_arrival = 0;
    let mut stats_reset = false;
    for frame in 1..=frames {
        let now = frame as f64 * dt;
        // While the buffer first fills the clock rightly runs near the head: the clock's counts
        // start with the metric window.
        if !stats_reset && now >= WARMUP_SECONDS {
            clock.stats = RemoteClockStats::default();
            stats_reset = true;
        }

        // PreUpdate: every tick that landed this frame.
        let mut landed = Vec::new();
        while next_arrival < arrivals.len() && arrivals[next_arrival].0 <= now {
            landed.push(arrivals[next_arrival].1);
            next_arrival += 1;
        }
        if let Delivery::Component = cfg.delivery {
            landed = landed
                .into_iter()
                .max()
                .filter(|&newest| newest > applied)
                .into_iter()
                .collect();
            applied = applied.max(landed.first().copied().unwrap_or(0));
        }
        for &tick in &landed {
            clock.observe_server_tick(tick);
            arrived.insert(tick);
            for (buffer, motion) in buffers.iter_mut().zip(motions) {
                let (position, _) = truth(motion, tick as f64 / TICK_HZ);
                let sample = PlayerSnapshot {
                    server_tick: tick,
                    position,
                    ..Default::default()
                };
                buffer.record(tick, sample);
            }
        }
        // The clock advances once per fixed tick; the render frames between interpolate its
        // endpoints.
        if frame % (RENDER_HZ as u64 / TICK_HZ as u64) == 0 {
            clock.advance_fixed();
            let Some(fixed_time) = clock.target_time() else {
                continue;
            };
            let ticks = fixed_time.as_ticks_f64();
            // The command built now is judged when it reaches the server,
            // a tick of input lead after the one-way trip, against what the
            // server's history holds by then.
            let judged = ((now + cfg.one_way_ms / 1000.0) * TICK_HZ) as u64 + 1;
            if now >= WARMUP_SECONDS {
                let seen = ticks as u64;
                for knot in seen.saturating_sub(1)..=(seen + 1).min(judged) {
                    report.knots += 1;
                    if !arrived.contains(&knot) {
                        report.missing += 1;
                    }
                }
            }
            if now >= WARMUP_SECONDS
                && let Some(&after) = arrived.range(ticks.floor() as u64 + 1..).next()
                && let Some(&before) = arrived.range(..after).next_back()
                && after - before > 1
            {
                report.gaps += 1;
            }
            for index in 0..motions.len() {
                let Some(sample) = pose_at(&buffers[index], fixed_time, RATE) else {
                    continue;
                };
                endpoints[index] = Some(match endpoints[index] {
                    Some((_, current_ticks, _, current)) => {
                        (current_ticks, fixed_time.as_ticks_f64(), current, sample)
                    }
                    None => (
                        fixed_time.as_ticks_f64(),
                        fixed_time.as_ticks_f64(),
                        sample,
                        sample,
                    ),
                });
            }
        }
        let Some(target) = clock.target_time() else {
            continue;
        };
        let frames_per_tick = RENDER_HZ / TICK_HZ;
        let alpha = (frame % frames_per_tick as u64) as f32 / frames_per_tick as f32;
        let render_ticks = endpoints[0]
            .map(|(previous, current, _, _)| previous + (current - previous) * f64::from(alpha))
            .unwrap_or(target.as_ticks_f64());
        let render_secs = render_ticks / TICK_HZ;
        if now >= WARMUP_SECONDS
            && let Some(previous) = prev_render_secs
        {
            let advance = (render_secs - previous) / dt;
            report.min_advance = report.min_advance.min(advance);
            let latest_renderable = (clock.latest_server_tick as f64 / TICK_HZ
                - clock.interpolation_delay_seconds)
                .max(0.0);
            let lag = (render_secs - latest_renderable) / f64::from(REMOTE_EXTRAPOLATION_SECONDS);
            // A hold ends on the frame the head lands again, while the
            // fixed-tick endpoint is still frozen: still the same hold.
            let at_cap = advance < 0.75 && (lag >= 0.95 || held_last_frame);
            held_last_frame = at_cap;
            if advance < 0.75 {
                report.hitches += 1;
                report.held += u32::from(at_cap);
            }
            report.max_lag = report.max_lag.max(lag);
        }
        prev_render_secs = Some(render_secs);

        for (index, motion) in motions.iter().enumerate() {
            let Some((_, _, previous_endpoint, current_endpoint)) = endpoints[index] else {
                continue;
            };
            let sample_position =
                hermite_render_position(previous_endpoint, current_endpoint, alpha);
            let previous = last_pos[index].replace(sample_position);
            if now < WARMUP_SECONDS {
                continue;
            }
            let reversal = matches!(motion, Motion::Reversal);
            let (true_position, true_velocity) = truth(*motion, render_secs);
            if !reversal {
                errs.push(sample_position.distance(true_position));
            }
            let Some(previous) = previous else {
                continue;
            };
            let step = sample_position.distance(previous);
            // Stalled: the drawn player moved far slower than the real one at the drawn time.
            let expected = f64::from(true_velocity.length()) * dt;
            if f64::from(step) < STALL_FRACTION * expected {
                runs[index] += 1;
                if !reversal {
                    report.stalled += 1;
                }
            } else {
                note_run(&mut report, runs[index], reversal);
                runs[index] = 0;
            }
            if !reversal {
                report.max_step = report.max_step.max(step);
                if step > 1.0 {
                    report.jumps += 1;
                }
            }
        }
    }
    for (index, motion) in motions.iter().enumerate() {
        note_run(&mut report, runs[index], matches!(motion, Motion::Reversal));
    }

    report.extrapolated = clock.stats.extrapolated_frames;
    report.capped = clock.stats.capped_frames;
    report.snaps = clock.stats.snaps;
    errs.sort_unstable_by(f32::total_cmp);
    report.err_p95 = errs[errs.len() * 95 / 100];
    report.err_max = *errs.last().unwrap();
    report
}

fn merge(reports: impl IntoIterator<Item = Report>) -> Report {
    let mut merged = Report {
        min_advance: 1.0,
        ..Report::default()
    };
    for report in reports {
        merged.hitches += report.hitches;
        merged.held += report.held;
        merged.min_advance = merged.min_advance.min(report.min_advance);
        merged.max_lag = merged.max_lag.max(report.max_lag);
        merged.stalled += report.stalled;
        merged.freezes += report.freezes;
        merged.max_run = merged.max_run.max(report.max_run);
        merged.rev_max_run = merged.rev_max_run.max(report.rev_max_run);
        merged.jumps += report.jumps;
        merged.max_step = merged.max_step.max(report.max_step);
        merged.err_p95 = merged.err_p95.max(report.err_p95);
        merged.err_max = merged.err_max.max(report.err_max);
        merged.gaps += report.gaps;
        merged.extrapolated += report.extrapolated;
        merged.capped += report.capped;
        merged.snaps += report.snaps;
        merged.knots += report.knots;
        merged.missing += report.missing;
    }
    merged
}

/// Every scenario over the same three seeds, scenarios in parallel.
fn run(cfgs: &[Cfg]) -> Vec<Report> {
    std::thread::scope(|scope| {
        let runs: Vec<_> = cfgs
            .iter()
            .map(|&cfg| {
                scope.spawn(move || merge([7, 1337, 0xDEAD_BEEF].map(|seed| simulate(cfg, seed))))
            })
            .collect();
        runs.into_iter().map(|run| run.join().unwrap()).collect()
    })
}

fn ws(label: &'static str, one_way_ms: f64, jitter_ms: f64, loss: f64) -> Cfg {
    Cfg {
        label,
        link: Link::Stream,
        delivery: Delivery::Messages,
        one_way_ms,
        jitter_ms,
        loss,
        delay_ticks: None,
        forced_single: false,
        forced: &[],
        burst: 1.0,
    }
}

#[test]
fn snapshot_sim_bad_link_profiles() {
    let cfgs = [
        ws("ws clean rtt=50        ", 25.0, 5.0, 0.0),
        ws("ws rtt=50 j=20 1%      ", 25.0, 20.0, 0.01),
        ws("ws rtt=100 j=30 1%     ", 50.0, 30.0, 0.01),
        ws("ws rtt=150 j=40 1%     ", 75.0, 40.0, 0.01),
        ws("ws rtt=200 j=60 1%     ", 100.0, 60.0, 0.01),
        ws("ws rtt=200 j=60 3%     ", 100.0, 60.0, 0.03),
        // Does the interpolation delay absorb one retransmit stall?
        Cfg {
            label: "ws single loss rtt=100 ",
            forced_single: true,
            ..ws("", 50.0, 20.0, 0.0)
        },
        Cfg {
            label: "ws single loss rtt=200 ",
            forced_single: true,
            ..ws("", 100.0, 20.0, 0.0)
        },
        // A ~1 s outage: hold at the cap, then snap on recovery, never crawl.
        Cfg {
            label: "ws outage ~1s rtt=200  ",
            forced: &[(600, 618)],
            ..ws("", 100.0, 20.0, 0.0)
        },
        Cfg {
            label: "dgram rtt=100 j=30 2%  ",
            link: Link::Datagram,
            ..ws("", 50.0, 30.0, 0.02)
        },
        Cfg {
            label: "ws delay=3 rtt=200   ",
            delay_ticks: Some(3),
            ..ws("", 100.0, 30.0, 0.01)
        },
        Cfg {
            label: "dgram delay=3 5%     ",
            link: Link::Datagram,
            delay_ticks: Some(3),
            ..ws("", 50.0, 40.0, 0.05)
        },
        // One retransmit stall that fits inside the interpolation delay, then the same stall had
        // poses been a replicated component.
        Cfg {
            label: "ws single loss rtt=50  ",
            forced_single: true,
            ..ws("", 25.0, 10.0, 0.0)
        },
        Cfg {
            label: "  ...as a component    ",
            forced_single: true,
            delivery: Delivery::Component,
            ..ws("", 25.0, 10.0, 0.0)
        },
    ];

    println!(
        "{:<24} {:>7} {:>7} {:>7} {:>7} {:>7} {:>8} {:>6} {:>8} {:>8} {:>8} {:>6} {:>6} {:>6} {:>6}",
        "scenario",
        "hitches",
        "min_adv",
        "max_lag",
        "freezes",
        "max_run",
        "rev_run",
        "jumps",
        "max_step",
        "err_p95",
        "err_max",
        "gaps",
        "xtrap",
        "capped",
        "snaps"
    );
    let results = run(&cfgs);
    for (cfg, merged) in cfgs.iter().zip(&results) {
        println!(
            "{:<24} {:>7} {:>7.2} {:>7.2} {:>7} {:>7} {:>8} {:>6} {:>8.3} {:>8.3} {:>8.3} {:>6} {:>6} {:>6} {:>6}",
            cfg.label,
            merged.hitches,
            merged.min_advance,
            merged.max_lag,
            merged.freezes,
            merged.max_run,
            merged.rev_max_run,
            merged.jumps,
            merged.max_step,
            merged.err_p95,
            merged.err_max,
            merged.gaps,
            merged.extrapolated,
            merged.capped,
            merged.snaps
        );
    }

    let [
        clean,
        ws50,
        ws100,
        _ws150,
        ws200,
        _ws200_3,
        single100,
        _single200,
        outage,
        dgram,
        _delay3_ws,
        delay3_dgram,
        backfill,
        coalesced,
    ] = &results[..]
    else {
        unreachable!()
    };

    // A clean link advances exactly one server tick per fixed tick: no
    // freezes, jumps, cap contact, or clock resets.
    assert_eq!(clean.stalled, 0, "clean link must never stall a frame");
    assert_eq!(clean.jumps + clean.snaps, 0);
    assert_eq!(clean.capped, 0);
    assert!((clean.min_advance - 1.0).abs() < 1e-6);

    // The delay plus extrapolation covers ordinary links, a 100 ms RTT retransmit and datagram
    // holes without parking the clock.
    for (label, report) in [
        ("ws50", ws50),
        ("single100", single100),
        ("dgram", dgram),
        ("delay3_dgram", delay3_dgram),
    ] {
        assert_eq!(report.hitches, 0, "{label}: presentation hitched");
        assert_eq!(report.capped, 0, "{label}: clock hit extrapolation cap");
    }
    assert_eq!(ws100.hitches, 0, "ws100: presentation hitched");
    assert_eq!(ws100.capped, 0);

    // It covers 200 ms RTT stream loss too; only a full outage may reach the cap.
    assert_eq!(
        ws200.capped, 0,
        "ws200: 450 ms budget should cover retransmit stalls"
    );
    assert!(outage.capped > 0, "outage should park the clock at the cap");
    assert!(outage.hitches > 0, "the capped playback clock must hold");
    assert!(outage.jumps > 0, "recovery jumps instead of crawling");

    // A stall inside the delay is invisible: the burst backfills the history before playback
    // gets there. A component would play it as a gap.
    assert_eq!(backfill.gaps, 0, "messages backfill the stall");
    assert_eq!(
        backfill.extrapolated, 0,
        "the stall fits in the interp delay"
    );
    assert!(
        coalesced.gaps > 0,
        "a component coalesces the burst into a gap"
    );
}

#[test]
fn snapshot_sim_unreliable_frames() {
    // `Ordered`: a lost packet is resent and holds back everything behind
    // it, the stream model.
    let ordered = |jitter_ms, loss| ws("", 50.0, jitter_ms, loss);
    // Unreliable: a lost frame is lost, the rest land as they come.
    let unreliable = |jitter_ms, loss| Cfg {
        link: Link::Datagram,
        ..ordered(jitter_ms, loss)
    };
    let mut rows = Vec::new();
    for loss in [0.01, 0.02, 0.05] {
        for jitter in [30.0, 60.0] {
            rows.push((
                format!("ordered j={jitter} {}%", loss * 100.0),
                ordered(jitter, loss),
            ));
            rows.push(("  unreliable".to_owned(), unreliable(jitter, loss)));
        }
    }
    // Loss in bursts of 100 ms on average, what Wi-Fi and congested queues
    // do. Counted in packets, so scaled to the tick rate.
    let bursty = |cfg: Cfg| Cfg {
        burst: 0.1 * TICK_HZ,
        ..cfg
    };
    rows.push((
        "ordered j=40 2% bursts".to_owned(),
        bursty(ordered(40.0, 0.02)),
    ));
    rows.push(("  unreliable".to_owned(), bursty(unreliable(40.0, 0.02))));
    let cfgs: Vec<Cfg> = rows.iter().map(|(_, cfg)| *cfg).collect();

    println!(
        "{:<26} {:>7} {:>6} {:>6} {:>8} {:>8} {:>7}",
        "scenario", "hitches", "gaps", "xtrap", "err_p95", "err_max", "miss%"
    );
    let results = run(&cfgs);
    for ((label, _), report) in rows.iter().zip(&results) {
        println!(
            "{:<26} {:>7} {:>6} {:>6} {:>8.3} {:>8.3} {:>7.2}",
            label,
            report.hitches,
            report.gaps,
            report.extrapolated,
            report.err_p95,
            report.err_max,
            report.percent(report.missing),
        );
    }

    // On the same loss, dropping beats resending: fewer knots missing when the step needs them,
    // and no more dead reckoning, since nothing waits behind a lost frame. A burst longer than
    // the delay plus extrapolation is an outage: the clock may hitch only while held at the cap.
    for pair in results.chunks(2) {
        let (ordered, unreliable) = (&pair[0], &pair[1]);
        assert_eq!(unreliable.hitches, unreliable.held, "hitched off the cap");
        assert!(
            unreliable.missing < ordered.missing,
            "missing {} vs ordered {}",
            unreliable.missing,
            ordered.missing
        );
        assert!(unreliable.extrapolated <= ordered.extrapolated);
    }
}
