//! The game-state POD, Quake style: one plain struct per player
//! ([`NetAhoyPlayerState`]) that rides [`AhoyPredictionFrame`] whole, stepped by
//! [`step_player_state`] right after the movement step on both peers. Rewind restores
//! the clone from the frame at the ack and replay re-runs the commands — no
//! ack bookkeeping, no separate rollback machinery. To extend, add fields to
//! the POD and logic to `step_player_state` (this is the `pmove`/`playerState_t`
//! pattern; fork it like it's `bg_pmove.c`).
//!
//! Rockets stay *closed-form*: one raycast at fire time fixes the whole path,
//! so position is a pure function of elapsed ticks. Detonation is the baked
//! world impact — unless the per-command sweep finds a player on the flight
//! segment first (poses sampled from lag-comp history at the command's seen
//! time, identically on both peers) and shortens the rocket to that direct
//! hit. A rocket is removed the moment it blasts; older frames hold their own
//! copies, so any rewind that needs to replay across the blast just
//! re-derives it. The live list also rides [`crate::protocol::AhoySnapshot`]
//! and stomps the frame's rockets on restore, so any residual disagreement
//! (clamped rewinds, pose gaps from packet loss) heals in one ack.
//!
//! [`AhoyPredictionFrame`]: crate::step::AhoyPredictionFrame

use avian3d::prelude::*;
use bevy::prelude::*;
use bevy_replicon::prelude::*;
use serde::{Deserialize, Serialize};

use crate::math::{LagCompensatedCast, LagCompensationHistory, RemoteRenderTime};
use crate::protocol::{
    sequence_is_newer, AhoyButtons, AhoyUserCmd, PlayerId, FIXED_TIMESTEP_HZ,
    PLAYER_CAPSULE_HALF_HEIGHT, PLAYER_CAPSULE_RADIUS,
};

/// Library-owned collision layers, so the colliders the game spawns and the
/// rocket raycast here agree on one numbering.
pub const WORLD_COLLISION_LAYER: LayerMask = LayerMask(1 << 0);
pub const PLAYER_COLLISION_LAYER: LayerMask = LayerMask(1 << 1);

/// "Fire rocket" bit, in the high range the library leaves for games.
pub const ROCKET_FIRE: AhoyButtons = AhoyButtons::from_bits_retain(1 << 16);
/// Weapon-select bits. Held-state selects (not a toggle edge), so a dropped
/// command can't strand a switch — the next command repeats the intent.
pub const EQUIP_FISTS: AhoyButtons = AhoyButtons::from_bits_retain(1 << 17);
pub const EQUIP_BAZOOKA: AhoyButtons = AhoyButtons::from_bits_retain(1 << 18);

/// Rocket tuning. Plain consts so client and server share them with no resource.
const EYE_HEIGHT: f32 = 0.6;
const SPEED: f32 = 42.0;
const LIFETIME_SECONDS: f32 = 1.35;
/// Public so game code can judge "was I in the blast?" (e.g. hit reactions).
pub const SPLASH_RADIUS: f32 = 4.0;
const IMPULSE_SPEED: f32 = 42.0;
/// A dynamic rigid body of this mass gets the same launch as a player; heavier
/// bodies move proportionally less (`rocket_impulse` is a player velocity, so we
/// treat it as momentum for a body of this mass and divide by the real mass).
/// Public so game code applying blasts to its own bodies matches the server.
pub const BLAST_REFERENCE_MASS: f32 = 40.0;
const MAX_DISTANCE: f32 = SPEED * LIFETIME_SECONDS;
/// In-flight cap, deep headroom: the 4-tick cooldown against the 27-tick
/// lifetime bounds legitimate in-flight rockets to 7. Overflow means an
/// abusive stream and the fire is declined — both peers run the same step,
/// so the decline predicts cleanly.
const MAX_ROCKETS: usize = 16;
/// Player-player separation, TF2 style: overlapping another player's cylinder
/// adds up to this much horizontal speed directly away, fading linearly to
/// zero at touch distance. Players are never hard-solid to each other — a
/// wall at 20 Hz mispredicts sharply (hit or didn't), a force mispredicts
/// smoothly.
pub const PLAYER_PUSH_SPEED: f32 = 6.0;

/// Refire delay: 16 ticks = 0.8 s at 20 Hz, the classic rocket cadence.
pub const ROCKET_COOLDOWN_TICKS: u16 = 4;
pub const ROCKET_AMMO_MAX: u16 = 20;
/// One rocket regenerates per second, so the movement demo never bricks dry.
pub const AMMO_REGEN_TICKS: u16 = FIXED_TIMESTEP_HZ as u16;

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
    /// The player the rocket struck directly, when the detonation came from a
    /// lag-compensated sweep instead of the baked world impact. Games read
    /// this for direct-hit damage; the splash impulse is unchanged either way.
    pub direct_hit: Option<PlayerId>,
}

/// One rocket. The raycast at fire time bakes `start`/`dir`/`hit_distance`/
/// `fuse_ticks` against the static world; the per-command player sweep in
/// [`step_player_state`] may later *shorten* `hit_distance`/`fuse_ticks` to a
/// direct hit — the path itself never changes.
/// `Default` is only the empty-slot filler for [`ActiveRockets`]'s array.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Rocket {
    pub owner: PlayerId,
    /// Owner's command sequence at the moment of firing.
    pub fired_sequence: u32,
    pub start: Vec3,
    pub dir: Vec3,
    pub hit_distance: f32,
    pub fuse_ticks: u32,
    /// Set by the sweep when the detonation is a direct hit on a player.
    pub direct_hit: Option<PlayerId>,
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
        // in step_player_state runs right after the rocket is pushed).
        let fuse_ticks = (hit_distance / SPEED * FIXED_TIMESTEP_HZ as f32).round() as u32;
        if fuse_ticks == 0 {
            println!(
                "Rocket fired at zero fuse: owner={:?} sequence={} pos={:?} look={:?} dist={}",
                owner,
                fired_sequence,
                position,
                look,
                hit_distance
            );
        }
        Self { owner, fired_sequence, start, dir, hit_distance, fuse_ticks, direct_hit: None }
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

/// What the player is holding. Selected via the `EQUIP_*` buttons inside the
/// shared step, so switches predict, replay, and replicate like any other
/// command-driven state.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum WeaponSlot {
    #[default]
    Fists,
    Bazooka,
}

/// The replicated weapon subset of the POD — the part the client cannot
/// re-derive from its own commands (the server may decline a fire the client
/// predicted), so it rides [`crate::protocol::AhoySnapshot`] and gets stomped
/// over the frame's prediction on every restore. One flat `Copy` struct so
/// "apply server truth" stays a single assignment no matter how many fields
/// games add to it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WeaponState {
    pub equipped: WeaponSlot,
    pub ammo: u16,
    /// Ticks until the next fire is accepted; counts down once per command.
    pub cooldown_ticks: u16,
    /// Ticks accumulated toward the next ammo regen.
    pub regen_ticks: u16,
    /// Total accepted fires, wrapping. Not gameplay: the presentation layer
    /// watermarks this to play fire effects exactly once across replays.
    pub shots_fired: u16,
}

impl Default for WeaponState {
    fn default() -> Self {
        Self {
            equipped: WeaponSlot::default(),
            ammo: ROCKET_AMMO_MAX,
            cooldown_ticks: 0,
            regen_ticks: 0,
            shots_fired: 0,
        }
    }
}

/// The live rocket list: fixed storage in memory (`Copy`, memcpy clones), a
/// count-bounded sequence on the wire (an empty bay costs one byte, not the
/// whole array). Dead slots are kept zeroed so derived `PartialEq` compares
/// only what's real — that's what the reconcile mismatch check relies on.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(into = "Vec<Rocket>", try_from = "Vec<Rocket>")]
pub struct ActiveRockets {
    rockets: [Rocket; MAX_ROCKETS],
    count: usize,
}

impl Default for ActiveRockets {
    fn default() -> Self {
        Self {
            rockets: [Rocket::default(); MAX_ROCKETS],
            count: 0,
        }
    }
}

impl ActiveRockets {
    /// The live rockets, e.g. for rendering in flight via [`Rocket::position_at`].
    pub fn as_slice(&self) -> &[Rocket] {
        &self.rockets[..self.count]
    }

    pub fn is_full(&self) -> bool {
        self.count == MAX_ROCKETS
    }

    fn push(&mut self, rocket: Rocket) {
        self.rockets[self.count] = rocket;
        self.count += 1;
    }

    /// Swap-remove (order carries no meaning), zeroing the vacated slot to
    /// keep dead slots default for `PartialEq`.
    fn remove(&mut self, index: usize) {
        self.count -= 1;
        self.rockets[index] = self.rockets[self.count];
        self.rockets[self.count] = Rocket::default();
    }
}

impl From<ActiveRockets> for Vec<Rocket> {
    fn from(active: ActiveRockets) -> Self {
        active.as_slice().to_vec()
    }
}

impl TryFrom<Vec<Rocket>> for ActiveRockets {
    type Error = String;

    fn try_from(rockets: Vec<Rocket>) -> Result<Self, Self::Error> {
        if rockets.len() > MAX_ROCKETS {
            return Err(format!("{} rockets exceeds the cap of {MAX_ROCKETS}", rockets.len()));
        }
        let mut active = Self::default();
        for rocket in rockets {
            active.push(rocket);
        }
        Ok(active)
    }
}

/// The per-player game POD. Lives as a component on the KCC entity on both
/// peers; [`crate::step::NetAhoyStepper`] steps it, copies it into every
/// prediction frame, and restores it whole on rewind. C-style on purpose:
/// fixed storage, `Copy`, no heap — every clone the netcode makes is a memcpy.
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct NetAhoyPlayerState {
    pub rockets: ActiveRockets,
    pub weapon: WeaponState,
}

/// Server-only outbox: what [`step_player_state`] fired and blasted, drained into
/// broadcasts by [`process_rocket_events`]. The client never creates it —
/// prediction and replay would push duplicates, and client visuals come from
/// the server events (or the game's own raycast).
#[derive(Resource, Default)]
pub struct NetAhoyPlayerEvents {
    pub fired: Vec<RocketFired>,
    pub hit: Vec<RocketHit>,
}

/// Advance one player's POD for one command, inside the movement step.
/// Fire on the rising edge, sweep rockets against lag-compensated player
/// poses, blast rockets whose fuse is due, apply self-knockback.
///
/// `previous_sequence` is the last command stepped before this one; the sweep
/// covers the whole gap so rockets can't tunnel through victims across lost
/// commands. `poses` is the pose history both peers keep — `None` skips
/// direct hits (rockets then only blast on their baked world impact).
pub fn step_player_state(
    state: &mut NetAhoyPlayerState,
    events: Option<&mut NetAhoyPlayerEvents>,
    owner: PlayerId,
    command: &AhoyUserCmd,
    previous_sequence: u32,
    previous_buttons: AhoyButtons,
    position: Vec3,
    look: Vec2,
    spatial: &SpatialQuery,
    poses: Option<&LagCompensationHistory>,
    velocity: &mut Vec3,
) {
    let mut events = events;

    // Weapon timers tick once per command, before the fire gate, so a 16-tick
    // cooldown yields exactly a 16-command refire period.
    let weapon = &mut state.weapon;
    if command.buttons.contains(EQUIP_FISTS) {
        weapon.equipped = WeaponSlot::Fists;
    }
    if command.buttons.contains(EQUIP_BAZOOKA) {
        weapon.equipped = WeaponSlot::Bazooka;
    }
    weapon.cooldown_ticks = weapon.cooldown_ticks.saturating_sub(1);
    if weapon.ammo < ROCKET_AMMO_MAX {
        weapon.regen_ticks += 1;
        if weapon.regen_ticks == AMMO_REGEN_TICKS {
            weapon.regen_ticks = 0;
            weapon.ammo += 1;
        }
    } else {
        weapon.regen_ticks = 0;
    }

    // Player-player separation. Self-only: each player pushes *themselves*
    // away in their own step (the other does the same in theirs), so no
    // cross-player writes — it stays inside the prediction contract. Poses
    // sample at the command's seen time exactly like the rocket sweep, so
    // both peers compute the identical nudge and it predicts rollback-free.
    // Ids are sorted before summing: f32 addition isn't associative and the
    // pose map's iteration order differs between peers.
    if let Some(poses) = poses {
        let seen_time = RemoteRenderTime::new(command.seen_server_tick, command.seen_alpha);
        let mut others: Vec<PlayerId> = poses.poses.keys().copied().collect();
        others.sort_by_key(|player_id| player_id.0);
        for other in others {
            if other == owner {
                continue;
            }
            let Some(pose) = poses.pose_at_time(other, seen_time) else {
                continue;
            };
            let delta = position - pose.position;
            if delta.y.abs() >= PLAYER_CAPSULE_HALF_HEIGHT * 2.0 {
                continue;
            }
            let flat = Vec2::new(delta.x, delta.z);
            let distance = flat.length();
            let touch = PLAYER_CAPSULE_RADIUS * 2.0;
            if distance >= touch {
                continue;
            }
            // Dead-center overlap (spawn stacks) still needs a deterministic
            // way out; +X is as good as any and identical on both peers.
            let direction = if distance > 0.001 { flat / distance } else { Vec2::X };
            let strength = PLAYER_PUSH_SPEED * (1.0 - distance / touch);
            *velocity += Vec3::new(direction.x, 0.0, direction.y) * strength;
        }
    }

    let firing = command.buttons.contains(ROCKET_FIRE);
    let was_firing = previous_buttons.contains(ROCKET_FIRE);
    if firing
        && !was_firing
        && state.weapon.equipped == WeaponSlot::Bazooka
        && state.weapon.cooldown_ticks == 0
        && state.weapon.ammo > 0
        && !state.rockets.is_full()
    {
        state.weapon.ammo -= 1;
        state.weapon.cooldown_ticks = ROCKET_COOLDOWN_TICKS;
        state.weapon.shots_fired = state.weapon.shots_fired.wrapping_add(1);
        let rocket = Rocket::fire(owner, command.sequence, position, look, spatial);
        state.rockets.push(rocket);
        if let Some(events) = events.as_deref_mut() {
            events.fired.push(rocket.fired_event());
        }
    }

    // Sweep each rocket's flight segment against the other players' hitboxes
    // as they stood at this command's seen time — the poses the shooter was
    // rendering when they built the command. Both peers sample the same pose
    // history, so a direct hit (and the early blast it causes) predicts. A
    // direct hit shortens the rocket to the impact: the fuse now lands on this
    // command and the detonation pass below blasts it at the struck point.
    if let Some(poses) = poses {
        let seen_time = RemoteRenderTime::new(command.seen_server_tick, command.seen_alpha);
        for index in 0..state.rockets.count {
            let rocket = &mut state.rockets.rockets[index];
            // Cover everything since the last stepped command (lost commands
            // leave gaps), but never before the rocket existed. A rocket fired
            // this command has a degenerate segment and skips — its first tick
            // of flight is swept by the next command.
            let from_sequence = if sequence_is_newer(previous_sequence, rocket.fired_sequence) {
                previous_sequence
            } else {
                rocket.fired_sequence
            };
            let from = rocket.position_at(from_sequence);
            let to = rocket.position_at(command.sequence);
            let segment = to - from;
            let length = segment.length();
            if length <= f32::EPSILON {
                continue;
            }

            if let Some(hit) = poses.raycast_hitboxes_at_time(LagCompensatedCast {
                server_time: seen_time,
                origin: from,
                direction: segment / length,
                max_distance: length,
                radius: PLAYER_CAPSULE_RADIUS,
                half_height: PLAYER_CAPSULE_HALF_HEIGHT,
                ignored_player: Some(owner),
            }) {
                rocket.hit_distance = from.distance(rocket.start) + hit.distance;
                rocket.fuse_ticks = command.sequence.wrapping_sub(rocket.fired_sequence);
                rocket.direct_hit = Some(hit.player_id);
            }
        }
    }

    // Blast rockets whose fuse lands on this command and drop them — old
    // frames keep their own copies, so a rewind that replays across the blast
    // just re-derives it from its frame's POD.
    // "At or before" instead of "exactly at": when the server loses the
    // fuse-tick command, the blast lands on the next command it does process
    // (a late blast tracks the client's prediction closer than no blast at all).
    let mut index = 0;
    while index < state.rockets.count {
        let rocket = state.rockets.rockets[index];
        if sequence_is_newer(rocket.detonation_sequence(), command.sequence) {
            index += 1;
            continue;
        }

        *velocity += rocket_impulse(rocket.detonation_point(), position);
        if let Some(events) = events.as_deref_mut() {
            events.hit.push(RocketHit {
                id: rocket.id(),
                point: rocket.detonation_point(),
                direct_hit: rocket.direct_hit,
            });
        }

        state.rockets.remove(index);
        // No index bump: re-examine the rocket just swapped into this slot.
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

#[cfg(test)]
mod tests {
    use super::*;

    fn rocket(fired_sequence: u32) -> Rocket {
        Rocket {
            owner: PlayerId(7),
            fired_sequence,
            start: Vec3::new(1.0, 2.0, 3.0),
            dir: Vec3::X,
            hit_distance: 10.0,
            fuse_ticks: 5,
            direct_hit: None,
        }
    }

    /// Wire roundtrip via the serde `into`/`try_from` conversions, after a
    /// removal: proves only live rockets cross the wire AND that dead slots
    /// stay default (try_from rebuilds from a default array, so roundtrip
    /// equality would fail if removal left garbage behind).
    #[test]
    fn active_rockets_roundtrip_after_removal() {
        let mut active = ActiveRockets::default();
        active.push(rocket(1));
        active.push(rocket(2));
        active.push(rocket(3));
        active.remove(0);

        let wire: Vec<Rocket> = active.into();
        assert_eq!(wire.len(), 2);
        assert_eq!(ActiveRockets::try_from(wire).unwrap(), active);

        assert!(ActiveRockets::try_from(vec![rocket(0); MAX_ROCKETS + 1]).is_err());
    }
}

/// Server-only: publish rocket events and push every *other* player caught in a
/// blast this tick. The firer already got self-knockback in [`step_player_state`].
pub fn process_rocket_events(
    mut commands: Commands,
    mut events: ResMut<NetAhoyPlayerEvents>,
    // Players are selected by PlayerId, NOT by the absence of RigidBody: the
    // KCC is a kinematic rigid body (bevy_ahoy's CharacterController requires
    // RigidBody::Kinematic), so a RigidBody filter would silently reroute
    // players into the mass-scaled loop below.
    mut players: Query<(&PlayerId, &Position, &mut LinearVelocity)>,
    mut bodies: Query<
        (&RigidBody, &Position, &mut LinearVelocity, &ComputedMass),
        Without<PlayerId>,
    >,
) {
    for message in events.fired.drain(..) {
        commands.server_trigger(ToClients {
            mode: SendMode::Broadcast,
            message,
        });
    }

    for hit in events.hit.drain(..) {
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
