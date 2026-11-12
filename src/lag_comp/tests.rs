use super::*;
use crate::{poses::RemoteFlags, protocol::MoveFlags};

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

#[test]
fn hitbox_matches_movement_cylinder() {
    let center = Vec3::new(0.0, 1.0, 0.0);

    // Side-on: surface at exactly the radius.
    let side = ray_hitbox_distance(
        Vec3::new(5.0, 1.0, 0.0),
        Vec3::NEG_X,
        100.0,
        center,
        0.45,
        0.75,
    )
    .unwrap();
    assert!((side - (5.0 - 0.45)).abs() < 1e-3);

    // The cylinder tops out at +0.75: a side-on ray at +0.9 misses.
    let over_head = ray_hitbox_distance(
        Vec3::new(5.0, 1.9, 0.0),
        Vec3::NEG_X,
        100.0,
        center,
        0.45,
        0.75,
    );
    assert!(over_head.is_none());

    // Straight down onto the flat lid at y = center + 0.75.
    let top = ray_hitbox_distance(
        Vec3::new(0.2, 5.0, 0.0),
        Vec3::NEG_Y,
        100.0,
        center,
        0.45,
        0.75,
    )
    .unwrap();
    assert!((top - (5.0 - 1.75)).abs() < 1e-3);
}

/// A ray down -X along y = 1 at hitboxes standing on that line: the nearest one it reaches
/// that isn't ignored is the hit.
#[test]
fn a_ray_hits_the_nearest_hitbox_in_reach() {
    let near = buffer(&[sample(10, Vec3::Y)]);
    let far = buffer(&[sample(10, Vec3::new(-3.0, 1.0, 0.0))]);
    let (a, b) = (PlayerId(1), PlayerId(2));
    let cast = LagCompensatedCast {
        server_time: RemoteRenderTime::new(10, 0.0),
        origin: Vec3::new(5.0, 1.0, 0.0),
        direction: Vec3::NEG_X,
        max_distance: 100.0,
        radius: 0.45,
        half_height: 0.75,
        ignored_player: None,
    };

    let alone = PlayerPoses::new([(&a, &near)], RATE)
        .raycast_hitboxes_at_time(cast)
        .unwrap();
    assert_eq!(alone.player_id, a);
    assert!((alone.distance - 4.55).abs() < 1e-4, "{}", alone.distance);
    assert!(
        alone.position.distance(Vec3::new(0.45, 1.0, 0.0)) < 1e-4,
        "{}",
        alone.position
    );

    let poses = PlayerPoses::new([(&b, &far), (&a, &near)], RATE);
    assert_eq!(
        poses.raycast_hitboxes_at_time(cast).unwrap().player_id,
        a,
        "the nearer one"
    );
    let past_a = poses
        .raycast_hitboxes_at_time(LagCompensatedCast {
            ignored_player: Some(a),
            ..cast
        })
        .unwrap();
    assert_eq!(past_a.player_id, b);
    assert!((past_a.distance - 7.55).abs() < 1e-4, "{}", past_a.distance);

    let short = LagCompensatedCast {
        max_distance: 4.5,
        ..cast
    };
    assert_eq!(
        poses.raycast_hitboxes_at_time(short),
        None,
        "a is 4.55 m away"
    );
    let short_past_a = LagCompensatedCast {
        max_distance: 7.5,
        ignored_player: Some(a),
        ..cast
    };
    assert_eq!(
        poses.raycast_hitboxes_at_time(short_past_a),
        None,
        "b is 7.55 m away"
    );
}

#[test]
fn poses_iterate_by_player_id() {
    let one = buffer(&[sample(10, Vec3::X)]);
    let two = buffer(&[sample(10, Vec3::Y)]);
    let poses = PlayerPoses::new([(&PlayerId(2), &two), (&PlayerId(1), &one)], RATE);
    assert_eq!(
        poses.ids().collect::<Vec<_>>(),
        vec![PlayerId(1), PlayerId(2)]
    );
    let at = RemoteRenderTime::new(10, 0.0);
    assert_eq!(
        poses.pose_at_time(PlayerId(2), at).unwrap().position,
        Vec3::Y
    );
}
