//! A rocket jump. The shove runs inside the movement step so client and server
//! agree and replay stays exact. The explosion visual is client-only eye candy.
#![allow(dead_code)]

use avian3d::prelude::*;
use bevy::prelude::*;
use bevy_ahoy::CharacterLook;
use bevy_netahoy::*;

use super::shared::WORLD_COLLISION_LAYER;

/// Our "fire rocket" bit. It lives in the high range that the library leaves alone
/// for games, so we use `from_bits_retain` to keep it.
pub const ROCKET_FIRE: AhoyButtons = AhoyButtons::from_bits_retain(1 << 16);

const ROCKET_EYE_HEIGHT: f32 = 0.6;
const ROCKET_SPEED: f32 = 42.0;
const ROCKET_LIFETIME_SECONDS: f32 = 1.35;
const ROCKET_MAX_DISTANCE: f32 = ROCKET_SPEED * ROCKET_LIFETIME_SECONDS;
const ROCKET_SPLASH_RADIUS: f32 = 4.0;
const ROCKET_IMPULSE_SPEED: f32 = 42.0;
const ROCKET_DEBUG_SECONDS: f32 = 0.85;
/// Seconds per fixed tick. Travel time gets rounded to a whole number of these.
const FIXED_DT: f32 = (1.0 / FIXED_TIMESTEP_HZ) as f32;

pub fn add_client_rockets(app: &mut App) {
    app.add_systems(
        FixedPreUpdate,
        spawn_rocket_visual.after(ClientNetAhoySystems::Predict),
    )
    .add_systems(Update, update_rocket_markers);
}

/// A rocket with travel time: on fire we raycast the blast point and schedule it a
/// few ticks out by command sequence, then push the player when that sequence lands.
pub(crate) fn rocket_jump(
    view: MoveView,
    command: &AhoyUserCmd,
    previous_buttons: AhoyButtons,
    world: &SpatialQuery,
    pending_shoves: &mut PendingShoves,
    velocity: &mut Vec3,
) {
    let firing = command.buttons.contains(ROCKET_FIRE);
    let was_firing = previous_buttons.contains(ROCKET_FIRE);
    if firing && !was_firing
        && let Some((explosion, distance)) =
            rocket_explosion_point(view.position, view.look, world)
    {
        // One rocket per player, so a new shot replaces the old. It lives in
        // PendingShoves, which rolls back, so client and server land it the same.
        let ticks = (distance / ROCKET_SPEED / FIXED_DT).round() as u32;
        pending_shoves.explosion = explosion;
        pending_shoves.due_sequence = command.sequence.wrapping_add(ticks);
        pending_shoves.armed = true;
    }

    // When it's due, push the player based on where they are right now, so a far
    // shot only launches you if you're still near the blast when it lands.
    if pending_shoves.armed && pending_shoves.due_sequence == command.sequence {
        *velocity += rocket_impulse(pending_shoves.explosion, view.position);
        pending_shoves.armed = false;
    }
}

/// Raycast from `position` along `look` for the blast point and its distance. Only
/// tests `WORLD_COLLISION_LAYER`, so it ignores players and matches on both sides.
fn rocket_explosion_point(position: Vec3, look: Vec2, world: &SpatialQuery) -> Option<(Vec3, f32)> {
    let rotation = Quat::from_euler(EulerRot::YXZ, look.x, look.y, 0.0);
    let direction = (rotation * Vec3::NEG_Z).normalize_or_zero();
    let ray_direction = Dir3::new(direction).ok()?;
    let origin = position + Vec3::Y * ROCKET_EYE_HEIGHT;

    let filter = SpatialQueryFilter::from_mask(WORLD_COLLISION_LAYER);
    let distance = world
        .cast_ray(origin, ray_direction, ROCKET_MAX_DISTANCE, true, &filter)
        .map(|hit| hit.distance)
        .unwrap_or(ROCKET_MAX_DISTANCE);
    Some((origin + direction * distance, distance))
}

fn rocket_impulse(explosion: Vec3, player: Vec3) -> Vec3 {
    let to_player = player - explosion;
    let distance = to_player.length();
    if distance >= ROCKET_SPLASH_RADIUS {
        return Vec3::ZERO;
    }
    let direction = if distance > 0.001 {
        to_player / distance
    } else {
        Vec3::Y
    };
    let falloff = 1.0 - distance / ROCKET_SPLASH_RADIUS;
    direction * (ROCKET_IMPULSE_SPEED * falloff)
}

#[derive(Component)]
struct RocketMarker {
    timer: Timer,
}

/// Spawns the explosion marker, one per shot. Client-only and never replayed, so it
/// fires once, redoing the raycast off the predicted player to match the real shove.
fn spawn_rocket_visual(
    input: Res<ClientInput>,
    mut fired: Local<bool>,
    player: Query<(&Transform, &CharacterLook), With<ClientPredictionKcc>>,
    spatial: SpatialQuery,
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    let pressed = input.buttons.contains(ROCKET_FIRE);
    let edge = pressed && !*fired;
    *fired = pressed;
    if !edge {
        return;
    }

    let Ok((transform, look)) = player.single() else {
        return;
    };
    let look = Vec2::new(look.yaw, look.pitch);
    // The marker shows up right away even though the real blast has travel time.
    // It only marks where the rocket will land, so that is fine for now.
    let Some((explosion, _distance)) = rocket_explosion_point(transform.translation, look, &spatial)
    else {
        return;
    };
    let origin = transform.translation + Vec3::Y * ROCKET_EYE_HEIGHT;

    let material = materials.add(StandardMaterial {
        base_color: Color::srgba(0.1, 0.9, 1.0, 0.7),
        alpha_mode: AlphaMode::Blend,
        ..default()
    });

    let segment = explosion - origin;
    let length = segment.length();
    if length > 0.001 {
        let direction = segment / length;
        let up = if direction.cross(Vec3::Y).length_squared() < 0.001 {
            Vec3::Z
        } else {
            Vec3::Y
        };
        let mut ray_transform = Transform::from_translation(origin + segment * 0.5);
        ray_transform.look_to(direction, up);
        commands.spawn((
            Name::new("rocket trail"),
            RocketMarker {
                timer: Timer::from_seconds(ROCKET_DEBUG_SECONDS, TimerMode::Once),
            },
            Mesh3d(meshes.add(Cuboid::new(0.045, 0.045, length))),
            MeshMaterial3d(material.clone()),
            ray_transform,
        ));
    }

    commands.spawn((
        Name::new("rocket explosion"),
        RocketMarker {
            timer: Timer::from_seconds(ROCKET_DEBUG_SECONDS, TimerMode::Once),
        },
        Mesh3d(meshes.add(Cuboid::new(0.35, 0.35, 0.35))),
        MeshMaterial3d(material),
        Transform::from_translation(explosion),
    ));
}

fn update_rocket_markers(
    mut commands: Commands,
    time: Res<Time>,
    mut markers: Query<(Entity, &mut RocketMarker, &mut Transform)>,
) {
    for (entity, mut marker, mut transform) in &mut markers {
        marker.timer.tick(time.delta());
        let remaining = marker.timer.remaining_secs() / ROCKET_DEBUG_SECONDS;
        transform.scale = Vec3::splat(remaining.max(0.15));
        if marker.timer.is_finished() {
            commands.entity(entity).despawn();
        }
    }
}
