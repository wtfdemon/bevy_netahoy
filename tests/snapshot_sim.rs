//! Deterministic link-condition simulation of the DOWNSTREAM snapshot
//! pipeline — the sibling of `link_sim.rs` (which covers usercmds going up).
//!
//! Runs the real client-side smoothing code — the disciplined `ClientServerClock` +
//! `RemoteInterpolationBuffer` + `sample_buffer_at` (Hermite,
//! gap lerp, capped dead-reckoning) — against a wire model of the WebSocket
//! transport (in-order; a loss becomes a head-of-line stall of ~one RTT) and
//! a datagram model (holes + reorder), while simulated players bhop, jump,
//! and reverse direction.
//!
//! One replicon reality the model reproduces: replicated components coalesce.
//! However many snapshot messages land in one client frame, the component
//! ends at the newest value and `Changed<PlayerSnapshot>` fires once — a
//! post-stall burst inserts ONE sample, and the interp buffer spans the stall
//! as a gap. The de-jitter question is therefore exactly the user-visible
//! one: does the rendered capsule ever stop while the real player was moving?
//!
//! Metrics per scenario (bhop + jumper assert; the direction-reversal player
//! reports separately because a reversal inside a coalesced gap renders as a
//! genuine slow-down no algorithm could avoid — the data never crossed the
//! wire):
//! - `hitches`: frames where the render clock advanced under 75% of real
//!   time. Quake's normal 1-2ms offset nudges stay above that at 144Hz; fast
//!   correction and cap contact can cross it. The primary metric.
//! - `max_lag`: closest approach to the cap (1.0 = a hitch) — the margin
//!   the link profile leaves before hitches become possible.
//! - `stalled`/`freezes`: position-level backstop — rendered motion under
//!   15% of the player's true speed, and runs of 3+ such frames.
//! - `max_step`: biggest single-frame position step (a snap prints here).
//! - `err`: rendered position vs ground truth at the rendered server time.
//! - `xtrap`/`capped`/`snaps`: the clock's own diagnostics.

use bevy::math::Vec3;
use bevy_netahoy::client::{ClientServerClock, RemoteClockStats, RemoteInterpolationBuffer};
use bevy_netahoy::math::{REMOTE_EXTRAPOLATION_SECONDS, RemoteSnapshotSample};
use bevy_netahoy::protocol::FIXED_TIMESTEP_HZ;

const RENDER_HZ: f64 = 60.0;
const SIM_SECONDS: f64 = 240.0;
const WARMUP_SECONDS: f64 = 3.0;
/// "Froze for a couple of frames": 3 frames = 50 ms at 60 Hz.
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

#[derive(Clone, Copy)]
struct Cfg {
    label: &'static str,
    link: Link,
    one_way_ms: f64,
    jitter_ms: f64,
    loss: f64,
    /// Override the interp delay (ticks); `None` = the library default (3).
    delay_ticks: Option<u64>,
    /// Force one stream retransmit in the middle of the run.
    forced_single: bool,
    /// Server ticks force-lost on top of random loss — deterministic stall
    /// events (single retransmit, a full outage).
    forced: &'static [(u64, u64)],
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
            (Vec3::new(x as f32, 0.0, 0.0), Vec3::new(vx as f32, 0.0, 0.0))
        }
    }
}

fn hermite_render_position(
    previous: RemoteSnapshotSample,
    current: RemoteSnapshotSample,
    t: f32,
) -> Vec3 {
    let t2 = t * t;
    let t3 = t2 * t;
    let dt = 1.0 / FIXED_TIMESTEP_HZ as f32;
    (2.0 * t3 - 3.0 * t2 + 1.0) * previous.position
        + (t3 - 2.0 * t2 + t) * dt * previous.velocity
        + (-2.0 * t3 + 3.0 * t2) * current.position
        + (t3 - t2) * dt * current.velocity
}

#[derive(Default, Clone, Copy)]
struct Report {
    /// Frames where the render clock advanced under 75% of real time. Since
    /// normal playback stays near 1x, anything below it is cap contact: a
    /// single-frame presentation hitch on EVERY remote player at once (the
    /// exact "player froze for a frame" artifact; position is otherwise
    /// continuous, so clock hitches are the only hitch source).
    hitches: u32,
    /// Worst single-frame clock advance as a fraction of real time (1.0 =
    /// nominal speed, 0.0 = fully parked).
    min_advance: f64,
    /// Closest approach to the extrapolation cap, as a fraction of the cap
    /// allowance (render lag past `latest_renderable` / 0.25s). 1.0 = cap
    /// contact = a visible hitch; the gap to 1.0 is the whole safety margin
    /// the link profile leaves.
    max_lag: f64,
    stalled: u32,
    freezes: u32,
    max_run: u32,
    rev_max_run: u32,
    jumps: u32,
    max_step: f32,
    err_p95: f32,
    err_max: f32,
    extrapolated: u32,
    capped: u32,
    snaps: u32,
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
    let mut rng = Rng(seed);
    let rtt_s = 2.0 * cfg.one_way_ms / 1000.0;

    // --- the wire: one packet per server tick, carrying every player ---
    let total_ticks = (SIM_SECONDS * FIXED_TIMESTEP_HZ) as u64;
    let mut arrivals: Vec<(f64, u64)> = Vec::with_capacity(total_ticks as usize);
    let mut last_arrival = 0.0_f64;
    for tick in 1..=total_ticks {
        let send = tick as f64 / FIXED_TIMESTEP_HZ;
        let lost = rng.chance(cfg.loss) || (cfg.forced_single && tick == total_ticks / 2);
        // A forced range models the link being DOWN: nothing (including
        // retransmits) lands until the range ends, then the backlog bursts.
        let outage_until = cfg
            .forced
            .iter()
            .find(|&&(a, b)| tick >= a && tick <= b)
            .map(|&(_, b)| b as f64 / FIXED_TIMESTEP_HZ);
        let jitter = rng.next_f64() * cfg.jitter_ms / 1000.0;
        match cfg.link {
            Link::Stream => {
                let mut arrival = send + cfg.one_way_ms / 1000.0 + jitter;
                if lost {
                    // Retransmit: this packet (and everything queued behind
                    // it, via in-order delivery below) lands 1.5-2x RTT late
                    // — dup-ack/RACK detection plus the return trip, the same
                    // stall assumption LinkLossModel::Stream documents and
                    // a deliberately pessimistic stream recovery model.
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

    // --- the real client: disciplined clock + interpolation buffers ---
    let motions = [Motion::BhopStrafe, Motion::JumpSpam, Motion::Reversal];
    let mut clock = ClientServerClock::default();
    if let Some(ticks) = cfg.delay_ticks {
        clock.interpolation_delay_seconds = ticks as f64 / FIXED_TIMESTEP_HZ;
    }
    let mut buffers: Vec<RemoteInterpolationBuffer> = motions
        .iter()
        .map(|_| RemoteInterpolationBuffer::default())
        .collect();
    let mut applied = [0u64; 3];
    let mut last_pos: [Option<Vec3>; 3] = [None; 3];
    let mut runs = [0u32; 3];
    let mut report = Report {
        min_advance: 1.0,
        ..Default::default()
    };
    let mut errs: Vec<f32> = Vec::new();
    let mut prev_render_secs: Option<f64> = None;
    let mut endpoints: [Option<(f64, f64, RemoteSnapshotSample, RemoteSnapshotSample)>; 3] =
        [None; 3];

    let dt = 1.0 / RENDER_HZ;
    let frames = (SIM_SECONDS * RENDER_HZ) as u64;
    let mut next_arrival = 0;
    let mut stats_reset = false;
    for frame in 1..=frames {
        let now = frame as f64 * dt;
        // Join/priming is a separate problem: while the buffer fills to its
        // full depth the clock legitimately runs near the head. Steady-state
        // diagnostics start when the metric window does.
        if !stats_reset && now >= WARMUP_SECONDS {
            clock.stats = RemoteClockStats::default();
            stats_reset = true;
        }

        // PreUpdate: replicon applies every message that landed this frame;
        // the component ends at the NEWEST value and Changed fires once, so
        // the buffer gets exactly one sample — a burst coalesces into a gap.
        let mut newest = 0u64;
        while next_arrival < arrivals.len() && arrivals[next_arrival].0 <= now {
            newest = newest.max(arrivals[next_arrival].1);
            next_arrival += 1;
        }
        if newest > 0 {
            clock.observe_server_tick(newest);
            for (index, motion) in motions.iter().enumerate() {
                if newest > applied[index] {
                    applied[index] = newest;
                    let (position, velocity) =
                        truth(*motion, newest as f64 / FIXED_TIMESTEP_HZ);
                    buffers[index].push(RemoteSnapshotSample {
                        server_tick: newest,
                        position,
                        velocity,
                        ..Default::default()
                    });
                }
            }
        }
        // The production clock advances once per 20 Hz fixed tick; Avian
        // fills the three 60 Hz render frames between those endpoints.
        if frame % (RENDER_HZ as u64 / FIXED_TIMESTEP_HZ as u64) == 0 {
            clock.advance_fixed();
            let Some(fixed_time) = clock.target_time() else {
                continue;
            };
            for index in 0..motions.len() {
                let Some(sample) = buffers[index].sample(fixed_time) else {
                    continue;
                };
                endpoints[index] = Some(match endpoints[index] {
                    Some((_, current_ticks, _, current)) => (
                        current_ticks,
                        fixed_time.as_ticks_f64(),
                        current,
                        sample,
                    ),
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
        let frames_per_tick = RENDER_HZ / FIXED_TIMESTEP_HZ;
        let alpha = (frame % frames_per_tick as u64) as f32 / frames_per_tick as f32;
        let render_ticks = endpoints[0]
            .map(|(previous, current, _, _)| previous + (current - previous) * f64::from(alpha))
            .unwrap_or(target.as_ticks_f64());
        let render_secs = render_ticks / FIXED_TIMESTEP_HZ;
        if now >= WARMUP_SECONDS
            && let Some(previous) = prev_render_secs
        {
            let advance = (render_secs - previous) / dt;
            report.min_advance = report.min_advance.min(advance);
            if advance < 0.75 {
                report.hitches += 1;
            }
            let latest_renderable = (clock.latest_server_tick as f64 / FIXED_TIMESTEP_HZ
                - clock.interpolation_delay_seconds)
                .max(0.0);
            let lag = (render_secs - latest_renderable) / f64::from(REMOTE_EXTRAPOLATION_SECONDS);
            report.max_lag = report.max_lag.max(lag);
        }
        prev_render_secs = Some(render_secs);

        for (index, motion) in motions.iter().enumerate() {
            let Some((_, _, previous_endpoint, current_endpoint)) = endpoints[index] else {
                continue;
            };
            let sample_position = hermite_render_position(previous_endpoint, current_endpoint, alpha);
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
            // Stalled: the capsule moved way slower than the player truly was
            // at the rendered time. Normal clock motion stays near 1x.
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
        ..Default::default()
    };
    for report in reports {
        merged.hitches += report.hitches;
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
        merged.extrapolated += report.extrapolated;
        merged.capped += report.capped;
        merged.snaps += report.snaps;
    }
    merged
}

#[test]
fn snapshot_sim_bad_link_profiles() {
    let ws = |label, one_way_ms, jitter_ms, loss| Cfg {
        label,
        link: Link::Stream,
        one_way_ms,
        jitter_ms,
        loss,
        delay_ticks: None,
        forced_single: false,
        forced: &[],
    };

    let cfgs = [
        ws("ws clean rtt=50        ", 25.0, 5.0, 0.0),
        ws("ws rtt=50 j=20 1%      ", 25.0, 20.0, 0.01),
        ws("ws rtt=100 j=30 1%     ", 50.0, 30.0, 0.01),
        ws("ws rtt=150 j=40 1%     ", 75.0, 40.0, 0.01),
        ws("ws rtt=200 j=60 1%     ", 100.0, 60.0, 0.01),
        ws("ws rtt=200 j=60 3%     ", 100.0, 60.0, 0.03),
        // The direct question: does the interp buffer eat exactly ONE
        // WebSocket retransmit stall (1.5-2x RTT)?
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
        // Beyond spec: a ~1s outage. The designed failure mode is hold
        // (capped) then a clock snap on recovery — never a crawl.
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
        // Explicit three-tick rows: the same delay as the default, on rougher links.
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
    ];

    println!(
        "{:<24} {:>7} {:>7} {:>7} {:>7} {:>7} {:>8} {:>6} {:>8} {:>8} {:>8} {:>6} {:>6} {:>6}",
        "scenario", "hitches", "min_adv", "max_lag", "freezes", "max_run", "rev_run", "jumps",
        "max_step", "err_p95", "err_max", "xtrap", "capped", "snaps"
    );
    let mut results = Vec::new();
    for cfg in cfgs {
        let merged = merge([7, 1337, 0xDEAD_BEEF].map(|seed| simulate(cfg, seed)));
        println!(
            "{:<24} {:>7} {:>7.2} {:>7.2} {:>7} {:>7} {:>8} {:>6} {:>8.3} {:>8.3} {:>8.3} {:>6} {:>6} {:>6}",
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
            merged.extrapolated,
            merged.capped,
            merged.snaps
        );
        results.push(merged);
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

    // The 150 ms buffer plus real extrapolation covers ordinary regional
    // links, a 100 ms RTT retransmit, and datagram holes without parking the
    // presentation clock.
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

    // 150 ms history + 300 ms prediction now covers even severe 200 ms RTT
    // stream loss without parking the clock at the cap (it used to exhaust
    // the old 100 + 250 ms budget). Only a full outage may reach the cap.
    assert_eq!(ws200.capped, 0, "ws200: 450 ms budget should cover retransmit stalls");
    assert!(outage.capped > 0, "outage should park the clock at the cap");
    assert!(outage.hitches > 0, "the capped playback clock must hold");
    assert!(outage.jumps > 0, "recovery jumps instead of crawling");
}
