//! Pacing the fixed clock (the input clock's nudge) must not show as jank on the rendered local
//! player.
//!
//! A real Bevy app with `Time<Fixed>` and `pace_fixed_clock` renders a known curve (a running
//! weave) the way a presentation would: a cubic Hermite over the last two tick endpoints at the
//! overstep fraction. Each frame's pose is compared to the true pose at the game time it stands
//! for, on three counts:
//! 1. a paced tick still steps exactly one timestep;
//! 2. the render stays on the Hermite curve and never jumps;
//! 3. on-screen game speed changes gently: the pace is the playback speed of the whole local
//!    sim, so a pace step is a velocity step on screen, which the slew turns into a ramp.

use std::time::Duration;

use super::{COMMAND_CLOCK_RATE_SLEW, CommandClock, pace_fixed_clock, settle_fixed_clock};
use crate::protocol::TickRate;
use bevy::{
    app::{RunFixedMainLoop, RunFixedMainLoopSystems},
    prelude::*,
    time::TimeUpdateStrategy,
};

const RATE: TickRate = TickRate::new(20);
const HZ: f64 = RATE.hz() as f64;
const SPEED: f64 = 8.0;
const WEAVE: f64 = 1.0;
const OMEGA: f64 = std::f64::consts::PI;

fn path(t: f64) -> (f64, f64) {
    (SPEED * t, WEAVE * (OMEGA * t).sin())
}

fn path_velocity(t: f64) -> (f64, f64) {
    (SPEED, WEAVE * OMEGA * (OMEGA * t).cos())
}

#[derive(Clone, Copy)]
struct TickFrame {
    game_time: f64,
    position: (f64, f64),
    velocity: (f64, f64),
}

#[derive(Clone, Copy)]
struct RenderSample {
    real_time: f64,
    game_time: f64,
    position: (f64, f64),
}

#[derive(Resource)]
struct Rig {
    /// Pace schedule: (real seconds from, target pace).
    schedule: Vec<(f64, f64)>,
    /// Skip the slew: jump the applied pace straight to the target.
    instant: bool,
    ticks: u64,
    frames: Option<[TickFrame; 2]>,
    samples: Vec<RenderSample>,
    tick_deltas_exact: bool,
    real_time: f64,
}

fn sim_tick(fixed: Res<Time<Fixed>>, mut rig: ResMut<Rig>) {
    if fixed.delta() != fixed.timestep() {
        rig.tick_deltas_exact = false;
    }
    rig.ticks += 1;
    let game_time = rig.ticks as f64 / HZ;
    let current = TickFrame {
        game_time,
        position: path(game_time),
        velocity: path_velocity(game_time),
    };
    let previous = rig.frames.map_or(current, |[_, current]| current);
    rig.frames = Some([previous, current]);
}

/// A presentation's cubic Hermite, in f64 so the measurement isn't f32 noise.
fn hermite(p0: f64, v0: f64, p1: f64, v1: f64, dt: f64, u: f64) -> f64 {
    let u2 = u * u;
    let u3 = u2 * u;
    (2.0 * u3 - 3.0 * u2 + 1.0) * p0
        + (u3 - 2.0 * u2 + u) * dt * v0
        + (-2.0 * u3 + 3.0 * u2) * p1
        + (u3 - u2) * dt * v1
}

fn render(fixed: Res<Time<Fixed>>, real: Res<Time<Real>>, mut rig: ResMut<Rig>) {
    rig.real_time += real.delta_secs_f64();
    let Some([previous, current]) = rig.frames else {
        return;
    };
    let dt = fixed.timestep().as_secs_f64();
    let u = fixed.overstep_fraction_f64();
    let position = (
        hermite(
            previous.position.0,
            previous.velocity.0,
            current.position.0,
            current.velocity.0,
            dt,
            u,
        ),
        hermite(
            previous.position.1,
            previous.velocity.1,
            current.position.1,
            current.velocity.1,
            dt,
            u,
        ),
    );
    let sample = RenderSample {
        real_time: rig.real_time,
        game_time: previous.game_time + u * dt,
        position,
    };
    rig.samples.push(sample);
}

fn drive_pace(rig: Res<Rig>, mut clock: ResMut<CommandClock>) {
    let now = rig.real_time;
    let target = rig
        .schedule
        .iter()
        .rev()
        .find(|(from, _)| now >= *from)
        .map_or(1.0, |(_, pace)| *pace);
    clock.target_rate = target;
    if rig.instant {
        clock.rate = target;
    }
}

struct Run {
    samples: Vec<RenderSample>,
    tick_deltas_exact: bool,
}

fn run(frame_ms: &[f64], schedule: Vec<(f64, f64)>, instant: bool, seconds: f64) -> Run {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins)
        .insert_resource(Time::<Fixed>::from_hz(HZ))
        .insert_resource(CommandClock::new(RATE))
        .insert_resource(Rig {
            schedule,
            instant,
            ticks: 0,
            frames: None,
            samples: Vec::new(),
            tick_deltas_exact: true,
            real_time: 0.0,
        })
        .add_systems(
            RunFixedMainLoop,
            (drive_pace, pace_fixed_clock)
                .chain()
                .in_set(RunFixedMainLoopSystems::BeforeFixedMainLoop),
        )
        .add_systems(FixedUpdate, sim_tick)
        .add_systems(
            RunFixedMainLoop,
            (settle_fixed_clock, render)
                .chain()
                .in_set(RunFixedMainLoopSystems::AfterFixedMainLoop),
        );

    let mut elapsed = 0.0;
    let mut frame = 0;
    while elapsed < seconds * 1000.0 {
        let ms = frame_ms[frame % frame_ms.len()];
        app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(
            ms / 1000.0,
        )));
        app.update();
        elapsed += ms;
        frame += 1;
    }

    let rig = app.world_mut().remove_resource::<Rig>().unwrap();
    Run {
        // Until two real ticks exist, the "pair" is one tick twice.
        samples: rig
            .samples
            .into_iter()
            .filter(|sample| sample.real_time >= 0.5)
            .collect(),
        tick_deltas_exact: rig.tick_deltas_exact,
    }
}

#[derive(Debug)]
struct Report {
    /// Worst distance between the render and the true pose at the game time
    /// it represents. Pacing must not change this: it's Hermite's own error.
    max_curve_error: f64,
    /// Largest one-frame on-screen velocity error against the true curve
    /// played at the same pace, m/s: Hermite's knots. Pacing must not add.
    max_velocity_step: f64,
    /// Largest one-frame change of on-screen game speed (dτ/dt).
    max_pace_step: f64,
    /// That change as felt: the speed jolt it puts on the player, m/s.
    max_speed_jolt: f64,
    /// Fastest a single frame moved, relative to the curve's speed at that
    /// pace (1.0 = exactly as far as it should; a pop would be >> 1).
    max_frame_travel_ratio: f64,
}

fn report(run: &Run) -> Report {
    let samples = &run.samples;
    let mut report = Report {
        max_curve_error: 0.0,
        max_velocity_step: 0.0,
        max_pace_step: 0.0,
        max_speed_jolt: 0.0,
        max_frame_travel_ratio: 0.0,
    };
    let distance =
        |a: (f64, f64), b: (f64, f64)| ((a.0 - b.0).powi(2) + (a.1 - b.1).powi(2)).sqrt();

    for sample in samples {
        report.max_curve_error = report
            .max_curve_error
            .max(distance(sample.position, path(sample.game_time)));
    }
    let velocity = |a: &RenderSample, b: &RenderSample| {
        let dt = b.real_time - a.real_time;
        (
            (b.position.0 - a.position.0) / dt,
            (b.position.1 - a.position.1) / dt,
        )
    };
    let true_velocity = |a: &RenderSample, b: &RenderSample| {
        let dt = b.real_time - a.real_time;
        let (x0, y0) = path(a.game_time);
        let (x1, y1) = path(b.game_time);
        ((x1 - x0) / dt, (y1 - y0) / dt)
    };
    for window in samples.windows(3) {
        let [a, b, c] = window else { unreachable!() };
        let (v0, v1) = (velocity(a, b), velocity(b, c));
        let (t0, t1) = (true_velocity(a, b), true_velocity(b, c));
        // The curve's own turn (and Hermite's knots) subtract out: what's
        // left is the velocity change pacing added on top.
        let step = distance((v1.0 - v0.0, v1.1 - v0.1), (t1.0 - t0.0, t1.1 - t0.1));
        report.max_velocity_step = report.max_velocity_step.max(step);

        let pace0 = (b.game_time - a.game_time) / (b.real_time - a.real_time);
        let pace1 = (c.game_time - b.game_time) / (c.real_time - b.real_time);
        report.max_pace_step = report.max_pace_step.max((pace1 - pace0).abs());
        let (vx, vy) = path_velocity(b.game_time);
        report.max_speed_jolt = report
            .max_speed_jolt
            .max((pace1 - pace0).abs() * (vx * vx + vy * vy).sqrt());

        let travelled = distance(b.position, c.position);
        let expected = distance(path(b.game_time), path(c.game_time));
        if expected > 1e-6 {
            report.max_frame_travel_ratio = report.max_frame_travel_ratio.max(travelled / expected);
        }
    }
    report
}

/// 1.0, then +3%, then -3%, then back: the nudge's biggest everyday swing.
fn swing() -> Vec<(f64, f64)> {
    vec![(0.0, 1.0), (2.0, 1.03), (5.0, 0.97), (8.0, 1.0)]
}

#[test]
fn pacing_keeps_the_rendered_player_on_its_curve() {
    for (label, frames) in [
        ("144 Hz", vec![1000.0 / 144.0]),
        ("60 Hz", vec![1000.0 / 60.0]),
        ("jittery ~90 Hz", vec![9.0, 13.0, 10.0, 12.5, 8.5]),
    ] {
        let baseline = report(&run(&frames, vec![(0.0, 1.0)], false, 11.0));
        let slewed_run = run(&frames, swing(), false, 11.0);
        let slewed = report(&slewed_run);
        let stepped = report(&run(&frames, swing(), true, 11.0));
        println!("{label:>15} baseline {baseline:?}");
        println!("{label:>15} slewed   {slewed:?}");
        println!("{label:>15} stepped  {stepped:?}");

        // 1. Every paced tick still steps exactly one timestep.
        assert!(
            slewed_run.tick_deltas_exact,
            "{label}: a tick stepped a different dt"
        );

        // 2. Same curve: pacing adds nothing to Hermite's own error, and no
        // frame jumps further than the curve moved.
        assert!(
            slewed.max_curve_error <= baseline.max_curve_error + 1e-6,
            "{label}: pacing pulled the render off the curve: {slewed:?} vs {baseline:?}"
        );
        assert!(
            slewed.max_frame_travel_ratio < 1.01,
            "{label}: a frame jumped: {slewed:?}"
        );

        // 3. Time base: with the slew, game speed changes by at most COMMAND_CLOCK_RATE_SLEW
        // per second; stepping the pace instead is a one-frame jolt of (pace change) x speed.
        let longest_frame = frames.iter().cloned().fold(0.0, f64::max) / 1000.0;
        assert!(
            slewed.max_pace_step <= COMMAND_CLOCK_RATE_SLEW * longest_frame * 1.5 + 1e-9,
            "{label}: pace changed too fast: {slewed:?}"
        );
        assert!(
            slewed.max_velocity_step <= baseline.max_velocity_step + 0.05,
            "{label}: pacing added a velocity jolt: {slewed:?} vs {baseline:?}"
        );
        assert!(
            slewed.max_speed_jolt < 0.01,
            "{label}: pacing jolted the player: {slewed:?}"
        );
        assert!(
            stepped.max_speed_jolt > slewed.max_speed_jolt * 50.0,
            "{label}: expected the unslewed step to be a visible jolt: {stepped:?}"
        );
    }
}
