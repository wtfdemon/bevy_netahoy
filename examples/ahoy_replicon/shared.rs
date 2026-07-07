//! Demo world, spawn points, and player cosmetics shared by the example
//! client and server. Not part of the bevy_netahoy library.
#![allow(dead_code)]

use avian3d::prelude::*;
use bevy::{prelude::*, state::app::StatesPlugin};
use bevy_ahoy::{prelude::*, CharacterLook};
use bevy_netahoy::{
    apply_debug_time_scale, AhoySnapshot, DebugTimeScale, NetAhoyProtocolPlugin, NetAhoyPlayerState,
    NetworkedPlayer, PlayerId, PlayerOwner, QueuedUserCmds, ServerCommandBuffer,
    FIXED_TIMESTEP_HZ, PLAYER_COLLISION_LAYER, WORLD_COLLISION_LAYER,
};
use bevy_replicon::prelude::*;

use ahoy_replicon::{
    BoardVehicle, Driver, HitScanAck, HitScanShot, Vehicle, VehicleRocketFired, VehicleState,
};

pub const SPAWN_POINT: Vec3 = Vec3::new(0.0, 2.2, 8.0);
pub const FLYING_TARGET_PLAYER_ID: u64 = 9_001;
pub const WALKING_TARGET_PLAYER_ID: u64 = 9_002;
pub struct ExampleSharedPlugin;

impl Plugin for ExampleSharedPlugin {
    fn build(&self, app: &mut App) {
        if !app.is_plugin_added::<StatesPlugin>() {
            app.add_plugins(StatesPlugin);
        }

        app.insert_resource(Time::<Fixed>::from_hz(FIXED_TIMESTEP_HZ))
            .init_resource::<DebugTimeScale>()
            .add_plugins((RepliconPlugins, NetAhoyProtocolPlugin))
            // On web it's all ReliableOrdered anyway (one IO layer, TCP), so the
            // channel barely matters here. Over UDP, prefer a fire button in UserCmd.
            .add_client_event::<HitScanShot>(Channel::Unordered)
            .add_server_event::<HitScanAck>(Channel::Unordered)
            // Vehicles: markers replicate down; boarding and the owner's
            // simulated pose stream up. Pose loss is fine (next tick supersedes).
            .replicate::<Vehicle>()
            .replicate::<Driver>()
            .add_client_event::<BoardVehicle>(Channel::Ordered)
            .add_client_event::<VehicleState>(Channel::Unreliable)
            // Fired by the driver, relayed by the server to everyone.
            .add_client_event::<VehicleRocketFired>(Channel::Ordered)
            .add_server_event::<VehicleRocketFired>(Channel::Ordered)
            .add_systems(Startup, apply_debug_time_scale);
    }
}

#[derive(Clone, Copy)]
pub struct WorldBox {
    pub name: &'static str,
    pub translation: Vec3,
    pub size: Vec3,
    pub rotation: Quat,
    pub color: Color,
}

pub fn world_boxes() -> [WorldBox; 8] {
    [
        // Too tall to land on with a jump (apex ~jump_height 1.8m < 2.4m top),
        // but jump + crane (reach 1.5m) climbs the face and mantle (hands reach
        // ~1.0m) pulls over the lip. Run at it, jump, hold Space.
        WorldBox {
            name: "crane + mantle block",
            translation: Vec3::new(4.0, 1.2, 6.0),
            size: Vec3::new(3.0, 2.4, 2.0),
            rotation: Quat::IDENTITY,
            color: Color::srgb(0.28, 0.50, 0.42),
        },
        // Tall, long wall to run alongside and tic-tac off of. Approach it
        // glancing (roughly parallel) while airborne — a head-on angle is
        // rejected by Ahoy's max_tac_cos.
        WorldBox {
            name: "tic tac wall",
            translation: Vec3::new(10.5, 4.0, 4.0),
            size: Vec3::new(0.6, 8.0, 12.0),
            rotation: Quat::IDENTITY,
            color: Color::srgb(0.50, 0.30, 0.35),
        },
        WorldBox {
            name: "floor",
            translation: Vec3::new(0.0, -0.2, 0.0),
            size: Vec3::new(34.0, 0.4, 34.0),
            rotation: Quat::IDENTITY,
            color: Color::srgb(0.18, 0.20, 0.22),
        },
        WorldBox {
            name: "surf ramp",
            translation: Vec3::new(0.0, 1.0, -2.0),
            size: Vec3::new(5.0, 0.35, 14.0),
            rotation: Quat::from_rotation_x(-0.36),
            color: Color::srgb(0.42, 0.58, 0.68),
        },
        WorldBox {
            name: "left bank",
            translation: Vec3::new(-7.0, 1.8, -6.0),
            size: Vec3::new(10.0, 0.35, 8.0),
            rotation: Quat::from_rotation_z(0.55),
            color: Color::srgb(0.30, 0.45, 0.55),
        },
        WorldBox {
            name: "right bank",
            translation: Vec3::new(7.0, 1.8, -6.0),
            size: Vec3::new(10.0, 0.35, 8.0),
            rotation: Quat::from_rotation_z(-0.55),
            color: Color::srgb(0.30, 0.45, 0.55),
        },
        WorldBox {
            name: "mantle block",
            translation: Vec3::new(-4.0, 0.8, 5.0),
            size: Vec3::new(3.0, 1.6, 2.0),
            rotation: Quat::IDENTITY,
            color: Color::srgb(0.38, 0.45, 0.30),
        },
        WorldBox {
            name: "reset platform",
            translation: Vec3::new(0.0, 0.45, 12.0),
            size: Vec3::new(5.0, 0.5, 4.0),
            rotation: Quat::IDENTITY,
            color: Color::srgb(0.45, 0.38, 0.30),
        },
    ]
}

pub fn spawn_world_colliders(commands: &mut Commands) {
    for world_box in world_boxes() {
        let transform = Transform {
            translation: world_box.translation,
            rotation: world_box.rotation,
            ..default()
        };

        commands.spawn((
            Name::new(world_box.name),
            transform,
            RigidBody::Static,
            Collider::cuboid(world_box.size.x, world_box.size.y, world_box.size.z),
            CollisionLayers::new(WORLD_COLLISION_LAYER, LayerMask::ALL),
        ));
    }
}

pub fn spawn_world_render(
    commands: &mut Commands,
    meshes: &mut Assets<Mesh>,
    materials: &mut Assets<StandardMaterial>,
) {
    for world_box in world_boxes() {
        commands.spawn((
            Name::new(world_box.name),
            Mesh3d(meshes.add(Cuboid::new(
                world_box.size.x,
                world_box.size.y,
                world_box.size.z,
            ))),
            MeshMaterial3d(materials.add(StandardMaterial {
                base_color: world_box.color,
                perceptual_roughness: 0.85,
                ..default()
            })),
            Transform {
                translation: world_box.translation,
                rotation: world_box.rotation,
                ..default()
            },
        ));
    }
}

pub fn player_controller() -> CharacterController {
    CharacterController {
        // World only — players are NOT solid to each other. A hard wall at
        // another player's pose mispredicts sharply (the peers disagree on
        // where that pose is); instead the shared step applies a TF2-style
        // separation push from lag-comp poses, which predicts cleanly. See
        // PLAYER_PUSH_SPEED in bevy_netahoy::player.
        filter: SpatialQueryFilter::from_mask(WORLD_COLLISION_LAYER),
        acceleration_hz: 10.0,
        air_acceleration_hz: 120.0,
        speed: 6.5,
        gravity: 23.0,
        friction_hz: 4.0,
        ..default()
    }
}

pub fn player_collision_layers() -> CollisionLayers {
    CollisionLayers::new(PLAYER_COLLISION_LAYER, LayerMask::ALL)
}

/// Spawn the server-side player entity for `client`. Used on join and when a
/// driver hops out of a vehicle (with the vehicle's velocity, for momentum).
pub fn spawn_player(
    commands: &mut Commands,
    client: Entity,
    player_id: u64,
    position: Vec3,
    velocity: Vec3,
) {
    commands.spawn((
        Name::new(format!("player {player_id}")),
        Replicated,
        NetworkedPlayer,
        PlayerId(player_id),
        AhoySnapshot::default(),
        PlayerOwner(client),
        ServerCommandBuffer::default(),
        QueuedUserCmds::default(),
        NetAhoyPlayerState::default(),
        CharacterLook::default(),
        player_controller(),
        Collider::cylinder(0.45, 1.5),
        player_collision_layers(),
        LinearVelocity(velocity),
        Transform::from_translation(position),
    ));
}

pub fn player_spawn_point(player_id: u64) -> Vec3 {
    let index = player_id.saturating_sub(1) as f32;
    SPAWN_POINT + Vec3::new(index * 1.4, 0.0, 0.0)
}

pub fn player_color(player_id: u64) -> Color {
    Color::hsv((player_id.wrapping_mul(137) % 360) as f32, 0.55, 0.9)
}
