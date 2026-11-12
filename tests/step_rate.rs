//! The step at another tick rate: at 60 Hz a walk, a jump, a rocket and the push take the seconds
//! they take at 20 Hz.

// `pub`, so the parts of the harness this file doesn't use aren't dead code.
pub mod common;

use bevy::prelude::*;
use bevy_netahoy::{
    Buttons, PlayerId, SubtickFire, TickRate,
    testing::{Outbox, PredictionFrame},
};
use playground::{Arsenal, ROCKET_SPEED, Shot};

use common::*;

/// A scripted run at `rate`, in seconds: what one walk and one jump do.
struct Moves {
    /// When the fall from 2 m first stands on the floor.
    landed: f64,
    /// How far 2 s of walking from a standstill carry the player.
    walked: f32,
    /// A jump from standing: how high it peaks, and how long until it stands again.
    peak: f32,
    airtime: f64,
}

fn moves_at(rate: TickRate) -> Moves {
    let ticks = |seconds: f64| rate.ticks(seconds) as u32;
    let time = |command: u32| f64::from(command) / f64::from(rate.hz());
    let grounded = |frame: &PredictionFrame<Arsenal>| frame.state.moves.ground.is_some();

    let mut app = make_app_at(Vec3::new(0.0, 2.0, 0.0), rate);
    let frames = run(&mut app, &idle(1..=ticks(1.0)));
    let landed = frames.iter().find(|frame| grounded(frame)).expect("lands");
    let from = frames.last().unwrap().position();
    let walk: Vec<_> = (ticks(1.0) + 1..=ticks(3.0))
        .map(|seq| command(seq, Vec2::Y, Buttons::empty()))
        .collect();
    let walked = run(&mut app, &walk).last().unwrap().position();
    assert!(
        walked.y - HALF_HEIGHT < 0.05,
        "walked on the floor: {walked}"
    );

    let mut app = make_app_at(Vec3::new(0.0, HALF_HEIGHT + 0.01, 0.0), rate);
    let mut script = idle(1..=ticks(2.0));
    script[ticks(0.5) as usize].buttons = Buttons::JUMP;
    let frames = run(&mut app, &script);
    let jumped = frames
        .iter()
        .position(|frame| frame.state.moves.since_jump == 0)
        .expect("jumps");
    let down = jumped
        + frames[jumped..]
            .iter()
            .position(grounded)
            .expect("lands again");
    let peak = frames
        .iter()
        .map(|frame| frame.position().y)
        .fold(0.0, f32::max);

    Moves {
        landed: time(landed.command.sequence),
        walked: from.distance(walked),
        peak: peak - HALF_HEIGHT,
        airtime: time(frames[down].command.sequence) - time(frames[jumped].command.sequence),
    }
}

/// At 60 Hz the step covers the ground a second that it does at 20 Hz: the fall lands, the walk
/// goes as far and the jump peaks as high and lasts as long, to a 20 Hz tick's discretization.
#[test]
fn a_walk_and_a_jump_take_the_same_seconds_at_60_hz() {
    let (twenty, sixty) = (moves_at(TickRate::new(20)), moves_at(TickRate::new(60)));
    let tick = 1.0 / 20.0;
    let (landed, walked) = ((twenty.landed, sixty.landed), (twenty.walked, sixty.walked));
    assert!(
        (landed.1 - landed.0).abs() <= tick,
        "landed at {landed:?} s"
    );
    assert!(walked.0 > 8.0, "walked {} m", walked.0);
    assert!(
        (walked.1 - walked.0).abs() < 0.02 * walked.0,
        "walked {walked:?} m"
    );
    let (peak, airtime) = ((twenty.peak, sixty.peak), (twenty.airtime, sixty.airtime));
    assert!(peak.0 > 0.5, "jumped {} m", peak.0);
    assert!((peak.1 - peak.0).abs() < 0.05, "peaked at {peak:?} m");
    assert!(
        (airtime.1 - airtime.0).abs() <= tick,
        "in the air for {airtime:?} s"
    );
}

/// From the click to the blast, a rocket's flight at `rate` (s): one into open air (its whole
/// lifetime), one at a wall 30 m ahead.
fn rocket_flights_at(rate: TickRate) -> [f64; 2] {
    let start = Vec3::new(0.0, HALF_HEIGHT + 0.01, 0.0);
    let (fired, frac) = (3, 0.5);
    [Vec2::new(0.0, 1.2), Vec2::ZERO].map(|look| {
        let mut app = make_app_at(start, rate);
        spawn_wall(&mut app, 30.0);
        let mut outbox = Outbox::default();
        for seq in 1..=rate.ticks(3.0) as u32 {
            let mut command = command(seq, Vec2::ZERO, Buttons::empty());
            command.look = look;
            if seq == fired {
                command.fire = Some(SubtickFire {
                    frac,
                    look,
                    origin: None,
                });
            }
            run_one(&mut app, &mut outbox, command, Buttons::empty());
            let shots: Vec<Shot> = written(&mut app);
            if shots.iter().any(|shot| matches!(shot, Shot::Hit(_))) {
                // The click is `frac` into command `fired`'s window, the blast ends `seq`'s.
                let ticks = f64::from(seq + 1 - fired) - f64::from(frac);
                return ticks / f64::from(rate.hz());
            }
        }
        panic!("no blast at {} Hz", rate.hz());
    })
}

/// A rocket's fuse is counted in ticks at the step's rate: at 60 Hz it flies the seconds it
/// flies at 20 Hz, each to within half its own tick.
#[test]
fn a_rocket_flies_the_same_seconds_at_60_hz() {
    let twenty = rocket_flights_at(TickRate::new(20));
    let sixty = rocket_flights_at(TickRate::new(60));
    let expected = [1.35, 30.0 / f64::from(ROCKET_SPEED)];
    for ((twenty, sixty), expected) in twenty.into_iter().zip(sixty).zip(expected) {
        assert!(
            (twenty - expected).abs() <= 0.5 / 20.0 + 1e-9,
            "{twenty} s at 20 Hz"
        );
        assert!(
            (sixty - expected).abs() <= 0.5 / 60.0 + 1e-9,
            "{sixty} s at 60 Hz"
        );
    }
}

/// The push out of another player standing 0.3 m off, at `rate`: the overlap after each command
/// of a second's standing still (m), and how long a command is (s).
fn push_at(rate: TickRate) -> (Vec<f32>, f64) {
    let ground = HALF_HEIGHT + 0.01;
    let mut app = make_app_at(Vec3::new(0.0, ground, 0.0), rate);
    let other = Vec3::new(0.3, ground, 0.0);
    let ticks = rate.ticks(1.0) as u32;
    spawn_seen_player(&mut app, PlayerId(2), u64::from(ticks) + 2, |_| other);
    let touch = 2.0 * shape().radius;
    let overlaps = run(&mut app, &idle(1..=ticks))
        .iter()
        .map(|frame| (touch - (frame.position() - other).xz().length()).max(0.0))
        .collect();
    (overlaps, rate.seconds())
}

/// The push is an acceleration: at 60 Hz a second of it drains the overlap it drains at 20 Hz.
/// Measured as overlap-seconds (how deep, for how long), which a share of the overlap closed
/// each tick would make the same at any rate; within a tenth, for 20 Hz's coarser ticks.
#[test]
fn the_push_drains_an_overlap_in_the_same_seconds_at_60_hz() {
    let overlap_seconds = |hz| {
        let (overlaps, dt) = push_at(TickRate::new(hz));
        assert!(
            overlaps[0] > 0.5,
            "{hz} Hz: starts 0.6 m deep: {overlaps:?}"
        );
        assert_eq!(overlaps.last(), Some(&0.0), "{hz} Hz: drained in a second");
        overlaps
            .iter()
            .map(|overlap| f64::from(*overlap) * dt)
            .sum::<f64>()
    };
    let (twenty, sixty) = (overlap_seconds(20), overlap_seconds(60));
    assert!(
        (sixty - twenty).abs() < 0.1 * twenty,
        "overlap-seconds {twenty} at 20 Hz, {sixty} at 60"
    );
}
