//! The example's bots through the real step, closed loop: each tick's command comes from where
//! the last one left the bot, as on the server.

pub mod common;

use bevy::prelude::*;
use bevy_netahoy::{Buttons, TickRate, testing::Outbox};
use playground::bots::{AdStrafer, Strafer};

use common::*;

/// The strafer hops and carves round its circuit: fast, mostly in the air, never far off the
/// line, a lap at a time.
#[test]
fn the_strafer_air_strafes_round_its_circuit() {
    let rate = TickRate::default();
    let mut brain = Strafer::default();
    let mut app = make_app(brain.spawn_point());
    let mut outbox = Outbox::default();
    let mut previous = Buttons::empty();

    let seconds = 60.0;
    let ticks = rate.ticks(seconds) as u32;
    let (mut airborne, mut worst_off_line, mut top) = (0, 0.0_f32, 0.0_f32);
    let mut speeds = Vec::new();
    let mut turned = 0.0;
    let mut last_angle = Strafer::circuit_angle(brain.spawn_point());
    let mut moves = run_one(&mut app, &mut outbox, idle(1..=1)[0], previous)
        .state
        .moves;
    for sequence in 2..=ticks {
        let intent = brain.think(&moves, u64::from(sequence), rate);
        let command = intent.command(sequence, u64::from(sequence));
        moves = run_one(&mut app, &mut outbox, command, previous)
            .state
            .moves;
        previous = command.buttons;

        let angle = Strafer::circuit_angle(moves.position);
        turned += (angle - last_angle + std::f32::consts::PI).rem_euclid(std::f32::consts::TAU)
            - std::f32::consts::PI;
        last_angle = angle;
        // Settled after the first 5 s, from a standing start.
        if sequence < rate.ticks(5.0) as u32 {
            continue;
        }
        let line = Strafer::circuit_point(angle);
        worst_off_line = worst_off_line.max((moves.position - line).xz().length());
        let speed = moves.velocity.xz().length();
        top = top.max(speed);
        speeds.push(speed);
        airborne += usize::from(moves.ground.is_none());
    }
    let mean = speeds.iter().sum::<f32>() / speeds.len() as f32;
    let air = airborne as f32 / speeds.len() as f32;
    let laps = turned / std::f32::consts::TAU;
    println!(
        "strafer: mean {mean:.1} m/s, top {top:.1}, {:.0}% in the air, {laps:.2} laps, at most {worst_off_line:.2} m off the line",
        air * 100.0
    );
    assert!(mean > 13.0, "fast: {mean:.1} m/s");
    assert!(top < 26.0, "but held under the top speed: {top:.1} m/s");
    assert!(air > 0.75, "mostly in the air: {:.0}%", air * 100.0);
    assert!(worst_off_line < 4.0, "on the line: {worst_off_line:.2} m");
    assert!(laps > 3.0, "lapping counterclockwise: {laps:.2}");
}

/// The A/D bot strafes hard both ways in its lane, on the ground.
#[test]
fn the_ad_bot_strafes_both_ways_in_its_lane() {
    let rate = TickRate::default();
    let mut brain = AdStrafer::default();
    let mut app = make_app(AdStrafer::SPAWN);
    let mut outbox = Outbox::default();
    let mut previous = Buttons::empty();
    let mut moves = run_one(&mut app, &mut outbox, idle(1..=1)[0], previous)
        .state
        .moves;

    let (mut reversals, mut last_sign, mut fastest) = (0, 0.0_f32, 0.0_f32);
    let (mut least_x, mut most_x) = (f32::MAX, f32::MIN);
    for sequence in 2..=rate.ticks(30.0) as u32 {
        let intent = brain.think(&moves, u64::from(sequence), rate);
        let command = intent.command(sequence, u64::from(sequence));
        moves = run_one(&mut app, &mut outbox, command, previous)
            .state
            .moves;
        previous = command.buttons;

        assert!(moves.ground.is_some() || sequence < 10, "on the ground");
        assert!(
            (moves.position.z - AdStrafer::SPAWN.z).abs() < 0.2,
            "in its lane"
        );
        least_x = least_x.min(moves.position.x);
        most_x = most_x.max(moves.position.x);
        fastest = fastest.max(moves.velocity.x.abs());
        let sign = moves.velocity.x.signum();
        if moves.velocity.x.abs() > 2.0 && sign != last_sign {
            reversals += 1;
            last_sign = sign;
        }
    }
    println!(
        "a/d bot: {reversals} reversals in 30 s, x {least_x:.1} to {most_x:.1}, fastest {fastest:.1} m/s"
    );
    assert!(reversals > 25, "keeps switching: {reversals}");
    assert!(fastest > 5.0, "hard strafes: {fastest:.1} m/s");
    let reach = AdStrafer::LANE_HALF_WIDTH + 1.5;
    assert!(
        least_x > -reach && most_x < reach,
        "x {least_x:.1} to {most_x:.1}"
    );
}
