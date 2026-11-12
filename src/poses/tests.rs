use super::*;
use crate::{
    lag_comp::PlayerPoses,
    protocol::{MoveFlags, PlayerId},
};

const RATE: TickRate = TickRate::new(20);

fn sample(server_tick: u64, position: Vec3) -> PlayerSnapshot {
    PlayerSnapshot {
        server_tick,
        position,
        look: Vec2::ZERO,
        move_flags: MoveFlags::default(),
        flags: RemoteFlags::empty(),
    }
}

fn buffer(samples: &[PlayerSnapshot]) -> History<PlayerSnapshot> {
    let mut history = History::new(RATE);
    for sample in samples {
        history.record(sample.server_tick, *sample);
    }
    history
}

fn body(tick: u64, position: Vec3, rotation: Quat) -> BodySnapshot {
    BodySnapshot {
        tick,
        position,
        rotation,
        ..default()
    }
}

fn bodies(snapshots: &[BodySnapshot]) -> History<BodySnapshot> {
    let mut history = History::new(RATE);
    for snapshot in snapshots {
        history.record(snapshot.tick, *snapshot);
    }
    history
}

#[test]
fn seconds_are_the_rates_ticks() {
    assert_eq!(
        RemoteRenderTime::from_seconds(1.3, RATE),
        RemoteRenderTime::new(26, 0.0)
    );
    let sixty = RemoteRenderTime::from_seconds(1.3, TickRate::new(60));
    assert_eq!(sixty.tick, 78);
    assert!(sixty.alpha < 1e-6, "{sixty:?}");
}

#[test]
fn interpolation_hits_endpoints() {
    let a = sample(10, Vec3::new(0.0, 0.0, 0.0));
    let b = sample(11, Vec3::new(1.0, 2.0, 3.0));
    let buf = buffer(&[a, b]);

    let at_a = pose_at(&buf, RemoteRenderTime::new(10, 0.0), RATE).unwrap();
    assert!(at_a.position.distance(a.position) < 1e-4);
    let at_b = pose_at(&buf, RemoteRenderTime::new(11, 0.0), RATE).unwrap();
    assert!(at_b.position.distance(b.position) < 1e-4);
}

#[test]
fn player_velocity_is_the_hermite_derivative() {
    let previous = sample(9, Vec3::new(-1.0, 0.0, 0.0));
    let start = sample(10, Vec3::ZERO);
    let end = sample(11, Vec3::X);
    let buf = buffer(&[previous, start, end]);

    let mid = pose_at(&buf, RemoteRenderTime::new(10, 0.5), RATE).unwrap();
    assert!(mid.position.y.abs() < 1e-5);
    assert!((mid.position.x - 0.5).abs() < 1e-5);
    assert!((mid.velocity.x - RATE.hz() as f32).abs() < 1e-3);
}

fn line(xs: &[(u64, f32)]) -> History<PlayerSnapshot> {
    let samples: Vec<_> = xs
        .iter()
        .map(|&(tick, x)| sample(tick, Vec3::X * x))
        .collect();
    buffer(&samples)
}

/// `(tick, alpha)` every tenth of a tick from `from` to `to`.
fn tenths(from: u64, to: u64) -> impl Iterator<Item = RemoteRenderTime> {
    (from..to).flat_map(|tick| (0..10).map(move |i| RemoteRenderTime::new(tick, i as f32 * 0.1)))
}

/// A knot's velocity is the average of the chords either side of it, and
/// the two segments meeting there agree on it.
#[test]
fn a_knot_moves_at_the_average_of_its_chords() {
    let hz = RATE.hz() as f32;
    let buf = line(&[(9, 0.0), (10, 1.0), (11, 3.0), (12, 6.0), (13, 10.0)]);
    let velocity = |tick, alpha| {
        pose_at(&buf, RemoteRenderTime::new(tick, alpha), RATE)
            .unwrap()
            .velocity
            .x
    };
    assert!((velocity(10, 0.0001) - 1.5 * hz).abs() < 0.1);
    assert!((velocity(10, 0.9999) - 2.5 * hz).abs() < 0.1);
    assert!((velocity(11, 0.0001) - 2.5 * hz).abs() < 0.1);
}

#[test]
fn leaving_a_stand_never_steps_back() {
    let buf = line(&[
        (9, 0.0),
        (10, 0.0),
        (11, 0.0),
        (12, 1.0),
        (13, 2.0),
        (14, 3.0),
    ]);
    let mut last = 0.0;
    for time in tenths(10, 13) {
        let x = pose_at(&buf, time, RATE).unwrap().position.x;
        assert!(x >= last, "{x} after {last} at {time:?}");
        last = x;
    }
    let standing = pose_at(&buf, RemoteRenderTime::new(11, 0.0), RATE).unwrap();
    assert_eq!(
        standing.velocity,
        Vec3::ZERO,
        "the knot it leaves from has no velocity"
    );
}

#[test]
fn a_stop_is_never_passed() {
    let buf = line(&[
        (9, -2.0),
        (10, -1.0),
        (11, 0.0),
        (12, 0.0),
        (13, 0.0),
        (14, 0.0),
    ]);
    let mut last = -2.0;
    for time in tenths(10, 13) {
        let x = pose_at(&buf, time, RATE).unwrap().position.x;
        assert!(x >= last && x <= 0.0, "{x} after {last} at {time:?}");
        last = x;
    }
}

#[test]
fn landing_at_a_run_does_not_dip_below_the_floor() {
    let buf = buffer(&[
        sample(9, Vec3::new(0.0, 2.0, 0.0)),
        sample(10, Vec3::new(0.5, 1.0, 0.0)),
        sample(11, Vec3::new(1.0, 0.0, 0.0)),
        sample(12, Vec3::new(1.5, 0.0, 0.0)),
        sample(13, Vec3::new(2.0, 0.0, 0.0)),
        sample(14, Vec3::new(2.5, 0.0, 0.0)),
    ]);
    for time in tenths(10, 13) {
        let pose = pose_at(&buf, time, RATE).unwrap();
        assert!(pose.position.y >= 0.0, "{pose:?} at {time:?}");
    }
}

#[test]
fn a_teleport_after_the_segment_does_not_bend_it() {
    let hz = RATE.hz() as f32;
    let buf = line(&[(9, 0.0), (10, 1.0), (11, 2.0), (12, 100.0), (13, 101.0)]);
    let end = pose_at(&buf, RemoteRenderTime::new(10, 0.9999), RATE).unwrap();
    assert!((end.velocity.x - hz).abs() < 0.1, "{end:?}");
}

fn looks(ticks: &[(u64, f32, f32)]) -> History<PlayerSnapshot> {
    let samples: Vec<_> = ticks
        .iter()
        .map(|&(tick, yaw, pitch)| PlayerSnapshot {
            look: Vec2::new(yaw, pitch),
            ..sample(tick, Vec3::ZERO)
        })
        .collect();
    buffer(&samples)
}

#[test]
fn a_look_turns_the_short_way_across_the_seam() {
    let tau = std::f32::consts::TAU;
    let buf = looks(&[
        (9, 3.0, 0.0),
        (10, 3.1, 0.0),
        (11, 3.2 - tau, 0.0),
        (12, 3.3 - tau, 0.0),
    ]);
    let mid = pose_at(&buf, RemoteRenderTime::new(10, 0.5), RATE)
        .unwrap()
        .look
        .x;
    assert!(radians_between(mid, 3.15).abs() < 1e-3, "{mid}");
}

#[test]
fn a_look_keeps_its_rate_through_a_knot() {
    let buf = looks(&[
        (9, 0.0, 0.0),
        (10, 0.1, 0.0),
        (11, 0.3, 0.0),
        (12, 0.6, 0.0),
        (13, 1.0, 0.0),
    ]);
    let yaw = |tick, alpha| {
        pose_at(&buf, RemoteRenderTime::new(tick, alpha), RATE)
            .unwrap()
            .look
            .x
    };
    let into = (yaw(10, 0.999) - yaw(10, 0.99)) / 0.009;
    let out = (yaw(11, 0.01) - yaw(11, 0.001)) / 0.009;
    assert!((into - out).abs() < 0.02, "{into} into, {out} out");
}

#[test]
fn a_look_stops_without_passing_and_pitch_stays_in_range() {
    let buf = looks(&[
        (9, 0.0, 1.0),
        (10, 0.2, 1.5),
        (11, 0.4, 1.4),
        (12, 0.4, 1.3),
        (13, 0.4, 1.2),
    ]);
    for time in tenths(10, 12) {
        let look = pose_at(&buf, time, RATE).unwrap().look;
        assert!(look.x <= 0.4 + 1e-5, "{look} at {time:?}");
        assert!(look.y <= REMOTE_PITCH_LIMIT, "{look} at {time:?}");
    }
}

/// The server publishes every player every tick, so a tick nothing arrived for was lost on the
/// way (a dropped WebTransport datagram): it is on the line between its neighbours, and the
/// player moves through it instead of standing for a tick and then stepping twice.
#[test]
fn a_player_tick_lost_on_the_way_is_on_the_line() {
    let previous = sample(8, Vec3::X * -2.0);
    let start = sample(10, Vec3::ZERO);
    let end = sample(11, Vec3::X);
    let buf = buffer(&[previous, start, end]);

    let lost = pose_at(&buf, RemoteRenderTime::new(9, 0.0), RATE).unwrap();
    assert_eq!(lost.position, Vec3::X * -1.0);
    let moving = pose_at(&buf, RemoteRenderTime::new(9, 0.5), RATE).unwrap();
    assert!(moving.position.x > -1.0 && moving.position.x < 0.0);
    let at_end = pose_at(&buf, RemoteRenderTime::new(11, 0.0), RATE).unwrap();
    assert_eq!(at_end.position, end.position);
}

#[test]
fn source_hermite_keeps_velocity_continuous_through_a_turn() {
    let buf = buffer(&[
        sample(9, Vec3::NEG_X),
        sample(10, Vec3::ZERO),
        sample(11, Vec3::Y),
        sample(12, Vec3::Y * 2.0),
    ]);

    let before = pose_at(&buf, RemoteRenderTime::new(10, 0.999), RATE).unwrap();
    let after = pose_at(&buf, RemoteRenderTime::new(11, 0.001), RATE).unwrap();
    assert!(before.velocity.distance(after.velocity) < 0.2);
}

/// Two entries have no preceding tangent: linear, at the segment's own
/// velocities.
#[test]
fn two_body_entries_are_linear() {
    let history = bodies(&[
        body(10, Vec3::ZERO, Quat::IDENTITY),
        body(11, Vec3::X, Quat::from_rotation_y(1.0)),
    ]);

    let mid = body_at(&history, RemoteRenderTime::new(10, 0.5), RATE).unwrap();
    assert!((mid.position.x - 0.5).abs() < 1e-4);
    let hz = RATE.hz() as f32;
    assert!(mid.linear_velocity.distance(Vec3::X * hz) < 1e-3);
    assert!(mid.angular_velocity.distance(Vec3::Y * hz) < 1e-3);
    assert!((mid.rotation.angle_between(Quat::from_rotation_y(0.5))).abs() < 1e-4);
}

/// A body turning about Y, `yaw(k)` rad at tick k, for ticks 8 to 14.
fn turning(yaw: impl Fn(f32) -> f32) -> History<BodySnapshot> {
    let entries: Vec<_> = (8..=14)
        .map(|tick| body(tick, Vec3::ZERO, Quat::from_rotation_y(yaw(tick as f32))))
        .collect();
    bodies(&entries)
}

/// On a steady turn and on one speeding up, the angular velocity runs on through an entry: the
/// segments either side agree at the knot, where a slerp's jumps by the change in the turn per
/// tick. It is the drawn rotation's rate of turn.
#[test]
fn a_body_turns_without_a_kink_at_an_entry() {
    let hz = RATE.hz() as f32;
    for (name, yaw) in [
        ("steady", (|k: f32| 0.2 * k) as fn(f32) -> f32),
        ("speeding up", |k: f32| 0.02 * k * k),
    ] {
        let history = turning(yaw);
        let at = |tick, alpha| body_at(&history, RemoteRenderTime::new(tick, alpha), RATE).unwrap();
        // Entry 11, from the segment into it (at its end) and the one out of it.
        let into = at(10, 0.9999).angular_velocity;
        let out = at(11, 0.0001).angular_velocity;
        assert!(into.distance(out) < 1e-2, "{name}: {into} into, {out} out");
        // The knot's rate: the turns either side, averaged.
        let knot = (yaw(12.0) - yaw(10.0)) / 2.0 * hz;
        assert!(
            (out.y - knot).abs() < 1e-2,
            "{name}: {out} at the knot, {knot}"
        );
        // The reported rate is the drawn rotation's.
        let (a, b) = (at(10, 0.4).rotation, at(10, 0.41).rotation);
        let drawn = angular_velocity(a, b, 0.01 / hz);
        let mid = at(10, 0.405).angular_velocity;
        assert!(
            drawn.distance(mid) < 1e-2,
            "{name}: {drawn} drawn, {mid} reported"
        );
    }
}

#[test]
fn body_velocity_is_the_hermite_derivative() {
    let history = bodies(&[
        body(9, Vec3::NEG_X, Quat::IDENTITY),
        body(10, Vec3::ZERO, Quat::IDENTITY),
        body(11, Vec3::X, Quat::IDENTITY),
    ]);

    let mid = body_at(&history, RemoteRenderTime::new(10, 0.5), RATE).unwrap();
    assert!(mid.position.y.abs() < 1e-5);
    assert!((mid.position.x - 0.5).abs() < 1e-5);
    assert!((mid.linear_velocity.x - RATE.hz() as f32).abs() < 1e-3);
}

#[test]
fn body_sampling_does_not_smear_motion_back_through_a_rest_gap() {
    let history = bodies(&[
        body(10, Vec3::ZERO, Quat::IDENTITY),
        body(20, Vec3::X, Quat::IDENTITY),
    ]);

    let before_motion = body_at(&history, RemoteRenderTime::new(15, 0.0), RATE).unwrap();
    assert_eq!(before_motion.position, Vec3::ZERO);
    let last_tick = body_at(&history, RemoteRenderTime::new(19, 0.5), RATE).unwrap();
    assert!(last_tick.position.x > 0.0 && last_tick.position.x < 1.0);
}

#[test]
fn a_long_gap_holds_instead_of_curving() {
    let previous = sample(9, Vec3::NEG_X);
    let a = sample(10, Vec3::ZERO);
    let b = sample(15, Vec3::new(10.0, 0.0, 0.0));
    let buf = buffer(&[previous, a, b]);

    let mid = pose_at(&buf, RemoteRenderTime::new(12, 0.5), RATE).unwrap();
    assert!(
        mid.position.y.abs() < 1e-4,
        "a long gap holds, no invented curve, got y = {}",
        mid.position.y
    );
}

#[test]
fn extrapolation_is_capped() {
    // 0.5 m a tick: 10 m/s.
    let buf = buffer(&[sample(99, Vec3::X * -0.5), sample(100, Vec3::ZERO)]);

    // One tick past: dead-reckon 50 ms at 10 m/s.
    let near = pose_at(&buf, RemoteRenderTime::new(101, 0.0), RATE).unwrap();
    assert!((near.position.x - 0.5).abs() < 1e-4);
    assert!((near.velocity.x - 10.0).abs() < 1e-3);

    // A full second past: clamped to REMOTE_EXTRAPOLATION_SECONDS, and held.
    let far = pose_at(&buf, RemoteRenderTime::new(120, 0.0), RATE).unwrap();
    assert!((far.position.x - 10.0 * REMOTE_EXTRAPOLATION_SECONDS).abs() < 1e-4);
    assert_eq!(far.velocity, Vec3::ZERO);

    // One entry has no motion to carry on.
    let lone = buffer(&[sample(100, Vec3::ZERO)]);
    let held = pose_at(&lone, RemoteRenderTime::new(101, 0.0), RATE).unwrap();
    assert_eq!((held.position, held.velocity), (Vec3::ZERO, Vec3::ZERO));
}

/// Into extrapolation the motion runs on: the end tangent the Hermite
/// ends the last segment on is the velocity it dead-reckons with.
#[test]
fn extrapolation_continues_the_hermite() {
    let buf = buffer(&[
        sample(97, Vec3::ZERO),
        sample(98, Vec3::X * 0.2),
        sample(99, Vec3::X * 0.6),
        sample(100, Vec3::new(1.2, 0.1, 0.0)),
    ]);
    let at = |tick, alpha| pose_at(&buf, RemoteRenderTime::new(tick, alpha), RATE).unwrap();
    let (inside, knot, outside) = (at(99, 0.999), at(100, 0.0), at(100, 0.001));
    assert_eq!(knot.position, Vec3::new(1.2, 0.1, 0.0), "the entry itself");
    assert!(
        inside.velocity.distance(knot.velocity) < 0.1,
        "{inside:?} vs {knot:?}"
    );
    assert!(knot.velocity.distance(outside.velocity) < 1e-4);
    assert!(knot.velocity.distance(Vec3::new(12.0, 2.0, 0.0)) < 1e-3);
    assert!(inside.position.distance(outside.position) < 0.03);
}

/// A stop is the same position held tick after tick: the tick it holds
/// from has no velocity, so the player eases into the stop over the tick
/// before it, never passes it, and stands there with no dead-reckoned
/// slide.
#[test]
fn a_stop_eases_to_zero_velocity_with_no_slide() {
    let speed = 10.0;
    let mut buf = buffer(&[
        sample(97, Vec3::X * -1.5),
        sample(98, Vec3::X * -1.0),
        sample(99, Vec3::X * -0.5),
        sample(100, Vec3::ZERO),
    ]);
    buf.hold_until(106);
    let at = |ticks| pose_at(&buf, RemoteRenderTime::from_ticks_f64(ticks), RATE).unwrap();

    // Running at full speed into the tick before the stop.
    assert!((at(98.5).velocity.x - speed).abs() < 1e-3);
    assert!((at(99.0).velocity.x - speed).abs() < 1e-3);
    // Easing over it: forward only, never past the stop, ending still.
    let poses: Vec<_> = (0..=100)
        .map(|step| at(99.0 + step as f64 / 100.0))
        .collect();
    assert!(
        poses
            .windows(2)
            .all(|pair| pair[1].position.x >= pair[0].position.x)
    );
    assert!(
        poses
            .iter()
            .all(|pose| pose.position.x <= 0.0 && pose.velocity.x >= 0.0)
    );
    assert_eq!(
        (poses[100].position, poses[100].velocity),
        (Vec3::ZERO, Vec3::ZERO)
    );
    assert!(
        poses[99].velocity.x < 0.1 * speed,
        "eased, not cut: {}",
        poses[99].velocity.x
    );
    // Stopped: exactly the stop, still, through the held ticks and past
    // them (the data has not arrived; the player has not moved either).
    for ticks in [100.5, 101.0, 103.5, 106.0, 107.5, 110.0] {
        let pose = at(ticks);
        assert_eq!(
            (pose.position, pose.velocity),
            (Vec3::ZERO, Vec3::ZERO),
            "at {ticks}"
        );
    }

    let mut bodies = bodies(&[
        body(99, Vec3::X * -0.5, Quat::IDENTITY),
        body(100, Vec3::ZERO, Quat::from_rotation_y(0.1)),
    ]);
    bodies.hold_until(102);
    let parked = body_at(&bodies, RemoteRenderTime::new(103, 0.5), RATE).unwrap();
    assert_eq!(parked.position, Vec3::ZERO);
    assert_eq!(parked.rotation, Quat::from_rotation_y(0.1));
    assert_eq!(
        (parked.linear_velocity, parked.angular_velocity),
        (Vec3::ZERO, Vec3::ZERO)
    );
}

#[test]
fn a_body_dead_reckons_on_its_last_tick() {
    let history = bodies(&[
        body(99, Vec3::X * -0.5, Quat::IDENTITY),
        body(100, Vec3::ZERO, Quat::from_rotation_y(0.1)),
    ]);
    let ahead = body_at(&history, RemoteRenderTime::new(102, 0.0), RATE).unwrap();
    assert!(ahead.position.distance(Vec3::X) < 1e-4);
    assert!(ahead.rotation.angle_between(Quat::from_rotation_y(0.3)) < 1e-4);
    let hz = RATE.hz() as f32;
    assert!(ahead.linear_velocity.distance(Vec3::X * 0.5 * hz) < 1e-3);
    assert!(ahead.angular_velocity.distance(Vec3::Y * 0.1 * hz) < 1e-3);
}

/// A burst lands newest first (replicon applies mutations that way): each
/// late tick lands in its place, and a tick already held is held once.
#[test]
fn player_history_orders_late_ticks() {
    let mut history = History::new(RATE);
    for tick in [12, 10, 11, 11] {
        let sample = sample(tick, Vec3::X * (tick - 10) as f32);
        history.record(tick, sample);
    }

    let ticks: Vec<_> = history.iter().map(|(tick, _)| tick).collect();
    assert_eq!(ticks, [10, 11, 12]);
    let poses = PlayerPoses::new([(&PlayerId(7), &history)], RATE);
    let held = poses
        .pose_at_time(PlayerId(7), RemoteRenderTime::new(11, 0.0))
        .unwrap();
    assert_eq!(held.position, Vec3::X);
}

#[test]
fn a_teleport_holds_until_its_tick_instead_of_sliding() {
    let a = sample(10, Vec3::ZERO);
    let b = sample(11, Vec3::new(50.0, 0.0, 0.0));
    let buf = buffer(&[a, b]);

    let mid = pose_at(&buf, RemoteRenderTime::new(10, 0.5), RATE).unwrap();
    assert_eq!(
        mid.position,
        Vec3::ZERO,
        "no 50 m slide across the teleport"
    );
    let end = pose_at(&buf, RemoteRenderTime::new(11, 0.0), RATE).unwrap();
    assert_eq!(end.position, b.position);
}

/// A teleport that overtook the tick before it: the late tick lands in
/// its place and the pose never lerps across the teleport.
#[test]
fn a_late_tick_from_before_a_teleport_is_not_lerped_across() {
    let at = |server_tick, x| PlayerSnapshot {
        server_tick,
        position: Vec3::X * x,
        ..default()
    };
    let mut history = History::new(RATE);
    for (tick, x) in [(10, 0.0), (12, 50.0), (11, 0.1), (13, 50.1)] {
        history.record(tick, at(tick, x));
    }
    let pose = |ticks| pose_at(&history, RemoteRenderTime::from_ticks_f64(ticks), RATE).unwrap();
    assert_eq!(pose(11.5).position, Vec3::X * 0.1);
    assert_eq!(pose(12.0).position, Vec3::X * 50.0);
}
