//! The example's game: rockets with rocket jumps, a hitscan pistol and a shield gate that stops
//! both. One [`Arsenal`] per player rides the netcode's [`PlayerState`], stepped after the move on
//! both peers.
//!
//! A rocket's path is closed-form: one raycast at fire time fixes it, so where it is is a
//! function of elapsed ticks. It blasts at the world impact, or sooner where the per-command
//! sweep finds a player on its path (poses from history at the command's seen time, the same
//! on both peers). The rockets in flight are player state: older frames keep their own copies,
//! so a rewind across a blast re-derives it, and a restore from the server's
//! [`bevy_netahoy::OwnerSnapshot`] overwrites them, so a disagreement heals in one round trip.
//!
//! The [`ShieldGate`] rises and falls on the server's timer. A step reads it through the game's
//! context, in its [`History`] at the command's seen time: a shot meets the gate as the shooter
//! saw it, on both peers.

use bevy::{
    ecs::entity::MapEntities,
    math::bounding::{Aabb3d, RayCast3d},
    prelude::*,
};
use bevy_ahoy::moves::MoveState;
use bevy_netahoy::{
    Buttons, EventId, History, LagCompensatedCast, PlayerGame, PlayerId, PlayerState, Recorded,
    RemoteRenderTime, Step, StepEvent, StepWorld, SubtickFire, TickRate, fire_position,
    sequence_is_newer,
};
use serde::{Deserialize, Serialize};

/// Held to select rockets: bit 2, the first the library leaves to the game. A held bit, not
/// an edge, so a lost command can't lose a switch: the next one repeats it.
pub const EQUIP_ROCKETS: Buttons = Buttons::from_bits_retain(1 << 2);
/// Held to select the pistol; see [`EQUIP_ROCKETS`].
pub const EQUIP_PISTOL: Buttons = Buttons::from_bits_retain(1 << 3);
/// Rocket speed (m/s): a flight lasts `hit_distance / ROCKET_SPEED` seconds.
pub const ROCKET_SPEED: f32 = 84.0;
const LIFETIME_SECONDS: f32 = 1.35;
/// Blast radius (m): the push falls off linearly to zero here.
pub const SPLASH_RADIUS: f32 = 4.0;
/// A rocket's radius against players (m), added to their hitbox on every side. The world
/// sees only a ray, so a rocket never blows up on a corner it visibly clears.
pub const ROCKET_GIRTH: f32 = 0.5;
const IMPULSE_SPEED: f32 = 42.0;
const MAX_DISTANCE: f32 = ROCKET_SPEED * LIFETIME_SECONDS;
/// Rockets in flight per player, over twice what the cooldown allows in a lifetime (7). A fire
/// past it is declined, the same on both peers.
const MAX_ROCKETS: usize = 16;
/// Rocket refire period (s).
pub const ROCKET_COOLDOWN_SECONDS: f64 = 0.2;
/// Pistol refire period (s).
pub const PISTOL_COOLDOWN_SECONDS: f64 = 0.3;
/// Speed (m/s) a bullet gives its victim, flat, away from where it hit.
pub const PISTOL_SHOVE_SPEED: f32 = 1.8;

/// The shield gate's center: across the right half of the range, between the spawn and the A/D
/// bot's lane.
pub const GATE_CENTER: Vec3 = Vec3::new(3.5, 1.5, -10.0);
/// The shield gate's full extents (m): a panel 5 m wide and 3 m high, facing along Z.
pub const GATE_SIZE: Vec3 = Vec3::new(5.0, 3.0, 0.1);
/// How long the gate stays down, then up (s).
pub const GATE_PERIOD_SECONDS: f64 = 2.0;

/// A panel that rises and falls on the server's timer. Up, it stops rockets and bullets; players
/// always walk through it, so it is game state, not a moving body. Both peers keep it in a
/// [`History`], and a step reads that at its command's seen time ([`Arsenal`]'s context).
#[derive(Component, Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ShieldGate {
    pub raised: bool,
}

impl Recorded for ShieldGate {}

impl ShieldGate {
    /// The server's timer: down, then up, [`GATE_PERIOD_SECONDS`] each.
    pub fn at_tick(tick: u64, rate: TickRate) -> Self {
        Self {
            raised: (tick / rate.ticks(GATE_PERIOD_SECONDS)) % 2 == 1,
        }
    }

    /// Whether `history` has the gate up at `time`: the entry in force, no blend.
    pub fn raised_at(history: &History<Self>, time: RemoteRenderTime) -> bool {
        history.sample(time).is_some_and(|gate| gate.from.raised)
    }
}

/// How far along `direction` (unit) from `origin` a shot meets the gate, within `max_distance`,
/// if the gate was up when the step's command saw it.
fn gate_distance(
    step: &Step<Arsenal>,
    origin: Vec3,
    direction: Vec3,
    max_distance: f32,
) -> Option<f32> {
    let gate = step.context().single().ok()?;
    if !ShieldGate::raised_at(gate, step.seen_time()) {
        return None;
    }
    let ray = RayCast3d::new(origin, Dir3::new(direction).ok()?, max_distance);
    ray.aabb_intersection_at(&Aabb3d::new(GATE_CENTER, GATE_SIZE / 2.0))
}

/// The weapon in hand, picked inside the step by the held `EQUIP_*` bit, so a switch predicts.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Weapon {
    #[default]
    Rockets,
    Pistol,
}

/// A rocket left or a bullet went off. It carries the whole flight, so a visual never reads
/// rocket state again.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Fired {
    pub id: EventId,
    /// The eye it left from.
    pub start: Vec3,
    pub dir: Vec3,
    /// Metres along `dir` to the world, or to the player a bullet hit.
    pub hit_distance: f32,
    pub weapon: Weapon,
    /// The firing command's seen tick. 0 for rockets (unused).
    pub seen_tick: u64,
}

/// A rocket blasted or a bullet landed; the server pushes players from it ([`blast`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Hit {
    pub id: EventId,
    pub point: Vec3,
    /// The player struck: a rocket's direct hit or a bullet's victim. A rocket's splash is the
    /// same either way.
    pub direct_hit: Option<PlayerId>,
    /// The command it landed on.
    pub sequence: u32,
    /// That command's seen tick.
    pub seen_tick: u64,
    /// That command's seen fraction past `seen_tick`.
    pub seen_alpha: f32,
    pub weapon: Weapon,
}

/// What one step made happen, read as a message: each once, however often its command steps.
#[derive(Message, Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub enum Shot {
    /// A rocket left, a bullet went off.
    Fired(Fired),
    /// A rocket blasted, a bullet landed.
    Hit(Hit),
}

impl StepEvent for Shot {
    fn id(&self) -> EventId {
        match self {
            Self::Fired(fired) => fired.id,
            Self::Hit(hit) => hit.id,
        }
    }
}

/// A rocket in flight, its path baked against the world when fired. [`ActiveRockets::sweep`]
/// may shorten `hit_distance` and `fuse_ticks` to a direct hit; the path never changes.
/// `Default` only fills [`ActiveRockets`]'s empty slots.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Rocket {
    pub owner: PlayerId,
    /// The owner's command that fired it.
    pub fired_sequence: u32,
    /// The eye it left from.
    pub start: Vec3,
    pub dir: Vec3,
    /// Metres along `dir` to the blast.
    pub hit_distance: f32,
    /// Commands from `fired_sequence` to the blast.
    pub fuse_ticks: u32,
    /// The player the sweep found on its path, if any.
    pub direct_hit: Option<PlayerId>,
}

impl Rocket {
    /// Raycasts the path from `eye` and bakes the rocket. `frac` is how far into its tick the
    /// click came: the fuse sheds the rest of that tick, already flown, while the path keeps
    /// its true start, so the first sweep still covers the muzzle.
    pub fn fire(
        owner: PlayerId,
        fired_sequence: u32,
        eye: Vec3,
        look: Vec2,
        frac: f32,
        world: &StepWorld,
        tick_rate: TickRate,
    ) -> Self {
        let (explosion, hit_distance) = rocket_explosion_point(eye, look, world);
        let dir = (explosion - eye).normalize_or_zero();
        // 0 point-blank: a shot into a wall blasts on the command that fired it, as `detonate`
        // runs after the push.
        let fuse_ticks = (hit_distance / ROCKET_SPEED * tick_rate.hz() as f32 - (1.0 - frac))
            .round()
            .max(0.0) as u32;
        // Both peers log every bake, replays included: one sequence across the logs shows
        // whether the eye or the world raycast disagreed.
        debug!(
            "rocket bake: seq {fired_sequence} owner {:?} eye ({:.3},{:.3},{:.3}) look ({:.4},{:.4}) frac {frac:.3} -> explosion ({:.3},{:.3},{:.3}) dist {hit_distance:.3} fuse {fuse_ticks}",
            owner.0, eye.x, eye.y, eye.z, look.x, look.y, explosion.x, explosion.y, explosion.z,
        );
        Self {
            owner,
            fired_sequence,
            start: eye,
            dir,
            hit_distance,
            fuse_ticks,
            direct_hit: None,
        }
    }

    /// The command it blasts on.
    pub fn detonation_sequence(&self) -> u32 {
        self.fired_sequence.wrapping_add(self.fuse_ticks)
    }

    /// The id its fire and its blast share.
    pub fn id(&self) -> EventId {
        EventId {
            owner: self.owner,
            sequence: self.fired_sequence,
        }
    }

    /// Its fire, as the event a visual flies it from.
    pub fn fired_event(&self) -> Fired {
        Fired {
            id: self.id(),
            start: self.start,
            dir: self.dir,
            hit_distance: self.hit_distance,
            weapon: Weapon::Rockets,
            seen_tick: 0,
        }
    }

    /// Where it blasts: the world impact, or the struck point after a direct hit.
    pub fn detonation_point(&self) -> Vec3 {
        self.start + self.dir * self.hit_distance
    }

    /// Closed-form position at a command `sequence`, for rendering in flight.
    pub fn position_at(&self, sequence: u32) -> Vec3 {
        let elapsed = sequence
            .wrapping_sub(self.fired_sequence)
            .min(self.fuse_ticks);
        let frac = elapsed as f32 / self.fuse_ticks.max(1) as f32;
        self.start + self.dir * (self.hit_distance * frac)
    }
}

/// A player's rockets in flight: a fixed array in memory, only the live ones on the wire. Dead
/// slots stay default, so the derived `PartialEq` (reconcile's check) sees only live ones.
#[derive(Clone, Copy, PartialEq, Serialize, Deserialize)]
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

// The live rockets only: the reconcile mismatch log prints the whole player.
impl std::fmt::Debug for ActiveRockets {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.debug_list().entries(self.as_slice()).finish()
    }
}

impl ActiveRockets {
    /// The rockets in flight, in no particular order.
    pub fn as_slice(&self) -> &[Rocket] {
        &self.rockets[..self.count]
    }

    /// At the cap: another fire is declined.
    pub fn is_full(&self) -> bool {
        self.count == MAX_ROCKETS
    }

    fn push(&mut self, rocket: Rocket) {
        self.rockets[self.count] = rocket;
        self.count += 1;
    }

    /// Swap-remove, defaulting the vacated slot for `PartialEq`.
    fn remove(&mut self, index: usize) {
        self.count -= 1;
        self.rockets[index] = self.rockets[self.count];
        self.rockets[self.count] = Rocket::default();
    }

    /// Sweeps each rocket's path since the last command against the other players and the gate
    /// as the shooter saw them, the same on both peers, so a direct hit predicts. A hit shortens
    /// the rocket to it, and [`Self::detonate`] blasts it on this command.
    pub fn sweep(&mut self, step: &Step<Arsenal>) {
        let seen_time = step.seen_time();
        for index in 0..self.count {
            let rocket = &mut self.rockets[index];
            // From the last stepped command (lost ones leave gaps) or the fire, whichever is
            // later. A rocket fired on this command has no segment yet: the next one sweeps it.
            let previous = step.previous_sequence();
            let from_sequence = if sequence_is_newer(previous, rocket.fired_sequence) {
                previous
            } else {
                rocket.fired_sequence
            };
            let from = rocket.position_at(from_sequence);
            let to = rocket.position_at(step.command().sequence);
            let segment = to - from;
            let length = segment.length();
            if length <= f32::EPSILON {
                continue;
            }

            let gate = gate_distance(step, from, segment / length, length);
            if let Some(hit) = step.poses().raycast_hitboxes_at_time(LagCompensatedCast {
                server_time: seen_time,
                origin: from,
                direction: segment / length,
                max_distance: gate.unwrap_or(length),
                radius: step.shape().radius + ROCKET_GIRTH,
                half_height: step.shape().half_height + ROCKET_GIRTH,
                ignored_player: Some(step.owner()),
            }) {
                rocket.hit_distance = from.distance(rocket.start) + hit.distance;
                rocket.fuse_ticks = step.command().sequence.wrapping_sub(rocket.fired_sequence);
                rocket.direct_hit = Some(hit.player_id);
            } else if let Some(distance) = gate {
                rocket.hit_distance = from.distance(rocket.start) + distance;
                rocket.fuse_ticks = step.command().sequence.wrapping_sub(rocket.fired_sequence);
            }
        }
    }

    /// Blasts and drops the rockets due by this command, pushing the shooter in `moves` away
    /// from each (the rocket jump).
    pub fn detonate(&mut self, moves: &mut MoveState, step: &mut Step<Arsenal>) {
        let position = moves.position;
        // Due by, not due on: if the server lost the fuse's command, a late blast is closer to
        // the client's prediction than none.
        let mut index = 0;
        while index < self.count {
            let rocket = self.rockets[index];
            if sequence_is_newer(rocket.detonation_sequence(), step.command().sequence) {
                index += 1;
                continue;
            }

            let hit = Hit {
                id: rocket.id(),
                point: rocket.detonation_point(),
                direct_hit: rocket.direct_hit,
                sequence: step.command().sequence,
                seen_tick: step.command().seen_server_tick,
                seen_alpha: step.command().seen_alpha,
                weapon: Weapon::Rockets,
            };
            moves.velocity += rocket_impulse(hit.point, position);
            step.emit(Shot::Hit(hit));

            self.remove(index);
            // No index bump: re-examine the rocket just swapped into this slot.
        }
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
            return Err(format!(
                "{} rockets exceeds the cap of {MAX_ROCKETS}",
                rockets.len()
            ));
        }
        let mut active = Self::default();
        for rocket in rockets {
            active.push(rocket);
        }
        Ok(active)
    }
}

/// The example's per-player game POD: the weapon in hand, its cooldown and the rockets in
/// flight. `Copy`, fixed storage, no heap: every clone the netcode makes is a memcpy.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize, MapEntities)]
pub struct Arsenal {
    /// What a click fires.
    pub weapon: Weapon,
    /// Commands until the next fire is accepted; counts down once per command.
    pub cooldown: u16,
    /// The rockets in flight.
    pub rockets: ActiveRockets,
}

impl PlayerGame for Arsenal {
    type Event = Shot;
    /// The gate's history: a step reads it at its command's seen time.
    type Context = Query<'static, 'static, &'static History<ShieldGate>>;

    fn step(player: &mut PlayerState<Self>, step: &mut Step<Self>) {
        let before = player.moves.position;
        step.pmove(&mut player.moves);
        step.separate(&mut player.moves);
        let PlayerState { moves, game } = player;
        game.select(step.command().buttons);
        // `fire` is the press itself, set even for a click released within the tick.
        if let Some(fire) = step.command().fire
            && game.cooldown == 0
        {
            match game.weapon {
                Weapon::Rockets => game.fire_rocket(fire, before, moves.position, step),
                Weapon::Pistol => game.fire_pistol(fire, before, moves.position, step),
            }
        }
        game.rockets.sweep(step);
        game.rockets.detonate(moves, step);
    }
}

impl Arsenal {
    /// The held `EQUIP_*` bit picks the weapon, then the cooldown ticks down: before the fire
    /// gate, so an n-tick cooldown is exactly an n-command refire period.
    fn select(&mut self, buttons: Buttons) {
        if buttons.contains(EQUIP_ROCKETS) {
            self.weapon = Weapon::Rockets;
        } else if buttons.contains(EQUIP_PISTOL) {
            self.weapon = Weapon::Pistol;
        }
        self.cooldown = self.cooldown.saturating_sub(1);
    }

    /// Bake a rocket from where the player was drawn ([`fire_position`] between `before` and
    /// `position`, the move's ends), unless the bay is full.
    fn fire_rocket(
        &mut self,
        fire: SubtickFire,
        before: Vec3,
        position: Vec3,
        step: &mut Step<Self>,
    ) {
        if self.rockets.is_full() {
            return;
        }
        self.cooldown = step.tick_rate().ticks(ROCKET_COOLDOWN_SECONDS) as u16;
        let eye_height = step.shape().eye_height;
        let world = step.world();
        let origin = fire_position(&fire, before, position, eye_height, &world);
        let mut rocket = Rocket::fire(
            step.owner(),
            step.command().sequence,
            origin + Vec3::Y * eye_height,
            fire.look,
            fire.frac,
            &world,
            step.tick_rate(),
        );
        // A rocket due on this command blasts with no sweep: it meets the gate here.
        if rocket.fuse_ticks == 0
            && let Some(distance) =
                gate_distance(step, rocket.start, rocket.dir, rocket.hit_distance)
        {
            rocket.hit_distance = distance;
        }
        self.rockets.push(rocket);
        step.emit(Shot::Fired(rocket.fired_event()));
    }

    /// Hitscan: the world ray and the gate cap the range, then the hitbox cast at the seen time
    /// picks the victim. No splash; the server shoves the victim ([`blast`]).
    fn fire_pistol(
        &mut self,
        fire: SubtickFire,
        before: Vec3,
        position: Vec3,
        step: &mut Step<Self>,
    ) {
        self.cooldown = step.tick_rate().ticks(PISTOL_COOLDOWN_SECONDS) as u16;
        let owner = step.owner();
        let command = *step.command();
        let shape = step.shape();
        let world = step.world();
        let base = fire_position(&fire, before, position, shape.eye_height, &world);
        let origin = base + Vec3::Y * shape.eye_height;
        let (_, world_distance) = rocket_explosion_point(origin, fire.look, &world);
        let rotation = Quat::from_euler(EulerRot::YXZ, fire.look.x, fire.look.y, 0.0);
        let direction = rotation * Vec3::NEG_Z;
        let world_distance =
            gate_distance(step, origin, direction, world_distance).unwrap_or(world_distance);
        let hit = step.poses().raycast_hitboxes_at_time(LagCompensatedCast {
            server_time: step.seen_time(),
            origin,
            direction,
            max_distance: world_distance,
            radius: shape.radius,
            half_height: shape.half_height,
            ignored_player: Some(owner),
        });
        let id = EventId {
            owner,
            sequence: command.sequence,
        };
        step.emit(Shot::Fired(Fired {
            id,
            start: origin,
            dir: direction,
            hit_distance: hit.as_ref().map_or(world_distance, |hit| hit.distance),
            weapon: Weapon::Pistol,
            seen_tick: command.seen_server_tick,
        }));
        if let Some(hit) = hit {
            step.emit(Shot::Hit(Hit {
                id,
                point: origin + direction * hit.distance,
                direct_hit: Some(hit.player_id),
                sequence: command.sequence,
                seen_tick: command.seen_server_tick,
                seen_alpha: command.seen_alpha,
                weapon: Weapon::Pistol,
            }));
        }
    }
}

/// Where a shot from the `origin` eye along `look` meets the world, and how far that is.
/// Players don't count. A client visual can trace it through [`StepWorld::live`].
pub fn rocket_explosion_point(origin: Vec3, look: Vec2, world: &StepWorld) -> (Vec3, f32) {
    let rotation = Quat::from_euler(EulerRot::YXZ, look.x, look.y, 0.0);
    let direction = rotation * Dir3::NEG_Z;
    let distance = world
        .cast_ray(origin, direction, MAX_DISTANCE, true)
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
    let direction = if distance > 0.001 {
        to_player / distance
    } else {
        Vec3::Y
    };
    direction * (IMPULSE_SPEED * (1.0 - distance / SPLASH_RADIUS))
}

/// Server only: pushes every other player caught in a blast this tick, and shoves a bullet's
/// victim. The shooter's own push is in the step ([`ActiveRockets::detonate`]).
pub fn blast(
    mut events: MessageReader<Shot>,
    mut players: Query<(&PlayerId, &mut PlayerState<Arsenal>)>,
) {
    for event in events.read() {
        let Shot::Hit(hit) = *event else {
            continue;
        };

        match hit.weapon {
            Weapon::Pistol => {
                for (player_id, mut state) in &mut players {
                    if hit.direct_hit != Some(*player_id) {
                        continue;
                    }
                    let delta = state.moves.position - hit.point;
                    let flat = Vec3::new(delta.x, 0.0, delta.z)
                        .try_normalize()
                        .unwrap_or(Vec3::X);
                    state.moves.velocity += flat * PISTOL_SHOVE_SPEED;
                }
            }
            Weapon::Rockets => {
                for (player_id, mut state) in &mut players {
                    if *player_id != hit.id.owner {
                        let impulse = rocket_impulse(hit.point, state.moves.position);
                        state.moves.velocity += impulse;
                    }
                }
            }
        }
    }
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

    /// Only live rockets cross the wire, and a removal leaves its slot default: the roundtrip
    /// rebuilds from a default array, so it would differ otherwise.
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

    /// Debug shows the live rockets only, not the empty slots.
    #[test]
    fn active_rockets_debug_lists_the_live_rockets() {
        let mut active = ActiveRockets::default();
        assert_eq!(format!("{active:?}"), "[]");
        active.push(rocket(1));
        assert_eq!(format!("{active:?}"), format!("[{:?}]", rocket(1)));
    }

    /// The server's reaction to the hits: a rocket's blast pushes every other player in reach
    /// by [`rocket_impulse`], and leaves the players out of reach and the shooter alone; a
    /// bullet shoves the player it hit away from it, flat, and no one else.
    #[test]
    fn a_blast_pushes_the_players_in_reach_and_a_bullet_shoves_its_victim() {
        use bevy::ecs::message::Messages;

        const SHOOTER: PlayerId = PlayerId(1);
        const NEAR: PlayerId = PlayerId(2);
        const FAR: PlayerId = PlayerId(3);

        let mut world = World::new();
        world.init_resource::<Messages<Shot>>();
        let blast = world.register_system(blast);
        let point = Vec3::ZERO;
        let players = [
            (SHOOTER, Vec3::new(1.0, 0.5, 0.0)),
            (NEAR, Vec3::new(0.0, 0.5, 2.0)),
            (FAR, Vec3::new(SPLASH_RADIUS + 0.5, 0.5, 0.0)),
        ];
        for (id, at) in players {
            world.spawn((id, PlayerState::<Arsenal>::at(at)));
        }
        let velocities = |world: &mut World| {
            let mut players = world.query::<(&PlayerId, &PlayerState<Arsenal>)>();
            let mut velocities: Vec<_> = players
                .iter(world)
                .map(|(id, state)| (*id, state.moves.velocity))
                .collect();
            velocities.sort_by_key(|(id, _)| id.0);
            velocities
        };
        let react = |world: &mut World, sequence: u32, hit: Hit| {
            let id = EventId {
                owner: SHOOTER,
                sequence,
            };
            let event = Shot::Hit(Hit {
                id,
                sequence,
                ..hit
            });
            world.write_message(event);
            world.run_system(blast).unwrap();
            velocities(world)
        };

        let rocket = Hit {
            point,
            weapon: Weapon::Rockets,
            ..default()
        };
        let pushed = rocket_impulse(point, players[1].1);
        assert!(pushed.length() > 1.0);
        let after = react(&mut world, 3, rocket);
        assert_eq!(
            after,
            [(SHOOTER, Vec3::ZERO), (NEAR, pushed), (FAR, Vec3::ZERO)]
        );

        let bullet = |(victim, at): (PlayerId, Vec3)| Hit {
            point: at - Vec3::new(0.5, 0.2, 0.0),
            direct_hit: Some(victim),
            weapon: Weapon::Pistol,
            ..default()
        };
        let shoved = Vec3::X * PISTOL_SHOVE_SPEED;
        let after = react(&mut world, 9, bullet(players[2]));
        assert_eq!(
            after,
            [(SHOOTER, Vec3::ZERO), (NEAR, pushed), (FAR, shoved)]
        );
    }
}
