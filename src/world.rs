//! Predicted, traveling rockets. The whole movement-extra story now: fire a
//! rocket, it flies, it knocks you (and others) around.
//!
//! A rocket is *closed-form*: one raycast at fire time fixes its whole path, so
//! its position and detonation are pure functions of how many ticks have elapsed
//! since it was fired. That's what makes it correct under the client's
//! rewind+replay without storing anything in the rollback frame — on rewind we
//! drop rockets fired after the ack and replay re-fires them.
//!
//! [`PredictWorld`] + [`step_rockets`] run on both peers (shared movement step);
//! the client also prunes on rewind, the server only steps forward. Same consts,
//! same math, so prediction matches the server with no correction.

use std::cmp::Ordering;
use std::collections::VecDeque;

use avian3d::prelude::*;
use bevy::prelude::*;
use serde::{Deserialize, Serialize};

use crate::protocol::{sequence_cmp, sequence_is_newer, AhoyButtons, AhoyUserCmd, PlayerId, FIXED_TIMESTEP_HZ};

/// Library-owned collision layers, so the colliders the game spawns and the
/// rocket raycast here agree on one numbering.
pub const WORLD_COLLISION_LAYER: LayerMask = LayerMask(1 << 0);
pub const PLAYER_COLLISION_LAYER: LayerMask = LayerMask(1 << 1);

/// "Fire rocket" bit, in the high range the library leaves for games.
pub const ROCKET_FIRE: AhoyButtons = AhoyButtons::from_bits_retain(1 << 16);

/// Rocket tuning. Plain consts so client and server share them with no resource.
const EYE_HEIGHT: f32 = 0.6;
const SPEED: f32 = 42.0;
const LIFETIME_SECONDS: f32 = 1.35;
const SPLASH_RADIUS: f32 = 4.0;
const IMPULSE_SPEED: f32 = 42.0;
const MAX_DISTANCE: f32 = SPEED * LIFETIME_SECONDS;
/// Backstop against an abusive fire stream; rockets normally retire on detonation.
const MAX_ROCKETS: usize = 64;

/// One rocket. Immutable after firing: the raycast bakes `start`/`dir`/
/// `hit_distance`/`fuse_ticks`, everything else derives from elapsed ticks.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Rocket {
    pub owner: PlayerId,
    /// Owner's command sequence at the moment of firing.
    pub fired_sequence: u32,
    pub start: Vec3,
    pub dir: Vec3,
    pub hit_distance: f32,
    pub fuse_ticks: u32,
}

impl Rocket {
    /// Raycast the path now and bake the rocket. Deterministic: same inputs +
    /// same static world => same rocket, which is what lets the sides agree.
    pub fn fire(
        owner: PlayerId,
        fired_sequence: u32,
        position: Vec3,
        look: Vec2,
        spatial: &SpatialQuery,
    ) -> Self {
        let (explosion, hit_distance) = rocket_explosion_point(position, look, spatial);
        let start = position + Vec3::Y * EYE_HEIGHT;
        let dir = (explosion - start).normalize_or_zero();
        // distance / speed = seconds, * HZ = ticks. Rounds to 0 point-blank, so a
        // shot into a wall detonates the same tick it's fired (the detonation pass
        // in step_rockets runs right after the rocket is pushed).
        let fuse_ticks = (hit_distance / SPEED * FIXED_TIMESTEP_HZ as f32).round() as u32;
        Self { owner, fired_sequence, start, dir, hit_distance, fuse_ticks }
    }

    pub fn detonation_sequence(&self) -> u32 {
        self.fired_sequence.wrapping_add(self.fuse_ticks)
    }

    pub fn detonation_point(&self) -> Vec3 {
        self.start + self.dir * self.hit_distance
    }

    /// Closed-form position at a command `sequence`, for rendering in flight.
    pub fn position_at(&self, sequence: u32) -> Vec3 {
        let elapsed = sequence.wrapping_sub(self.fired_sequence).min(self.fuse_ticks);
        let frac = elapsed as f32 / self.fuse_ticks.max(1) as f32;
        self.start + self.dir * (self.hit_distance * frac)
    }
}

/// Rockets in flight, in one place. Lives on both peers; the client prunes it on
/// rewind, the server only steps forward. On the server it holds every player's
/// rockets, so access is scoped by owner.
#[derive(Resource, Default)]
pub struct PredictWorld {
    rockets: VecDeque<Rocket>,
    /// Blasts that went off this tick: `(firer, point)`, for the server splash.
    pub detonations: Vec<(PlayerId, Vec3)>,
}

impl PredictWorld {
    /// Rewind hook (client only): drop `owner`'s rockets fired after `ack`; the
    /// replay re-fires them. Survivors re-derive their position from elapsed ticks.
    pub fn prune_after(&mut self, owner: PlayerId, ack: u32) {
        self.rockets
            .retain(|r| r.owner != owner || !sequence_is_newer(r.fired_sequence, ack));
    }

    /// Rockets and their current position at `sequence`, for rendering.
    pub fn iter_at(&self, sequence: u32) -> impl Iterator<Item = (&Rocket, Vec3)> {
        self.rockets.iter().map(move |r| (r, r.position_at(sequence)))
    }
}

/// Advance one player's rockets for one command, inside the movement step.
/// Fire on the rising edge, apply self-knockback when due, retire spent ones.
pub fn step_rockets(
    world: &mut PredictWorld,
    owner: PlayerId,
    command: &AhoyUserCmd,
    previous_buttons: AhoyButtons,
    position: Vec3,
    look: Vec2,
    spatial: &SpatialQuery,
    velocity: &mut Vec3,
) {
    let firing = command.buttons.contains(ROCKET_FIRE);
    let was_firing = previous_buttons.contains(ROCKET_FIRE);
    if firing && !was_firing {
        if world.rockets.len() == MAX_ROCKETS {
            world.rockets.pop_front();
        }
        world.rockets.push_back(Rocket::fire(owner, command.sequence, position, look, spatial));
    }

    // Detonate this owner's due rockets and retire spent ones. Collect blasts
    // first so the retain closure doesn't also borrow `world`.
    let mut blasts = Vec::new();
    world.rockets.retain(|rocket| {
        if rocket.owner != owner {
            return true;
        }
        match sequence_cmp(rocket.detonation_sequence(), command.sequence) {
            Ordering::Greater => true,                                    // in flight
            Ordering::Equal => { blasts.push(rocket.detonation_point()); false } // boom
            Ordering::Less => false,                                      // spent
        }
    });
    for point in blasts {
        *velocity += rocket_impulse(point, position);
        world.detonations.push((owner, point));
    }
}

/// Raycast from `position` along `look` for the blast point and its distance.
/// Only tests [`WORLD_COLLISION_LAYER`], so it ignores players and matches both
/// sides. Public so a client visual can trace the same path.
pub fn rocket_explosion_point(position: Vec3, look: Vec2, spatial: &SpatialQuery) -> (Vec3, f32) {
    let rotation = Quat::from_euler(EulerRot::YXZ, look.x, look.y, 0.0);
    let direction = rotation * Dir3::NEG_Z;
    let origin = position + Vec3::Y * EYE_HEIGHT;
    let filter = SpatialQueryFilter::from_mask(WORLD_COLLISION_LAYER);
    let distance = spatial
        .cast_ray(origin, direction, MAX_DISTANCE, true, &filter)
        .map(|hit| hit.distance)
        .unwrap_or(MAX_DISTANCE);
    (origin + direction * distance, distance)
}

/// Splash impulse on a player from a blast, with linear falloff to the radius.
pub fn rocket_impulse(explosion: Vec3, player: Vec3) -> Vec3 {
    let to_player = player - explosion;
    let distance = to_player.length();
    if distance >= SPLASH_RADIUS {
        return Vec3::ZERO;
    }
    let direction = if distance > 0.001 { to_player / distance } else { Vec3::Y };
    direction * (IMPULSE_SPEED * (1.0 - distance / SPLASH_RADIUS))
}

/// Server-only: push every *other* player caught in a blast this tick. The firer
/// already got its self-knockback in [`step_rockets`]; this rides the snapshot
/// stream down to clients (remote players aren't predicted).
pub fn splash_other_players(
    mut world: ResMut<PredictWorld>,
    mut players: Query<(&PlayerId, &Position, &mut LinearVelocity)>,
) {
    let blasts = std::mem::take(&mut world.detonations);
    for (firer, point) in blasts {
        for (player_id, position, mut velocity) in &mut players {
            if *player_id != firer {
                velocity.0 += rocket_impulse(point, position.0);
            }
        }
    }
}

/// Client-only: detonations are a server→splash channel, so drop them each tick.
pub fn clear_predicted_detonations(world: Option<ResMut<PredictWorld>>) {
    if let Some(mut world) = world {
        world.detonations.clear();
    }
}
