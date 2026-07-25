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
            .add_server_event::<RocketFired>(Channel::Ordered)
            .add_server_event::<RocketHit>(Channel::Ordered);
    }
}

#[derive(Component, Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
pub struct NetworkedPlayer;

#[derive(Component, Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct PlayerId(pub u64);

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

#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq)]
pub struct NetAhoyMoveState {
    pub grounded: bool,
    pub crouching: bool,
    pub mantle_height_left: Option<f32>,
    pub crane_height_left: Option<f32>,
}

impl NetAhoyMoveState {
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
    pub last_processed_sequence: u32,
    pub last_processed_buttons: AhoyButtons,
    pub position: Vec3,
    pub velocity: Vec3,
    pub look: Vec2,
    pub state: NetAhoyMoveState,
    /// The server-corrected player POD; whatever the client can't re-derive
    /// from its own command stream gets stomped over the frame's prediction
    /// on every restore.
    pub player_state: NetAhoyPlayerState,
}

/// [`AhoySnapshot`]'s sibling for plain rigid bodies (vehicles, props): the
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
