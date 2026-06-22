//! The movement-shooter batteries: jump pads, rocket jumps, rocket splash. Read
//! this one file to see everything the library does beyond plain movement, and
//! hack away. Every effect runs inside the movement step ([`movement_extras`]),
//! so client prediction, replay, and the server all compose them identically.

use avian3d::prelude::*;
use bevy::prelude::*;

use crate::protocol::{AhoyButtons, AhoyUserCmd, FIXED_TIMESTEP_HZ};
use crate::server::ServerNetAhoySystems;

/// The movement-shooter collision-layer scheme. The library owns these so the
/// colliders the game spawns and the spatial queries here agree on one numbering.
pub const WORLD_COLLISION_LAYER: LayerMask = LayerMask(1 << 0);
pub const PLAYER_COLLISION_LAYER: LayerMask = LayerMask(1 << 1);
/// Jump-pad trigger sensors live here. The KCC ignores sensors, so they never
/// block movement; [`jump_pad`] queries this layer to detect them.
pub const JUMP_PAD_COLLISION_LAYER: LayerMask = LayerMask(1 << 2);

/// "Fire rocket" bit. Lives in the high range the library leaves for games, so we
/// use `from_bits_retain` to keep it.
pub const ROCKET_FIRE: AhoyButtons = AhoyButtons::from_bits_retain(1 << 16);

pub const ROCKET_EYE_HEIGHT: f32 = 0.6;
const ROCKET_SPEED: f32 = 42.0;
const ROCKET_LIFETIME_SECONDS: f32 = 1.35;
const ROCKET_MAX_DISTANCE: f32 = ROCKET_SPEED * ROCKET_LIFETIME_SECONDS;
const ROCKET_SPLASH_RADIUS: f32 = 4.0;
const ROCKET_IMPULSE_SPEED: f32 = 42.0;
/// Seconds per fixed tick. Travel time gets rounded to a whole number of these.
const FIXED_DT: f32 = (1.0 / FIXED_TIMESTEP_HZ) as f32;

const JUMP_PAD_VERTICAL_SPEED: f32 = 50.0;

/// Per-player scratch handed to every movement extra — a Quake-style POD blob for
/// effects that remember something across ticks (e.g. a rocket in flight). It rides
/// the rollback frame, so prediction and replay stay exact. Keep it minimal: add a
/// field only when an effect actually needs it.
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct MovementExtrasState {
    /// Sequence a scheduled blast is due to land. `armed == false` => idle.
    pub due_sequence: u32,
    /// Where the pending blast goes off (world space).
    pub explosion: Vec3,
    pub armed: bool,
    /// Where a blast went off this tick, for [`splash_other_players`] to push the
    /// rest of the field (`None` = nothing detonated). Self-knockback is already
    /// applied in [`rocket_jump`]; this is the server-only path for everyone else.
    pub detonated: Option<Vec3>,
}

/// Run every movement extra for one player, in one fixed order. Order is
/// load-bearing: [`jump_pad`] sets vertical speed and [`rocket_jump`] adds to it.
/// Called from the movement step, so client and server always compose them the same.
pub fn movement_extras(
    position: Vec3,
    look: Vec2,
    command: &AhoyUserCmd,
    previous_buttons: AhoyButtons,
    world: &SpatialQuery,
    state: &mut MovementExtrasState,
    velocity: &mut Vec3,
) {
    jump_pad(position, world, velocity);
    rocket_jump(position, look, command, previous_buttons, world, state, velocity);
}

/// While the player's center is in a jump-pad trigger, set (not add) upward speed.
/// Idempotent against the static pad layer, so it needs no edge detection.
fn jump_pad(position: Vec3, world: &SpatialQuery, velocity: &mut Vec3) {
    let filter = SpatialQueryFilter::from_mask(JUMP_PAD_COLLISION_LAYER);
    let mut on_pad = false;
    world.point_intersections_callback(position, &filter, |_| {
        on_pad = true;
        false // first hit is enough
    });
    if on_pad {
        velocity.y = JUMP_PAD_VERTICAL_SPEED;
    }
}

/// A rocket with travel time: on fire we raycast the blast point and schedule it a
/// few ticks out by command sequence, then push the player when that sequence lands.
fn rocket_jump(
    position: Vec3,
    look: Vec2,
    command: &AhoyUserCmd,
    previous_buttons: AhoyButtons,
    world: &SpatialQuery,
    state: &mut MovementExtrasState,
    velocity: &mut Vec3,
) {
    let firing = command.buttons.contains(ROCKET_FIRE);
    let was_firing = previous_buttons.contains(ROCKET_FIRE);
    if firing && !was_firing
        && let Some((explosion, distance)) = rocket_explosion_point(position, look, world)
    {
        // One rocket per player, so a new shot replaces the old. It lives in the
        // rollback blob, so client and server land it the same.
        let ticks = (distance / ROCKET_SPEED / FIXED_DT).round() as u32;
        state.explosion = explosion;
        state.due_sequence = command.sequence.wrapping_add(ticks);
        state.armed = true;
    }

    // When it's due, push the player based on where they are right now, so a far
    // shot only launches you if you're still near the blast when it lands.
    if state.armed && state.due_sequence == command.sequence {
        *velocity += rocket_impulse(state.explosion, position);
        // Mark it for the server to splash everyone else (the firer is already pushed).
        state.detonated = Some(state.explosion);
        state.armed = false;
    }
}

/// Raycast from `position` along `look` for the blast point and its distance. Only
/// tests [`WORLD_COLLISION_LAYER`], so it ignores players and matches on both sides.
/// Public so the example's client-side rocket visual can trace the same path.
pub fn rocket_explosion_point(
    position: Vec3,
    look: Vec2,
    world: &SpatialQuery,
) -> Option<(Vec3, f32)> {
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

/// Server-only: when a rocket detonates, push every *other* player caught in the
/// blast. Remote players aren't predicted, so this rides the snapshot stream down
/// to clients; the firer already got its predicted self-knockback in [`rocket_jump`].
pub fn splash_other_players(
    mut players: Query<(Entity, &Position, &mut LinearVelocity, &mut MovementExtrasState)>,
) {
    // Collect this tick's blasts first (clears the markers) so the inner loop can
    // borrow every player mutably.
    let blasts: Vec<(Entity, Vec3)> = players
        .iter_mut()
        .filter_map(|(entity, _, _, mut state)| state.detonated.take().map(|at| (entity, at)))
        .collect();

    for (firer, explosion) in blasts {
        for (entity, position, mut velocity, _) in &mut players {
            if entity != firer {
                velocity.0 += rocket_impulse(explosion, position.0);
            }
        }
    }
}

/// Adds the server-only [`splash_other_players`] pass after commands are applied.
/// The per-player effects themselves run inside the movement step, so they need no
/// plugin — add this on the server to let blasts shove the rest of the field.
pub struct MovementExtrasPlugin;

impl Plugin for MovementExtrasPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(
            FixedPreUpdate,
            splash_other_players.after(ServerNetAhoySystems::ApplyCommands),
        );
    }
}
