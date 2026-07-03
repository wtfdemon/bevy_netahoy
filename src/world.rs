//! Predicted, traveling rockets. The whole movement-extra story now: fire a
//! rocket, it flies, it knocks you (and others) around.
//!
//! A rocket is *closed-form*: one raycast at fire time fixes its whole path, so
//! its position and detonation are pure functions of how many ticks have elapsed
//! since it was fired. That's what makes it correct under the client's
//! rewind+replay without storing anything in the rollback frame — on rewind we
//! drop rockets fired after the ack and replay re-fires them, and blasts
//! re-derive whenever a step crosses the fuse tick. Detonating never removes a
//! rocket (a later rewind may need to replay across it); rockets leave the
//! deque only through [`NetAhoyWorld::retire`], once the ack passes their
//! detonation. Deque + ack can always reconstruct any suffix of the timeline.
//!
//! [`NetAhoyWorld`] + [`step_world`] run on both peers (shared movement step);
//! the client also prunes on rewind, the server only steps forward. Same consts,
//! same math, so prediction matches the server with no correction.

use std::collections::VecDeque;

use avian3d::prelude::*;
use bevy::prelude::*;
use bevy_replicon::prelude::*;
use serde::{Deserialize, Serialize};

use crate::protocol::{
    sequence_is_newer, AhoyButtons, AhoySnapshot, AhoyUserCmd, PlayerId, FIXED_TIMESTEP_HZ,
};
use crate::server::{ServerCommandBuffer, ServerNetAhoySystems};

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
/// A dynamic rigid body of this mass gets the same launch as a player; heavier
/// bodies move proportionally less (`rocket_impulse` is a player velocity, so we
/// treat it as momentum for a body of this mass and divide by the real mass).
/// Public so game code applying blasts to its own bodies matches the server.
pub const BLAST_REFERENCE_MASS: f32 = 40.0;
const MAX_DISTANCE: f32 = SPEED * LIFETIME_SECONDS;
/// Backstop against an abusive fire stream; rockets normally retire on detonation.
const MAX_ROCKETS: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RocketId {
    pub owner: PlayerId,
    pub fired_sequence: u32,
}

/// Server event: a rocket was fired. Remote clients can use this for visuals.
#[derive(Event, Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct RocketFired {
    pub id: RocketId,
    pub start: Vec3,
    pub dir: Vec3,
}

/// Server event: a rocket detonated. The same value is used by server splash.
#[derive(Event, Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct RocketHit {
    pub id: RocketId,
    pub point: Vec3,
}

pub struct NetAhoyWorldServerPlugin;

impl Plugin for NetAhoyWorldServerPlugin {
    fn build(&self, app: &mut App) {
        app.add_server_event::<RocketFired>(Channel::Ordered)
            .add_server_event::<RocketHit>(Channel::Ordered)
            .init_resource::<NetAhoyWorld>()
            .add_systems(
                FixedPreUpdate,
                process_rocket_events.after(ServerNetAhoySystems::ApplyCommands),
            );
    }
}

pub struct NetAhoyWorldClientPlugin;

impl Plugin for NetAhoyWorldClientPlugin {
    fn build(&self, app: &mut App) {
        app.add_server_event::<RocketFired>(Channel::Ordered)
            .add_server_event::<RocketHit>(Channel::Ordered)
            .init_resource::<NetAhoyWorld>()
            .add_systems(
                FixedLast,
                (retire_acked_rockets, clear_predicted_rocket_events),
            );
    }
}

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
        // in step_world runs right after the rocket is pushed).
        let fuse_ticks = (hit_distance / SPEED * FIXED_TIMESTEP_HZ as f32).round() as u32;
        Self { owner, fired_sequence, start, dir, hit_distance, fuse_ticks }
    }

    pub fn detonation_sequence(&self) -> u32 {
        self.fired_sequence.wrapping_add(self.fuse_ticks)
    }

    pub fn id(&self) -> RocketId {
        RocketId {
            owner: self.owner,
            fired_sequence: self.fired_sequence,
        }
    }

    pub fn fired_event(&self) -> RocketFired {
        RocketFired {
            id: self.id(),
            start: self.start,
            dir: self.dir,
        }
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

/// Rockets in one place, kept until acked past their detonation. Lives on both
/// peers; the client drops unacked fires on rewind (replay re-fires them), the
/// server only steps forward. On the server it holds every player's rockets,
/// so access is scoped by owner.
#[derive(Resource)]
pub struct NetAhoyWorld {
    rockets: VecDeque<Rocket>,
    rockets_fired: Vec<RocketFired>,
    rockets_hit: Vec<RocketHit>,
}

impl Default for NetAhoyWorld {
    fn default() -> Self {
        Self {
            rockets: VecDeque::with_capacity(MAX_ROCKETS),
            rockets_fired: Vec::with_capacity(MAX_ROCKETS),
            rockets_hit: Vec::with_capacity(MAX_ROCKETS),
        }
    }
}

impl NetAhoyWorld {
    /// Rewind hook (client only): restore world-side prediction state to `ack`.
    /// Keeps fires at or before the ack; replay re-fires the rest.
    pub fn restore_world(&mut self, owner: PlayerId, ack: u32) {
        self.rockets.retain(|r| {
            if r.owner != owner {
                return true; // other players' rockets are out of scope
            }
            !sequence_is_newer(r.fired_sequence, ack)
        });
    }

    /// Drop `owner`'s rockets detonating at or before `seq` — the only way a
    /// rocket leaves the deque. On the client `seq` is the server ack, so any
    /// blast a replay can still cross stays re-derivable; the server calls it
    /// with its own processed sequence.
    pub fn retire(&mut self, owner: PlayerId, seq: u32) {
        self.rockets.retain(|r| {
            if r.owner != owner {
                return true; // other players' rockets are out of scope
            }
            sequence_is_newer(r.detonation_sequence(), seq)
        });
    }

    fn clear_transients(&mut self) {
        self.rockets_fired.clear();
        self.rockets_hit.clear();
    }
}

/// Advance one player's rockets for one command, inside the movement step.
/// Fire on the rising edge, apply self-knockback when due, retire spent ones.
pub fn step_world(
    netahoy_world: &mut NetAhoyWorld,
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
        if netahoy_world.rockets.len() == MAX_ROCKETS {
            netahoy_world.rockets.pop_front();
        }
        let rocket = Rocket::fire(owner, command.sequence, position, look, spatial);
        netahoy_world.rockets_fired.push(rocket.fired_event());
        netahoy_world.rockets.push_back(rocket);
    }

    // Blast this owner's rockets whose fuse lands exactly on this command.
    // Detonating is a pure read — the rocket stays put, so a later rewind can
    // replay across this tick and re-derive the same blast. Removal is
    // [`NetAhoyWorld::retire`]'s job, driven by the ack. Collected first so
    // the loop doesn't also borrow `netahoy_world` mutably.
    let mut blasts = Vec::new();
    for rocket in &netahoy_world.rockets {
        if rocket.owner == owner && rocket.detonation_sequence() == command.sequence {
            blasts.push(RocketHit {
                id: rocket.id(),
                point: rocket.detonation_point(),
            });
        }
    }
    for hit in blasts {
        *velocity += rocket_impulse(hit.point, position);
        netahoy_world.rockets_hit.push(hit);
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

/// Server-only: publish rocket events and push every *other* player caught in a
/// blast this tick. The firer already got self-knockback in [`step_world`].
fn process_rocket_events(
    mut commands: Commands,
    mut netahoy_world: ResMut<NetAhoyWorld>,
    // Players are selected by PlayerId, NOT by the absence of RigidBody: the
    // KCC is a kinematic rigid body (bevy_ahoy's CharacterController requires
    // RigidBody::Kinematic), so a RigidBody filter would silently reroute
    // players into the mass-scaled loop below.
    mut players: Query<(&PlayerId, &Position, &mut LinearVelocity)>,
    mut bodies: Query<
        (&RigidBody, &Position, &mut LinearVelocity, &ComputedMass),
        Without<PlayerId>,
    >,
    progress: Query<(&PlayerId, &ServerCommandBuffer)>,
) {
    // The server never rewinds, so its processed sequence retires directly —
    // a blasted rocket leaves the same tick, and rockets whose fuse tick was
    // lost with a dropped command get swept instead of lingering.
    for (player_id, command_buffer) in &progress {
        netahoy_world.retire(*player_id, command_buffer.last_processed_sequence);
    }

    for message in netahoy_world.rockets_fired.drain(..) {
        commands.server_trigger(ToClients {
            mode: SendMode::Broadcast,
            message,
        });
    }

    for hit in netahoy_world.rockets_hit.drain(..) {
        commands.server_trigger(ToClients {
            mode: SendMode::Broadcast,
            message: hit,
        });

        for (player_id, position, mut velocity) in &mut players {
            if *player_id != hit.id.owner {
                velocity.0 += rocket_impulse(hit.point, position.0);
            }
        }

        // Dynamic rigid bodies (e.g. a vehicle) get the same blast as a proper,
        // mass-scaled impulse. Body type is checked explicitly — ComputedMass
        // still holds collider-derived values on kinematic/static bodies, so
        // an inverse-mass guard alone doesn't exclude them.
        for (body, position, mut velocity, mass) in &mut bodies {
            let inverse_mass = mass.inverse();
            if !matches!(body, RigidBody::Dynamic) || inverse_mass <= 0.0 {
                continue;
            }
            velocity.0 +=
                rocket_impulse(hit.point, position.0) * (BLAST_REFERENCE_MASS * inverse_mass);
        }
    }
}

// TODO: This shit is stupid. these two functions should be one, and retire_acked_rockets
// doesn't query Option for NetAhoyWorld as it should. And also, why loop over every playerid
// on the client? There's only one playerid we care about for client rockets.

/// Client-only: retire rockets by the server ack. Runs every fixed tick, so
/// spent rockets leave even while prediction is accurate and no rewind ever
/// calls [`NetAhoyWorld::restore_world`].
fn retire_acked_rockets(
    mut netahoy_world: ResMut<NetAhoyWorld>,
    snapshots: Query<(&PlayerId, &AhoySnapshot)>,
) {
    for (player_id, snapshot) in &snapshots {
        if snapshot.last_processed_sequence != 0 {
            netahoy_world.retire(*player_id, snapshot.last_processed_sequence);
        }
    }
}

/// Client-only: transient rocket queues are server-owned, so drop predicted ones.
pub fn clear_predicted_rocket_events(netahoy_world: Option<ResMut<NetAhoyWorld>>) {
    if let Some(mut netahoy_world) = netahoy_world {
        netahoy_world.clear_transients();
    }
}
