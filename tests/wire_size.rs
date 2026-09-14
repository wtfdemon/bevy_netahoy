//! Measures the postcard-serialized size of everything that rides the wire,
//! with realistic (non-zero, mid-session) values — postcard varints integers,
//! so a fresh-boot tick of 3 lies about steady-state cost. Run with
//! `cargo test --test wire_size -- --nocapture` to see the byte table.

use bevy::prelude::*;
use bevy_netahoy::player::*;
use bevy_netahoy::protocol::*;

fn size<T: serde::Serialize>(value: &T) -> usize {
    postcard::to_allocvec(value).unwrap().len()
}

fn rocket(fired_sequence: u32) -> Rocket {
    Rocket {
        owner: PlayerId(123456),
        fired_sequence,
        start: Vec3::new(12.5, 1.7, -33.2),
        dir: Vec3::new(0.7, 0.1, -0.7),
        hit_distance: 87.3,
        fuse_ticks: 21,
        direct_hit: None,
    }
}

fn snapshot(rocket_count: usize) -> AhoySnapshot {
    let rockets: Vec<Rocket> = (0..rocket_count)
        .map(|i| rocket(700_000 + i as u32))
        .collect();
    let rockets = ActiveRockets::try_from(rockets).unwrap();
    AhoySnapshot {
        server_tick: 720_000, // one hour of uptime at 20 Hz
        last_processed_sequence: 700_123,
        last_processed_buttons: AhoyButtons::JUMP,
        position: Vec3::new(12.5, 1.7, -33.2),
        velocity: Vec3::new(4.2, -1.1, 6.8),
        look: Vec2::new(1.3, -0.2),
        state: NetAhoyMoveState {
            grounded: true,
            crouching: false,
            mantle_height_left: None,
            crane_height_left: None,
        },
        player_state: NetAhoyPlayerState {
            rockets,
            weapon: WeaponState {
                equipped: WeaponSlot::Bazooka,
                ammo: 17,
                cooldown_ticks: 3,
                regen_ticks: 11,
                shots_fired: 431,
            },
        },
    }
}

#[test]
fn print_wire_sizes() {
    let idle = snapshot(0);
    let firing = snapshot(4);
    let worst = snapshot(16);

    // What every client receives per player; AhoySnapshot goes owner-only.
    let player_snapshot = size(&PlayerSnapshot(bevy_netahoy::math::RemoteSnapshotSample {
        server_tick: idle.server_tick,
        position: idle.position,
        velocity: idle.velocity,
        look: idle.look,
        state: idle.state,
        flags: bevy_netahoy::math::RemoteFlags::MOVE_INPUT,
    }));

    let cmd = AhoyUserCmd {
        sequence: 700_124,
        movement: Vec2::new(0.0, 1.0),
        look: Vec2::new(1.3, -0.2),
        buttons: AhoyButtons::JUMP,
        fire: Some(SubtickFire { frac: 0.42, look: Vec2::new(1.3, -0.2) }),
        seen_server_tick: 719_998,
        seen_alpha: 0.7,
    };
    let packet = AhoyUserCmdPacket { commands: vec![cmd; 8] };

    let fired = RocketFired {
        id: RocketId { owner: PlayerId(123456), fired_sequence: 700_124 },
        start: Vec3::new(12.5, 2.3, -33.2),
        dir: Vec3::new(0.7, 0.1, -0.7),
        hit_distance: 87.3,
    };
    let hit = RocketHit {
        id: fired.id,
        point: Vec3::new(73.6, 3.1, -94.3),
        direct_hit: Some(PlayerId(654321)),
    };

    for (name, bytes) in [
        ("AhoySnapshot, 0 rockets", size(&idle)),
        ("AhoySnapshot, 4 rockets", size(&firing)),
        ("AhoySnapshot, 16 rockets", size(&worst)),
        ("PlayerSnapshot (all clients, per player)", player_snapshot),
        ("AhoyUserCmd (with fire)", size(&cmd)),
        ("AhoyUserCmdPacket, 8 cmds", size(&packet)),
        ("RocketFired event", size(&fired)),
        ("RocketHit event", size(&hit)),
        ("BodySnapshot", size(&BodySnapshot {
            tick: 720_000,
            position: Vec3::new(12.5, 1.7, -33.2),
            rotation: Quat::from_rotation_y(0.7),
            linear_velocity: Vec3::new(4.2, -1.1, 6.8),
            angular_velocity: Vec3::new(0.1, 0.9, 0.0),
        })),
    ] {
        println!("{bytes:>4} B  {name}");
    }
}
