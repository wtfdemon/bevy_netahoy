//! Wire types, replicated components, and the protocol plugin both peers add.

use std::{cmp::Ordering, marker::PhantomData, time::Duration};

use aeronet::transport::TransportSystems;
use avian3d::prelude::{Collider, CollisionLayers, LayerMask};
use bevy::prelude::*;
use bevy_ahoy::moves::MoveState;
use bevy_replicon::prelude::*;
use bitflags::bitflags;
use serde::{Deserialize, Serialize};

use crate::events::RemoteEvent;
use crate::game::PlayerGame;
use crate::history::{History, ReplicatedTicks, receive_replication_tick, replicate_history};
use crate::poses::RemoteFlags;

/// The library's layer for the level and every moving body: the only one a step's moves and
/// rays meet. Fixed, so the colliders a game spawns and the step agree on one numbering.
pub const WORLD_COLLISION_LAYER: LayerMask = LayerMask(1 << 0);
/// The library's layer for players' colliders, which a step's moves and rays pass through:
/// players meet each other in [`Step::separate`](crate::Step::separate) and in lag-compensated
/// casts against their pose histories instead.
pub const PLAYER_COLLISION_LAYER: LayerMask = LayerMask(1 << 1);
/// Why a player's collider can't be what `layers` make it, if it can't: a member of
/// [`WORLD_COLLISION_LAYER`] is level geometry to every step. `player` names it in the error.
pub(crate) fn player_layers_error(
    player: &str,
    collider: bool,
    layers: Option<&CollisionLayers>,
) -> Option<String> {
    let layers = layers.copied().unwrap_or_default();
    (collider && layers.memberships & WORLD_COLLISION_LAYER != LayerMask::NONE).then(|| {
        let default = if layers == CollisionLayers::default() {
            " (avian's default layers)"
        } else {
            ""
        };
        format!(
            "{player}'s collider is a member of WORLD_COLLISION_LAYER{default}, so every step \
             would meet it as level geometry: put players on PLAYER_COLLISION_LAYER only, e.g. \
             CollisionLayers::new(PLAYER_COLLISION_LAYER, LayerMask::ALL)"
        )
    })
}

/// Arrival history behind each [`InputLead`] report (s). Long enough to see
/// a jitter pattern, short enough to react within a second.
pub(crate) const INPUT_LEAD_WINDOW_SECONDS: f64 = 1.0;
/// Where a hard input-clock resync lands, on either peer: the freshest
/// command one tick early. Steady state is the client's nudge band.
pub(crate) const INPUT_LEAD_TARGET: i32 = 1;
/// The version of how netahoy's types cross the wire, part of the protocol hash: peers and
/// demos of another version refuse each other. Replicon hashes the types' names only, so bump
/// this with every change a name doesn't show: a field's type, order or encoding.
pub(crate) const WIRE_VERSION: u32 = 2;

/// How many fixed ticks a second both peers step: one command and one published pose per
/// player per tick. Part of the protocol hash, so peers at different rates refuse each other.
///
/// From [`Self::MIN_HZ`] to [`Self::MAX_HZ`] (10 to 1000 Hz).
#[derive(Resource, Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TickRate {
    hz: u32,
}

impl Default for TickRate {
    /// 20 Hz.
    fn default() -> Self {
        Self::new(20)
    }
}

impl TickRate {
    /// The slowest rate (Hz): a tick, and the input latency it adds, is 100 ms at most.
    pub const MIN_HZ: u32 = 10;

    /// The fastest rate (Hz). The windows capped in ticks (rollback, queue, replay) cover less
    /// time the faster it runs: 64 ms of rollback at 1000 Hz.
    pub const MAX_HZ: u32 = 1000;

    /// `hz` ticks a second. Panics outside [`Self::MIN_HZ`]..=[`Self::MAX_HZ`].
    pub const fn new(hz: u32) -> Self {
        assert!(
            hz >= Self::MIN_HZ,
            "TickRate::new: under 10 Hz (a tick rate is 10 to 1000 Hz)"
        );
        assert!(
            hz <= Self::MAX_HZ,
            "TickRate::new: over 1000 Hz (a tick rate is 10 to 1000 Hz)"
        );
        Self { hz }
    }

    /// Ticks a second.
    pub const fn hz(self) -> u32 {
        self.hz
    }

    /// One tick, in seconds.
    pub fn seconds(self) -> f64 {
        1.0 / f64::from(self.hz)
    }

    /// `seconds` as the nearest whole number of ticks.
    pub fn ticks(self, seconds: f64) -> u64 {
        (seconds * f64::from(self.hz)).round() as u64
    }

    /// `seconds` as whole ticks ([`Self::ticks`]), at most `cap`.
    ///
    /// Every window the library sizes covers time, so it means the same at any rate; what it
    /// costs (steps run at once, commands in one packet) is capped in ticks or commands, so a
    /// faster rate can't multiply the work or the bytes.
    pub fn capped_ticks(self, seconds: f64, cap: u64) -> u64 {
        self.ticks(seconds).min(cap)
    }
}

/// Every player's shape, the same on both peers: an upright cylinder, the player's position at
/// its center, and the eye on its axis. The push between players and the casts at them use it;
/// give the players' colliders [`Self::collider`] so the move does too.
#[derive(Resource, Clone, Copy, Debug, PartialEq)]
pub struct PlayerShape {
    /// The cylinder's radius (m).
    pub radius: f32,
    /// Half the cylinder's height (m).
    pub half_height: f32,
    /// How far above the player's position its eye sits (m): fires leave from there.
    pub eye_height: f32,
}

impl Default for PlayerShape {
    /// 0.45 m wide, 1.5 m tall, the eye 0.6 m above the center.
    fn default() -> Self {
        Self {
            radius: 0.45,
            half_height: 0.75,
            eye_height: 0.6,
        }
    }
}

impl PlayerShape {
    /// The cylinder as a collider, for a player's entity.
    pub fn collider(&self) -> Collider {
        Collider::cylinder(self.radius, self.half_height * 2.0)
    }
}

/// What both peers register: the replicated components and the events, for the game `G`,
/// the tick rate, as [`TickRate`] and `Time<Fixed>`, and the [`PlayerShape`].
///
/// The tick rate is set here, never on `Time<Fixed>`: building panics if the app's
/// `Time<Fixed>` already steps at another rate (Bevy's default excepted), and finishing panics
/// if anything changed it since. `RepliconPlugins` must be added before it.
pub struct NetAhoyProtocolPlugin<G> {
    /// The rate both peers step at.
    pub tick_rate: TickRate,
    /// Every player's shape, part of the protocol hash.
    pub player_shape: PlayerShape,
    game: PhantomData<G>,
}

impl<G> NetAhoyProtocolPlugin<G> {
    /// The protocol at `tick_rate`, for players of the default [`PlayerShape`].
    pub fn new(tick_rate: TickRate) -> Self {
        Self {
            tick_rate,
            player_shape: PlayerShape::default(),
            game: PhantomData,
        }
    }

    /// The same protocol for players of `shape`.
    pub fn with_player_shape(self, shape: PlayerShape) -> Self {
        Self {
            player_shape: shape,
            ..self
        }
    }
}

impl<G> Default for NetAhoyProtocolPlugin<G> {
    fn default() -> Self {
        Self::new(TickRate::default())
    }
}

impl<G: PlayerGame> Plugin for NetAhoyProtocolPlugin<G> {
    fn build(&self, app: &mut App) {
        assert!(
            app.world().contains_resource::<ProtocolHasher>(),
            "NetAhoyProtocolPlugin registers its types with replicon: add RepliconPlugins before it"
        );
        let fixed = Time::<Fixed>::from_hz(f64::from(self.tick_rate.hz()));
        if let Some(set) = app
            .world()
            .get_resource::<Time<Fixed>>()
            .map(Time::timestep)
            && set != fixed.timestep()
            && set != Time::<Fixed>::default().timestep()
        {
            panic!(
                "Time<Fixed> steps {set:?}, but the tick rate is {} Hz: set the rate with \
                 NetAhoyProtocolPlugin::new(TickRate::new(hz)), not on Time<Fixed>",
                self.tick_rate.hz()
            );
        }
        let mut hasher = app.world_mut().resource_mut::<ProtocolHasher>();
        hasher.add_custom(("bevy_netahoy wire", WIRE_VERSION));
        hasher.add_custom(self.tick_rate);
        let PlayerShape {
            radius,
            half_height,
            eye_height,
        } = self.player_shape;
        hasher.add_custom((
            "player shape",
            [radius, half_height, eye_height].map(f32::to_bits),
        ));
        app.insert_resource(self.tick_rate)
            .insert_resource(self.player_shape)
            .insert_resource(fixed)
            .replicate::<ServerPlayer>()
            .replicate::<PlayerId>()
            .replicate::<OwnerSnapshot<G>>()
            .init_resource::<ReplicatedTicks>()
            .add_observer(receive_replication_tick)
            .add_client_event::<JoinRequest>(Channel::Ordered)
            .add_server_event::<JoinAccepted>(Channel::Ordered)
            .add_server_event::<RejoinToken>(Channel::Ordered)
            .add_client_event::<UserCmdPacket>(Channel::Unreliable)
            .add_server_event::<RemoteEvent<G::Event>>(Channel::Ordered)
            // Every aeronet flush sends at least one packet (its acks), and everything worth
            // sending is written on a fixed tick: flush once per tick, not per frame. A game's
            // writes outside the fixed loop wait for the next tick's flush.
            .configure_sets(PostUpdate, TransportSystems::Flush.run_if(fixed_tick_ran));
        // Kept in a History on both peers, every tick of a burst included.
        replicate_history::<PlayerSnapshot>(app);
        replicate_history::<BodySnapshot>(app);
    }

    fn finish(&self, app: &mut App) {
        fixed_tick_rate(app.world(), "NetAhoyProtocolPlugin");
    }
}

/// The app's [`TickRate`], for `plugin` to step at once every plugin has built. Panics if
/// [`NetAhoyProtocolPlugin`] is missing, or if `Time<Fixed>` doesn't step at the rate.
pub(crate) fn fixed_tick_rate(world: &World, plugin: &str) -> TickRate {
    let rate = *world
        .get_resource::<TickRate>()
        .unwrap_or_else(|| panic!("{plugin} needs NetAhoyProtocolPlugin in the app"));
    let timestep = world.resource::<Time<Fixed>>().timestep();
    let expected = Time::<Fixed>::from_hz(f64::from(rate.hz())).timestep();
    assert!(
        timestep == expected,
        "Time<Fixed> steps {timestep:?}, but the tick rate is {} Hz ({expected:?}): set the rate \
         with NetAhoyProtocolPlugin::new(TickRate::new(hz)), not on Time<Fixed>",
        rate.hz()
    );
    rate
}

/// Whether a fixed tick ran since this condition last ran.
fn fixed_tick_ran(fixed: Res<Time<Fixed>>, mut last: Local<Duration>) -> bool {
    let elapsed = fixed.elapsed();
    core::mem::replace(&mut *last, elapsed) != elapsed
}

/// A player: the server's entity, replicated to every client, and what every peer finds players
/// by. A [`PlayerOwner`](crate::PlayerOwner) brings it.
#[derive(Component, Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq)]
pub struct ServerPlayer;

/// A player's id, the same on every peer: what events, histories and the push order players by.
#[derive(Component, Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct PlayerId(pub u64);

/// A player as every viewer sees it: the pose the server published this tick. Kept in the
/// player's [`History`] on both peers, which lag compensation, the push and remote rendering
/// all sample.
///
/// No tick rides the wire: the history keys it by the tick of the replication message that
/// carried it. A published pose is final: an input rollback re-steps the server's timeline, never
/// what it already sent.
#[derive(Component, Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq)]
#[require(History<PlayerSnapshot>)]
pub struct PlayerSnapshot {
    /// Not on the wire: its [`History`] key, the tick of the replication message that carried it.
    #[serde(skip)]
    pub server_tick: u64,
    /// Where the player stands.
    pub position: Vec3,
    /// Where it looks: yaw and pitch, radians.
    pub look: Vec2,
    /// Whether it stands on the ground and crouches.
    pub move_flags: MoveFlags,
    /// What else viewers draw it by.
    pub flags: RemoteFlags,
}

/// What a client sends the server on connecting: who it was before, and what lets it in.
#[derive(Event, Serialize, Deserialize, Clone, Debug, Default)]
pub struct JoinRequest {
    /// The [`RejoinToken`] from this client's previous connection, if any:
    /// the server drops that connection if it's somehow still open.
    pub rejoin: Option<RejoinToken>,
    /// Whatever [`crate::reconnect::JoinCredential`] holds: opaque here, the
    /// game's server decides what it needs to let someone in.
    pub ticket: String,
}

/// Issued in reply to every [`JoinRequest`]; the client presents it when it
/// reconnects (with [`NetAhoyClientPlugin`](crate::NetAhoyClientPlugin)).
#[derive(Event, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RejoinToken(pub u64);

/// Which player a client owns, for its [`OwnPlayerId`](crate::OwnPlayerId).
/// [`NetAhoyServerPlugin`](crate::NetAhoyServerPlugin) sends it when the game spawns a player
/// with that client's [`PlayerOwner`](crate::PlayerOwner).
#[derive(Event, Serialize, Deserialize, Clone, Copy, Debug)]
pub(crate) struct JoinAccepted {
    /// The player the client owns.
    pub player_id: PlayerId,
}

bitflags! {
    /// Buttons for one command. JUMP and CROUCH are the library's (the stock move reads
    /// them); bits 2-31 are the game's, unnamed here and carried untouched (e.g. weapon
    /// selects via from_bits_retain).
    #[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
    pub struct Buttons: u32 {
        /// Jump: held, it jumps on every landing; a press waits a few ticks for one.
        const JUMP = 1 << 0;
        /// Crouch, while held.
        const CROUCH = 1 << 1;
    }
}

/// A fire press, sub-tick. `Some` on a command is the rising edge, sampled per render frame so
/// a click can't fall between ticks.
///
/// `origin` is the body the player was looking through when they clicked. It can't be derived:
/// the command's own move sits a tick of own motion ahead of the drawn body, and corrections
/// move the drawn body further still. The step bounds it against that derived point
/// ([`fire_position`](crate::fire_position)), so both peers shoot from the same bits.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
pub struct SubtickFire {
    /// How far into the tick's window the click came, from the fixed clock's overstep.
    pub frac: f32,
    /// Look angles at the click, not the tick-boundary sample.
    pub look: Vec2,
    /// The drawn body at the click (the body position, not the eye). `None`:
    /// shoot from the derived point (bots and recordings draw nothing).
    pub origin: Option<Vec3>,
}

impl SubtickFire {
    /// The trust boundary for a fire: a finite fraction clamped to [0, 1], the pitch to the
    /// look range and a finite origin, or no fire at all. How far the origin may reach is the
    /// step's to bound. Both peers apply it to every command, so they step the same bits.
    pub fn sanitized(mut self) -> Option<Self> {
        if !(self.frac.is_finite()
            && self.look.x.is_finite()
            && self.look.y.is_finite()
            && self.origin.is_none_or(Vec3::is_finite))
        {
            return None;
        }
        self.frac = self.frac.clamp(0.0, 1.0);
        self.look.y = self.look.y.clamp(-1.5, 1.5);
        Some(self)
    }
}

/// One tick of a player's input: what its client sends and both peers step.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq)]
pub struct UserCmd {
    /// Its place in the player's command stream: one more each tick, wrapping.
    pub sequence: u32,
    /// The movement stick: x right, y forward, length at most one.
    pub movement: Vec2,
    /// Where the player looks: yaw and pitch, radians.
    pub look: Vec2,
    /// The buttons down for the tick.
    pub buttons: Buttons,
    /// The fire press, if the player clicked during the command's window.
    pub fire: Option<SubtickFire>,
    /// The remote tick other players were drawn at when the client built this command: with
    /// [`Self::seen_alpha`], the time both peers judge its hits at, so you hit what you saw.
    pub seen_server_tick: u64,
    /// Fraction into the tick after [`Self::seen_server_tick`], from the interpolation clock.
    pub seen_alpha: f32,
}

/// Every still-unacked command, oldest first. On the wire it is one blob, each command
/// delta-coded losslessly against the one before it. A received packet keeps its newest 50
/// commands, the most the server queues, however many the blob holds.
#[derive(Event, Clone, Debug, Default)]
pub struct UserCmdPacket {
    /// The commands, oldest first.
    pub commands: Vec<UserCmd>,
}

impl Serialize for UserCmdPacket {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_bytes(&crate::usercmd_wire::encode(&self.commands))
    }
}

impl<'de> Deserialize<'de> for UserCmdPacket {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Blob;

        impl<'de> serde::de::Visitor<'de> for Blob {
            type Value = UserCmdPacket;

            fn expecting(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
                f.write_str("a delta-coded usercmd blob")
            }

            fn visit_bytes<E: serde::de::Error>(self, bytes: &[u8]) -> Result<Self::Value, E> {
                let keep = crate::server::SERVER_CMD_QUEUE_MAX_COMMANDS as usize;
                crate::usercmd_wire::decode(bytes, keep)
                    .map(|commands| UserCmdPacket { commands })
                    .map_err(E::custom)
            }

            // Formats without a bytes type (JSON) hand over a sequence.
            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> Result<Self::Value, A::Error> {
                let mut bytes = Vec::new();
                while let Some(byte) = seq.next_element::<u8>()? {
                    bytes.push(byte);
                }
                self.visit_bytes(&bytes)
            }
        }

        deserializer.deserialize_bytes(Blob)
    }
}

/// How a link loses packets, tagged by the game on its endpoint: the client picks how many
/// commands to repeat by it.
#[derive(Component, Clone, Copy, Debug, PartialEq, Eq)]
pub enum LinkLossModel {
    /// TCP-like: a loss stalls the pipe until it is resent, so repeated commands buy nothing.
    Stream,
    /// UDP-like: a loss drops one packet; the repeated unacked tail recovers it.
    Datagram,
}

/// What viewers see of a player's movement, for drawing it. Rides every [`PlayerSnapshot`]; the
/// owner's [`OwnerSnapshot`] carries the whole [`MoveState`].
#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq)]
pub struct MoveFlags {
    /// Standing on the ground.
    pub grounded: bool,
    /// In the crouched hull: draw the low pose.
    pub crouching: bool,
}

impl MoveFlags {
    /// The flags of `moves`.
    pub fn from_moves(moves: &MoveState) -> Self {
        Self {
            grounded: moves.ground.is_some(),
            crouching: moves.crouching,
        }
    }
}

/// Tolerant equality of two movement states for the reconcile gate: exact on the discrete
/// fields and tick counters, within 0.02 (about a degree) on the ground normal. An exact `!=`
/// turns 1-ulp libm drift into full replays at zero position error; position and velocity are
/// the position-error gate's.
pub(crate) fn moves_agree(a: &MoveState, b: &MoveState) -> bool {
    const EPS: f32 = 0.02;
    let ground = match (a.ground, b.ground) {
        (None, None) => true,
        (Some(a), Some(b)) => a.normal.abs_diff_eq(b.normal, EPS),
        _ => false,
    };
    ground
        && a.crouching == b.crouching
        && a.since_ground == b.since_ground
        && a.since_jump == b.since_jump
        && a.buffered == b.buffered
}

/// The owner's own snapshot, beside the pose every client gets in [`PlayerSnapshot`]:
/// everything reconcile needs. It reaches only the owner; what others need of a player's game
/// travels as [`RemoteEvent`]s.
#[derive(Component, Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq)]
#[serde(bound = "")]
pub struct OwnerSnapshot<G: PlayerGame> {
    /// The server tick it was published on.
    pub server_tick: u64,
    /// The server's head has run past this state on guessed input (a command is late or lost):
    /// this is the last state built only from received commands. Still a full verdict at the
    /// ack: reconcile normally. A head that may be revised is never sent.
    pub(crate) provisional: bool,
    /// The ack: the newest command whose tick is final. The state is the player after it.
    pub(crate) last_processed_sequence: u32,
    /// That command's buttons: the previous buttons of the first replayed command.
    pub(crate) last_processed_buttons: Buttons,
    /// The whole movement half of the player at the ack: what a restore puts back, tick
    /// counters and ground included, so a replay starts on the server's timeline.
    pub moves: MoveState,
    /// The game half at the ack, which every restore writes over the prediction.
    #[entities]
    pub game: G,
    /// How early this owner's commands have been reaching the server; the
    /// client paces its fixed clock on it. `None` until enough arrivals.
    pub(crate) input_lead: Option<InputLead>,
}

impl<G: PlayerGame> OwnerSnapshot<G> {
    /// Where the player stood at the ack.
    pub fn position(&self) -> Vec3 {
        self.moves.position
    }

    /// How it moved at the ack.
    pub fn velocity(&self) -> Vec3 {
        self.moves.velocity
    }

    /// Its look at the ack: yaw and pitch.
    pub fn look(&self) -> Vec2 {
        crate::pmove::look(self.moves.orientation)
    }

    /// What viewers would see of it.
    pub fn move_flags(&self) -> MoveFlags {
        MoveFlags::from_moves(&self.moves)
    }
}

/// How early the owner's commands arrived over the last second, in ticks: per server tick that
/// received anything, the freshest sequence minus the one the server was about to step (0 just in
/// time, 2 two to spare, -3 three late and stepped on a guess). Ticks with no arrival record
/// nothing, so a stall's catch-up burst reads as fresh, not late: stalls are rollback's job, not
/// the clock's.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct InputLead {
    /// Worst (latest) arrival in the window: what the nudge steers on.
    pub min: i16,
    /// Best (freshest) arrival in the window: if even this is late, the
    /// client is behind the server's timeline, not just jittery.
    pub max: i16,
    /// Arrival ticks behind `min`/`max`; hard resyncs want a full window.
    pub samples: u16,
}

/// A moving body's authoritative pose, one tick-stamped sample: the only thing a mover that
/// isn't a player puts on the wire, never live physics components, so neither peer's physics
/// fights replication. A game spawns the body with `BodySnapshot::default()`; the server keeps
/// it fresh.
///
/// No velocities: every one a peer needs is the derivative of the pose history
/// ([`body_at`](crate::body_at)), the same on both peers. The tick stays on the wire though the
/// history key carries it: tick 0 marks the unpublished placeholder, and a body spawned
/// mid-frame can replicate before its first publish.
#[derive(Component, Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq)]
#[require(History<BodySnapshot>)]
pub struct BodySnapshot {
    /// The server tick it was published on; 0 until the first publish.
    pub tick: u64,
    /// Where the body is.
    pub position: Vec3,
    /// How it is turned.
    pub rotation: Quat,
    /// Ticks since the server wrote the pose before this one (saturating), 1 while the body
    /// moves every tick; 0 if there was none. A client that misses a write tells it from a rest
    /// by it (see [`crate::history`]).
    pub since_previous: u16,
}

/// Whether command sequence `incoming` comes after `current`, across the wrap: what a game
/// compares a step's sequences with ([`Step::previous_sequence`](crate::Step::previous_sequence),
/// a fuse counted in commands).
pub fn sequence_is_newer(incoming: u32, current: u32) -> bool {
    incoming != current && incoming.wrapping_sub(current) < (u32::MAX / 2)
}

/// Orders two command sequences, across the wrap.
pub(crate) fn sequence_cmp(a: u32, b: u32) -> Ordering {
    if a == b {
        Ordering::Equal
    } else if sequence_is_newer(a, b) {
        Ordering::Greater
    } else {
        Ordering::Less
    }
}

#[cfg(test)]
mod tests {
    use bevy::{prelude::*, state::app::StatesPlugin};
    use bevy_ahoy::moves::{Ground, MoveState};
    use bevy_replicon::prelude::*;

    use super::{NetAhoyProtocolPlugin, PlayerShape, TickRate, moves_agree};
    use crate::game::test_game::TestGame;

    #[test]
    fn a_tick_rate_takes_10_to_1000_hz() {
        assert_eq!(TickRate::new(TickRate::MIN_HZ).hz(), 10);
        assert_eq!(TickRate::new(TickRate::MAX_HZ).hz(), 1000);
    }

    #[test]
    #[should_panic(expected = "under 10 Hz")]
    fn a_tick_rate_under_10_hz_panics() {
        TickRate::new(9);
    }

    #[test]
    #[should_panic(expected = "over 1000 Hz")]
    fn a_tick_rate_over_1000_hz_panics() {
        TickRate::new(1001);
    }

    #[test]
    fn the_tick_rate_sets_the_fixed_clock_and_the_protocol() {
        let app = |rate: TickRate| {
            let mut app = App::new();
            app.add_plugins((
                MinimalPlugins,
                StatesPlugin,
                RepliconPlugins,
                NetAhoyProtocolPlugin::<TestGame>::new(rate),
            ));
            app.finish();
            app
        };
        let (twenty, sixty) = (app(TickRate::default()), app(TickRate::new(60)));
        assert_eq!(*sixty.world().resource::<TickRate>(), TickRate::new(60));
        assert_eq!(
            sixty.world().resource::<Time<Fixed>>().timestep(),
            Time::<Fixed>::from_hz(60.0).timestep()
        );
        assert_eq!(twenty.world().resource::<TickRate>().hz(), 20);
        assert_ne!(
            twenty.world().resource::<ProtocolHash>(),
            sixty.world().resource::<ProtocolHash>()
        );
    }

    #[test]
    fn the_player_shape_is_the_apps_and_part_of_the_protocol() {
        let app = |shape: PlayerShape| {
            let mut app = App::new();
            app.add_plugins((
                MinimalPlugins,
                StatesPlugin,
                RepliconPlugins,
                NetAhoyProtocolPlugin::<TestGame>::default().with_player_shape(shape),
            ));
            app.finish();
            app
        };
        let wide = PlayerShape {
            radius: 0.6,
            ..default()
        };
        let (default_app, wide_app) = (app(PlayerShape::default()), app(wide));
        assert_eq!(*wide_app.world().resource::<PlayerShape>(), wide);
        assert_ne!(
            default_app.world().resource::<ProtocolHash>(),
            wide_app.world().resource::<ProtocolHash>()
        );
    }

    #[test]
    #[should_panic(expected = "set the rate with NetAhoyProtocolPlugin::new")]
    fn a_fixed_clock_at_another_rate_panics() {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, StatesPlugin, RepliconPlugins))
            .insert_resource(Time::<Fixed>::from_hz(30.0))
            .add_plugins(NetAhoyProtocolPlugin::<TestGame>::default());
    }

    #[test]
    #[should_panic(expected = "add RepliconPlugins before it")]
    fn the_protocol_without_replicon_panics() {
        App::new().add_plugins((MinimalPlugins, NetAhoyProtocolPlugin::<TestGame>::default()));
    }

    #[test]
    #[should_panic(expected = "set the rate with NetAhoyProtocolPlugin::new")]
    fn a_fixed_clock_changed_after_the_protocol_panics_at_finish() {
        let mut app = App::new();
        app.add_plugins((
            MinimalPlugins,
            StatesPlugin,
            RepliconPlugins,
            NetAhoyProtocolPlugin::<TestGame>::default(),
        ))
        .insert_resource(Time::<Fixed>::from_hz(60.0));
        app.finish();
    }

    /// A tick's seconds are the fixed clock's timestep to the bit.
    #[test]
    fn seconds_convert_to_whole_ticks() {
        let (twenty, sixty) = (TickRate::new(20), TickRate::new(60));
        assert_eq!(
            (twenty.ticks(0.8), twenty.ticks(0.15), twenty.ticks(1.0)),
            (16, 3, 20)
        );
        assert_eq!(
            (sixty.ticks(0.8), sixty.ticks(0.15), sixty.ticks(1.0)),
            (48, 9, 60)
        );
        let timestep = Time::<Fixed>::from_hz(20.0).timestep();
        assert_eq!(
            (TickRate::default().seconds() as f32).to_bits(),
            timestep.as_secs_f32().to_bits()
        );
    }

    /// Position and velocity are the position-error gate's, not this one's.
    #[test]
    fn moves_agree_on_exact_state_and_a_close_ground_normal() {
        let ground = Ground {
            normal: Vec3::Y,
            friction: 0.5,
        };
        let a = MoveState {
            position: Vec3::Y,
            ground: Some(ground),
            since_ground: 0,
            since_jump: 12,
            ..default()
        };
        assert!(moves_agree(&a, &a));
        assert!(moves_agree(
            &a,
            &MoveState {
                position: Vec3::ZERO,
                velocity: Vec3::X,
                ..a
            }
        ));

        let tilted = |normal: Vec3| MoveState {
            ground: Some(Ground { normal, ..ground }),
            ..a
        };
        assert!(
            moves_agree(&a, &tilted(Vec3::new(0.015, 1.0, -0.015))),
            "within 0.02"
        );
        assert!(
            !moves_agree(&a, &tilted(Vec3::new(0.03, 1.0, 0.0))),
            "0.03 off in x"
        );
        assert!(
            !moves_agree(&a, &MoveState { ground: None, ..a }),
            "grounded or not"
        );

        assert!(!moves_agree(
            &a,
            &MoveState {
                crouching: true,
                ..a
            }
        ));
        assert!(!moves_agree(
            &a,
            &MoveState {
                since_ground: 1,
                ..a
            }
        ));
        assert!(!moves_agree(
            &a,
            &MoveState {
                since_jump: 13,
                ..a
            }
        ));
        assert!(!moves_agree(&a, &MoveState { buffered: 2, ..a }));
    }
}
