//! Wire types, replicated components, and the protocol plugin both peers add.

use std::{
    cmp::Ordering,
    net::{IpAddr, Ipv4Addr, SocketAddr},
};

use bevy::prelude::*;
use bevy_ahoy::{MantleState, prelude::*};
use bevy_replicon::prelude::*;
use bitflags::bitflags;
use serde::{Deserialize, Serialize};

use crate::math::RemoteSnapshotSample;
use crate::player::{NetAhoyPlayerState, RocketFired, RocketHit};

pub const DEFAULT_PORT: u16 = 5000;
pub const FIXED_TIMESTEP_HZ: f64 = 20.0;
pub const DEFAULT_SERVER_ADDR: SocketAddr =
    SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), DEFAULT_PORT);
pub const DEFAULT_SERVER_URL: &str = "ws://127.0.0.1:5000";
pub const PLAYER_CAPSULE_RADIUS: f32 = 0.45;
pub const PLAYER_CAPSULE_HALF_HEIGHT: f32 = 0.75;

#[derive(Default)]
pub struct NetAhoyProtocolPlugin;

impl Plugin for NetAhoyProtocolPlugin {
    fn build(&self, app: &mut App) {
        app.replicate::<NetworkedPlayer>()
            .replicate::<PlayerId>()
            .replicate::<PlayerSnapshot>()
            .replicate::<AhoySnapshot>()
            .replicate::<BodySnapshot>()
            .add_client_event::<JoinRequest>(Channel::Ordered)
            .add_server_event::<JoinAccepted>(Channel::Ordered)
            .add_client_event::<AhoyUserCmdPacket>(Channel::Unreliable)
            .add_server_message::<RemoteSnapshotFrame>(Channel::Ordered)
            .add_server_event::<RocketFired>(Channel::Ordered)
            .add_server_event::<RocketHit>(Channel::Ordered);
    }
}

#[derive(Component, Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
pub struct NetworkedPlayer;

#[derive(Component, Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct PlayerId(pub u64);

/// Every authoritative player pose from one server fixed tick. Unlike a
/// replicated component, messages retain all ticks when the transport
/// delivers several updates in one client frame.
#[derive(Message, Serialize, Deserialize, Clone, Debug)]
pub struct RemoteSnapshotFrame {
    pub server_tick: u64,
    pub samples: Vec<(PlayerId, RemoteSnapshotSample)>,
}

#[derive(Event, Serialize, Deserialize, Clone, Copy, Debug)]
pub struct JoinRequest;

#[derive(Event, Serialize, Deserialize, Clone, Copy, Debug)]
pub struct JoinAccepted {
    pub player_id: u64,
}

bitflags! {
    /// Buttons for one command. Bits 0..16 are the library's (movement reads them);
    /// bits 16.. are the game's, carried untouched (e.g. weapon fire via from_bits_retain).
    #[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
    pub struct AhoyButtons: u32 {
        const JUMP = 1 << 0;
        const CROUCH = 1 << 1;
        const TAC = 1 << 2;
        const MANTLE = 1 << 3;
        const CRANE = 1 << 4;
        const CLIMBDOWN = 1 << 5;
        const SWIM_UP = 1 << 6;
    }
}

/// A fire press, sub-tick. `Some` on a command IS the rising edge — sampled
/// per render frame, so a click can't fall between ticks. Carries no origin:
/// both peers derive it as `previous_position.lerp(position, frac)`, the same
/// interpolation the client rendered — derivable data never rides the wire,
/// so there's nothing for the server to plausibility-check.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
pub struct SubtickFire {
    /// Fraction into the tick window when the click happened, from the fixed
    /// clock's overstep. Raw f32 on purpose, like `seen_alpha`: both peers
    /// step the exact same bits.
    pub frac: f32,
    /// Look angles at the click, not the tick-boundary sample.
    pub look: Vec2,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq)]
pub struct AhoyUserCmd {
    pub sequence: u32,
    pub movement: Vec2,
    pub look: Vec2,
    pub buttons: AhoyButtons,
    /// Weapon fire for this command, if the player clicked during its window.
    /// Postcard makes `None` one byte, so idle commands stay lean.
    pub fire: Option<SubtickFire>,
    /// The remote render time (tick + `seen_alpha`) the client's remote player
    /// capsules were displayed at when it built this command. The shared step
    /// sweeps rocket-vs-player at this time via lag-comp history, so both
    /// peers judge hits against the same poses and the client's prediction
    /// holds — you hit what you saw.
    pub seen_server_tick: u64,
    /// Fraction into the tick after [`Self::seen_server_tick`], straight from
    /// the interpolation clock. Raw f32 on purpose: both peers sample with the
    /// exact same bits, so the judgments stay identical.
    pub seen_alpha: f32,
}

#[derive(Event, Serialize, Deserialize, Clone, Debug, Default)]
pub struct AhoyUserCmdPacket {
    pub commands: Vec<AhoyUserCmd>,
}

/// The loss semantics of an IO link. The game tags this at endpoint spawn;
/// netahoy uses it only to choose the client's usercmd redundancy policy and
/// stays ignorant of transport brands.
#[derive(Component, Clone, Copy, Debug, PartialEq, Eq)]
pub enum LinkLossModel {
    /// TCP-like: a loss stalls the whole pipe until retransmission. Redundant
    /// command copies buy nothing because the transport redelivers them.
    Stream,
    /// UDP-like: a loss drops one packet and the pipe keeps flowing. The
    /// redundant unacked-command tail is the recovery mechanism.
    Datagram,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq)]
pub struct NetAhoyMoveState {
    pub grounded: bool,
    pub crouching: bool,
    pub mantle_height_left: Option<f32>,
    pub crane_height_left: Option<f32>,
}

impl NetAhoyMoveState {
    /// Tolerant equality for the reconciliation gate: exact on the discrete
    /// fields, epsilon on the floats. The old exact `!=` turned 1-ulp float
    /// drift (cross-platform libm inside the cast pipeline, f32 wire
    /// round-trips) into full replays at zero position error.
    ///
    /// 0.02, not 1e-3: every float here is probe-derived (mantle/crane
    /// height), and against a MOVING surface the peers sample a hair apart
    /// in time, so mm-level disagreement is steady state. 2 cm is below
    /// anything gameplay-visible; the position-error gate catches real
    /// divergence.
    pub fn agrees_with(&self, other: &Self) -> bool {
        const EPS: f32 = 0.02;
        fn close(a: f32, b: f32) -> bool {
            (a - b).abs() <= EPS
        }
        fn opt_close(a: Option<f32>, b: Option<f32>) -> bool {
            match (a, b) {
                (None, None) => true,
                (Some(a), Some(b)) => close(a, b),
                _ => false,
            }
        }
        self.grounded == other.grounded
            && self.crouching == other.crouching
            && opt_close(self.mantle_height_left, other.mantle_height_left)
            && opt_close(self.crane_height_left, other.crane_height_left)
    }

    pub fn from_controller_state(state: &CharacterControllerState) -> Self {
        Self {
            grounded: state.grounded.is_some(),
            crouching: state.crouching,
            mantle_height_left: state.mantle.as_ref().map(|mantle| mantle.height_left),
            crane_height_left: state.crane_height_left,
        }
    }

    pub fn apply_to_controller_state(&self, state: &mut CharacterControllerState) {
        state.crouching = self.crouching;
        state.crane_height_left = self.crane_height_left;
        state.mantle = self
            .mantle_height_left
            .map(|height_left| MantleState { height_left });

        if self.grounded {
            state.last_ground.reset();
        } else {
            state.grounded = None;
        }
    }
}

/// Reconciliation-only completion of [`NetAhoyMoveState`]: the carry values
/// and stopwatches that decide the discrete moves — cooldowns, land-momentum
/// restore, the bounce pre-projection velocity. Without these a
/// rollback replays with timers from the mispredicted timeline, so one
/// disagreed event (a jump one tick apart, a cooldown) keeps re-disagreeing
/// on every ack: the state_mismatch cascade. Rides the owner-only
/// [`AhoySnapshot`]; remotes never branch on any of this.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq)]
pub struct NetAhoyCarryState {
    pub sliding: bool,
    pub tac_velocity: f32,
    pub land_speed: f32,
    pub last_velocity: Vec3,
    /// The platform carry ("standing on or recently jumped off"): added
    /// around every move phase, so a stale value is a permanent push the
    /// reconciled velocity never shows — and it diverges whenever the peers
    /// sample a moving platform one tick apart. Carried here so the next ack
    /// heals it in one replay instead of re-disagreeing forever.
    pub platform_velocity: Vec3,
    pub platform_angular_velocity: Vec3,
    // Stopwatches as elapsed seconds, clamped: the Duration::MAX defaults
    // would round-trip through f32 into from_secs_f32 territory best avoided.
    pub last_ground: f32,
    pub last_land: f32,
    pub last_slide: f32,
    pub last_bounce: f32,
    pub last_jump: f32,
    pub last_tac: f32,
    pub last_step_up: f32,
    pub last_step_down: f32,
}

const CARRY_ELAPSED_MAX: f32 = 1e6;

fn watch_secs(watch: &bevy::time::Stopwatch) -> f32 {
    watch.elapsed_secs().min(CARRY_ELAPSED_MAX)
}

fn set_watch(watch: &mut bevy::time::Stopwatch, secs: f32) {
    watch.set_elapsed(std::time::Duration::from_secs_f32(
        secs.clamp(0.0, CARRY_ELAPSED_MAX),
    ));
}

impl NetAhoyCarryState {
    pub fn from_controller_state(state: &CharacterControllerState) -> Self {
        Self {
            sliding: state.sliding,
            tac_velocity: state.tac_velocity,
            land_speed: state.land_speed,
            last_velocity: state.last_velocity,
            platform_velocity: state.platform_velocity,
            platform_angular_velocity: state.platform_angular_velocity,
            last_ground: watch_secs(&state.last_ground),
            last_land: watch_secs(&state.last_land),
            last_slide: watch_secs(&state.last_slide),
            last_bounce: watch_secs(&state.last_bounce),
            last_jump: watch_secs(&state.last_jump),
            last_tac: watch_secs(&state.last_tac),
            last_step_up: watch_secs(&state.last_step_up),
            last_step_down: watch_secs(&state.last_step_down),
        }
    }

    pub fn apply_to_controller_state(&self, state: &mut CharacterControllerState) {
        state.sliding = self.sliding;
        state.tac_velocity = self.tac_velocity;
        state.land_speed = self.land_speed;
        state.last_velocity = self.last_velocity;
        state.platform_velocity = self.platform_velocity;
        state.platform_angular_velocity = self.platform_angular_velocity;
        set_watch(&mut state.last_ground, self.last_ground);
        set_watch(&mut state.last_land, self.last_land);
        set_watch(&mut state.last_slide, self.last_slide);
        set_watch(&mut state.last_bounce, self.last_bounce);
        set_watch(&mut state.last_jump, self.last_jump);
        set_watch(&mut state.last_tac, self.last_tac);
        set_watch(&mut state.last_step_up, self.last_step_up);
        set_watch(&mut state.last_step_down, self.last_step_down);
    }
}

/// The per-player snapshot every client receives — exactly the subset remote
/// consumers read (interpolation, the client's lag-comp mirror, the server
/// clock), nothing else. The wire type IS the sample type the buffers store.
#[derive(Component, Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Deref)]
pub struct PlayerSnapshot(pub RemoteSnapshotSample);

/// The owner-only sibling of [`PlayerSnapshot`]: everything reconciliation
/// needs (ack sequence, buttons, the full game POD with rockets and weapon).
/// A `VisibilityFilter` on `PlayerOwner` keeps it off every other client's
/// wire — remote rockets already travel as [`RocketFired`]/[`RocketHit`]
/// events, so nobody but the owner ever read this.
#[derive(Component, Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq)]
pub struct AhoySnapshot {
    pub server_tick: u64,
    /// The server head contains one or more repeated-input ticks that may be
    /// revised when their real commands arrive. Never reconcile owner
    /// prediction against this provisional state.
    pub provisional: bool,
    pub last_processed_sequence: u32,
    pub last_processed_buttons: AhoyButtons,
    pub position: Vec3,
    pub velocity: Vec3,
    pub look: Vec2,
    pub state: NetAhoyMoveState,
    /// Timers/carry values the discrete moves branch on, applied over the
    /// locally recorded frame on every restore so replays converge to the
    /// server's timeline instead of re-fighting it.
    pub carry: NetAhoyCarryState,
    /// The server-corrected player POD; whatever the client can't re-derive
    /// from its own command stream gets stomped over the frame's prediction
    /// on every restore.
    pub player_state: NetAhoyPlayerState,
}

/// [`AhoySnapshot`]'s sibling for plain rigid bodies: the
/// authoritative pose + velocities as one atomic, tick-stamped sample. This is
/// the *only* thing a non-player mover puts on the wire — never live physics
/// components, so neither peer's physics engine can fight replication over
/// them. The server keeps it fresh via `publish_body_snapshots`; a game spawns
/// the body with `BodySnapshot::default()` and consumes it client-side with
/// explicitly-owned systems (a tick of 0 means "not yet published").
///
/// Velocities ride along because they're the cheapest bytes on the wire:
/// extrapolation through loss, interpolation tangents, sim seeding on
/// authority transfer, and blast responses all want them.
#[derive(Component, Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq)]
pub struct BodySnapshot {
    pub tick: u64,
    pub position: Vec3,
    pub rotation: Quat,
    pub linear_velocity: Vec3,
    pub angular_velocity: Vec3,
}

pub fn sequence_is_newer(incoming: u32, current: u32) -> bool {
    incoming != current && incoming.wrapping_sub(current) < (u32::MAX / 2)
}

pub fn sequence_cmp(a: u32, b: u32) -> Ordering {
    if a == b {
        Ordering::Equal
    } else if sequence_is_newer(a, b) {
        Ordering::Greater
    } else {
        Ordering::Less
    }
}

#[cfg(test)]
mod carry_tests {
    use super::*;

    /// The platform carry is added around every move phase but never shows
    /// in the reconciled velocity, so it MUST ride the carry state or a
    /// one-tick peer disagreement replays as a permanent constant offset.
    #[test]
    fn platform_carry_round_trips_through_carry_state() {
        let state = CharacterControllerState {
            platform_velocity: Vec3::new(9.2, 0.0, -1.0),
            platform_angular_velocity: Vec3::new(0.0, 0.5, 0.0),
            ..default()
        };
        let carry = NetAhoyCarryState::from_controller_state(&state);

        let mut restored = CharacterControllerState {
            platform_velocity: Vec3::splat(123.0),
            ..default()
        };
        carry.apply_to_controller_state(&mut restored);
        assert_eq!(restored.platform_velocity, state.platform_velocity);
        assert_eq!(restored.platform_angular_velocity, state.platform_angular_velocity);
    }
}
