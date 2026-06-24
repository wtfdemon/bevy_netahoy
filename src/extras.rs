//! The movement-shooter batteries: easy movement, jump pads, rocket jumps,
//! rocket splash. Read this one file to see everything the library does beyond
//! plain movement, and hack away. Every effect runs inside the movement step,
//! so client prediction, replay, and the server all compose them identically.

use std::f32::consts::{PI, TAU};

use avian3d::prelude::*;
use bevy::{prelude::*, time::Stopwatch};
use bevy_ahoy::input::AccumulatedInput;

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

/// Seconds per fixed tick. Travel time gets rounded to a whole number of these.
const FIXED_DT: f32 = (1.0 / FIXED_TIMESTEP_HZ) as f32;

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

/// Shape friendly movement inputs before Ahoy consumes them.
pub fn movement_pre_think(
    command: &AhoyUserCmd,
    previous_look: Vec2,
    config: &MovementExtrasPlugin,
    input: &mut AccumulatedInput,
    airborne: bool,
) {
    let jump_held = command.buttons.contains(AhoyButtons::JUMP);
    if config.auto_hop && jump_held {
        input.jumped = Some(Stopwatch::new());
    }

    let yaw_delta = wrap_angle(command.look.x - previous_look.x);
    let movement = input.last_movement.unwrap_or_default();
    let holding_forward = movement.y > 0.0;
    let manual_strafe = movement.x.abs() >= 0.1;
    if config.auto_strafe
        && jump_held
        && airborne
        && holding_forward
        && !manual_strafe
        && yaw_delta.abs() > config.auto_strafe_yaw_deadzone
    {
        input.last_movement = Some(
            Vec2::new(
                -yaw_delta.signum() * config.auto_strafe_strength.clamp(0.0, 1.0),
                0.0,
            )
            .clamp_length_max(1.0),
        );
    }
}

/// Run every post-KCC movement extra for one player, in one fixed order. Order
/// is load-bearing: [`jump_pad`] sets vertical speed and [`rocket_jump`] adds to it.
/// Called from the movement step, so client and server always compose them the same.
pub fn movement_extras(
    position: Vec3,
    look: Vec2,
    command: &AhoyUserCmd,
    previous_buttons: AhoyButtons,
    world: &SpatialQuery,
    config: &MovementExtrasPlugin,
    state: &mut MovementExtrasState,
    velocity: &mut Vec3,
) {
    jump_pad(position, world, config, velocity);
    rocket_jump(position, look, command, previous_buttons, world, config, state, velocity);
}

/// While the player's center is in a jump-pad trigger, set (not add) upward speed.
/// Idempotent against the static pad layer, so it needs no edge detection.
fn jump_pad(position: Vec3, world: &SpatialQuery, config: &MovementExtrasPlugin, velocity: &mut Vec3) {
    let filter = SpatialQueryFilter::from_mask(JUMP_PAD_COLLISION_LAYER);
    let mut on_pad = false;
    world.point_intersections_callback(position, &filter, |_| {
        on_pad = true;
        false // first hit is enough
    });
    if on_pad {
        velocity.y = config.jump_pad_vertical_speed;
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
    config: &MovementExtrasPlugin,
    state: &mut MovementExtrasState,
    velocity: &mut Vec3,
) {
    let firing = command.buttons.contains(ROCKET_FIRE);
    let was_firing = previous_buttons.contains(ROCKET_FIRE);
    if firing && !was_firing
        && let Some((explosion, distance)) = rocket_explosion_point(position, look, world, config)
    {
        // One rocket per player, so a new shot replaces the old. It lives in the
        // rollback blob, so client and server land it the same.
        let ticks = (distance / config.rocket_speed / FIXED_DT).round() as u32;
        state.explosion = explosion;
        state.due_sequence = command.sequence.wrapping_add(ticks);
        state.armed = true;
    }

    // When it's due, push the player based on where they are right now, so a far
    // shot only launches you if you're still near the blast when it lands.
    if state.armed && state.due_sequence == command.sequence {
        *velocity += rocket_impulse(state.explosion, position, config);
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
    config: &MovementExtrasPlugin,
) -> Option<(Vec3, f32)> {
    let rotation = Quat::from_euler(EulerRot::YXZ, look.x, look.y, 0.0);
    let direction = (rotation * Vec3::NEG_Z).normalize_or_zero();
    let ray_direction = Dir3::new(direction).ok()?;
    let origin = position + Vec3::Y * config.rocket_eye_height;

    let max_distance = config.rocket_max_distance();
    let filter = SpatialQueryFilter::from_mask(WORLD_COLLISION_LAYER);
    let distance = world
        .cast_ray(origin, ray_direction, max_distance, true, &filter)
        .map(|hit| hit.distance)
        .unwrap_or(max_distance);
    Some((origin + direction * distance, distance))
}

fn rocket_impulse(explosion: Vec3, player: Vec3, config: &MovementExtrasPlugin) -> Vec3 {
    let to_player = player - explosion;
    let distance = to_player.length();
    if distance >= config.rocket_splash_radius {
        return Vec3::ZERO;
    }
    let direction = if distance > 0.001 {
        to_player / distance
    } else {
        Vec3::Y
    };
    let falloff = 1.0 - distance / config.rocket_splash_radius;
    direction * (config.rocket_impulse_speed * falloff)
}

fn wrap_angle(angle: f32) -> f32 {
    (angle + PI).rem_euclid(TAU) - PI
}

/// Server-only: when a rocket detonates, push every *other* player caught in the
/// blast. Remote players aren't predicted, so this rides the snapshot stream down
/// to clients; the firer already got its predicted self-knockback in [`rocket_jump`].
pub fn splash_other_players(
    config: Res<MovementExtrasPlugin>,
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
                velocity.0 += rocket_impulse(explosion, position.0, &config);
            }
        }
    }
}

/// The movement-shooter tuning knobs, and the plugin that ships them. Add this on
/// the server to wire the server-only [`splash_other_players`] pass *and* publish
/// these values as a resource the movement step reads; `default()` is the shipped
/// game feel. The per-player effects run inside the movement step, so when a side
/// (e.g. the client predictor) skips the plugin the step falls back to `default()`.
#[derive(Resource, Clone, Copy, Debug)]
pub struct MovementExtrasPlugin {
    /// Holding jump refreshes Ahoy's jump buffer, so landing immediately hops.
    pub auto_hop: bool,
    /// Holding forward + jump adds strafe toward mouse yaw while airborne.
    pub auto_strafe: bool,
    pub auto_strafe_yaw_deadzone: f32,
    /// `1.0` is full side input; lower values are gentler.
    pub auto_strafe_strength: f32,
    /// Camera/muzzle height the rocket raycast fires from.
    pub rocket_eye_height: f32,
    pub rocket_speed: f32,
    pub rocket_lifetime_seconds: f32,
    pub rocket_splash_radius: f32,
    pub rocket_impulse_speed: f32,
    pub jump_pad_vertical_speed: f32,
}

impl Default for MovementExtrasPlugin {
    fn default() -> Self {
        Self {
            auto_hop: true,
            auto_strafe: true,
            auto_strafe_yaw_deadzone: 0.003,
            auto_strafe_strength: 1.0,
            rocket_eye_height: 0.6,
            rocket_speed: 42.0,
            rocket_lifetime_seconds: 1.35,
            rocket_splash_radius: 4.0,
            rocket_impulse_speed: 42.0,
            jump_pad_vertical_speed: 50.0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn jump_command(look_x: f32) -> AhoyUserCmd {
        AhoyUserCmd {
            look: Vec2::new(look_x, 0.0),
            buttons: AhoyButtons::JUMP,
            ..default()
        }
    }

    #[test]
    fn easy_movement_refreshes_jump_and_respects_manual_strafe() {
        let config = MovementExtrasPlugin::default();
        let mut input = AccumulatedInput {
            last_movement: Some(Vec2::Y),
            ..default()
        };

        movement_pre_think(&jump_command(-0.2), Vec2::ZERO, &config, &mut input, true);

        assert!(input.jumped.is_some());
        let assisted = input.last_movement.unwrap();
        assert!(assisted.x > 0.0);
        assert_eq!(assisted.y, 0.0);

        input.last_movement = Some(Vec2::new(-1.0, 1.0).clamp_length_max(1.0));
        movement_pre_think(&jump_command(-0.2), Vec2::ZERO, &config, &mut input, true);

        assert!(input.last_movement.unwrap().x < 0.0);
    }
}

impl MovementExtrasPlugin {
    /// How far a rocket travels before it expires. Derived, so retuning speed or
    /// lifetime can't leave a stale max distance behind.
    fn rocket_max_distance(&self) -> f32 {
        self.rocket_speed * self.rocket_lifetime_seconds
    }
}

impl Plugin for MovementExtrasPlugin {
    fn build(&self, app: &mut App) {
        app.insert_resource(*self).add_systems(
            FixedPreUpdate,
            splash_other_players.after(ServerNetAhoySystems::ApplyCommands),
        );
    }
}
