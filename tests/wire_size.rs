//! Prints the postcard size of everything on the wire, with mid-session values: postcard
//! varint-encodes integers, so a fresh-boot tick of 3 would understate the steady-state cost.
//! `cargo test --test wire_size -- --nocapture` shows the table.

use bevy::prelude::*;
use bevy_ahoy::moves::{Ground, MoveState};
use bevy_netahoy::{
    BodySnapshot, Buttons, EventId, OwnerSnapshot, PlayerId, PlayerSnapshot, RemoteEvent,
    RemoteFlags, SubtickFire, TickRate, UserCmd,
    debug::InputLead,
    testing::{self, LinkProfile, UserCmdPacket},
};
use playground::{ActiveRockets, Arsenal, Fired, Hit, Rocket, Shot, Weapon};

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

fn moves() -> MoveState {
    MoveState {
        position: Vec3::new(12.5, 1.7, -33.2),
        velocity: Vec3::new(4.2, -1.1, 6.8),
        orientation: Quat::from_euler(EulerRot::YXZ, 1.3, -0.2, 0.0),
        ground: Some(Ground {
            normal: Vec3::Y,
            friction: 0.6,
        }),
        since_jump: 431,
        ..default()
    }
}

fn snapshot(rocket_count: usize) -> OwnerSnapshot<Arsenal> {
    let rockets: Vec<Rocket> = (0..rocket_count)
        .map(|i| rocket(700_000 + i as u32))
        .collect();
    let rockets = ActiveRockets::try_from(rockets).unwrap();
    let mut snapshot = OwnerSnapshot::default();
    snapshot.server_tick = 720_000; // one hour of uptime at 20 Hz
    snapshot.moves = moves();
    snapshot.game = Arsenal {
        weapon: Weapon::Rockets,
        cooldown: 3,
        rockets,
    };
    let input_lead = Some(InputLead {
        min: 0,
        max: 2,
        samples: 20,
    });
    testing::set_ack(&mut snapshot, 700_123, Buttons::JUMP, input_lead);
    snapshot
}

#[test]
fn print_wire_sizes() {
    let idle = snapshot(0);
    let firing = snapshot(4);
    let worst = snapshot(16);

    // What every client receives per player, every tick; OwnerSnapshot goes owner-only.
    let sample = PlayerSnapshot {
        server_tick: idle.server_tick,
        position: idle.position(),
        look: idle.look(),
        move_flags: idle.move_flags(),
        flags: RemoteFlags::MOVE_INPUT,
    };
    // Replicon's userdata: netahoy's tick, once per replication message.
    let tick_userdata = size(&idle.server_tick);

    let cmd = UserCmd {
        sequence: 700_124,
        movement: Vec2::new(0.0, 1.0),
        look: Vec2::new(1.3, -0.2),
        buttons: Buttons::JUMP,
        fire: Some(SubtickFire {
            frac: 0.42,
            look: Vec2::new(1.3, -0.2),
            origin: None,
        }),
        seen_server_tick: 719_998,
        seen_alpha: 0.7,
    };
    // A tail as it runs: consecutive commands, the mouse turning (the wire is delta-coded).
    // The tail's cap is the same at any rate.
    let tail = LinkProfile::new(TickRate::new(60)).datagram_backup_cap;
    let packet = UserCmdPacket {
        commands: (0..tail as u32)
            .map(|i| UserCmd {
                sequence: cmd.sequence + i,
                look: cmd.look + Vec2::new(0.01, -0.002) * i as f32,
                fire: None,
                seen_server_tick: cmd.seen_server_tick + u64::from(i),
                ..cmd
            })
            .collect(),
    };

    let fired = Fired {
        id: EventId {
            owner: PlayerId(123456),
            sequence: 700_124,
        },
        start: Vec3::new(12.5, 2.3, -33.2),
        dir: Vec3::new(0.7, 0.1, -0.7),
        hit_distance: 87.3,
        weapon: Weapon::Rockets,
        seen_tick: 719_990,
    };
    let hit = Hit {
        id: fired.id,
        point: Vec3::new(73.6, 3.1, -94.3),
        direct_hit: Some(PlayerId(654321)),
        sequence: 700_130,
        seen_tick: 719_994,
        seen_alpha: 0.42,
        weapon: Weapon::Rockets,
    };

    for (name, bytes) in [
        ("OwnerSnapshot, 0 rockets", size(&idle)),
        ("OwnerSnapshot, 4 rockets", size(&firing)),
        ("OwnerSnapshot, 16 rockets", size(&worst)),
        ("PlayerSnapshot (all clients, per player)", size(&sample)),
        (
            "server tick userdata, per replication message",
            tick_userdata,
        ),
        ("UserCmd (with fire)", size(&cmd)),
        ("UserCmdPacket, the full datagram tail", size(&packet)),
        ("RemoteEvent, Fired", size(&RemoteEvent(Shot::Fired(fired)))),
        ("RemoteEvent, Hit", size(&RemoteEvent(Shot::Hit(hit)))),
        (
            "BodySnapshot",
            size(&BodySnapshot {
                tick: 720_000,
                position: Vec3::new(12.5, 1.7, -33.2),
                rotation: Quat::from_rotation_y(0.7),
                since_previous: 1,
            }),
        ),
    ] {
        println!("{bytes:>4} B  {name}");
    }
}
