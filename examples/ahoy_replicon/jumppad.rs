//! Jump-pad placement for the demo world. The movement effect itself lives in the
//! library (`bevy_netahoy::extras::jump_pad`); this just spawns the trigger sensor
//! it queries.
use avian3d::prelude::*;
use bevy::prelude::*;
use bevy_netahoy::JUMP_PAD_COLLISION_LAYER;

pub const JUMP_PAD_TRANSLATION: Vec3 = Vec3::new(8.0, 0.15, 7.0);
pub const JUMP_PAD_SIZE: Vec3 = Vec3::new(3.0, 0.3, 3.0);

const PLAYER_CAPSULE_HALF_HEIGHT: f32 = 0.75;
const JUMP_PAD_TRIGGER_HALF_EXTENTS: Vec3 =
    Vec3::new(JUMP_PAD_SIZE.x * 0.5, 0.3, JUMP_PAD_SIZE.z * 0.5);

/// The invisible sensor box that detects a player on a jump pad. The KCC ignores
/// sensors; `bevy_netahoy::extras::jump_pad` finds it with a point query. The visual
/// is the world render.
pub fn spawn_jump_pad_trigger(commands: &mut Commands, base: &Transform) {
    let center =
        base.translation + Vec3::Y * (JUMP_PAD_SIZE.y * 0.5 + PLAYER_CAPSULE_HALF_HEIGHT);
    let full = JUMP_PAD_TRIGGER_HALF_EXTENTS * 2.0;
    commands.spawn((
        Name::new("jump pad trigger"),
        Transform::from_translation(center),
        RigidBody::Static,
        Collider::cuboid(full.x, full.y, full.z),
        Sensor,
        CollisionLayers::new(JUMP_PAD_COLLISION_LAYER, LayerMask::NONE),
    ));
}
