//! Client prediction, server input rollback and lag compensation for Bevy movement shooters on
//! bevy_ahoy and bevy_replicon.
//!
//! A game implements [`PlayerGame`] on the plain-data game half of its player, then adds
//! [`NetAhoyProtocolPlugin`], [`NetAhoyServerPlugin`] and [`NetAhoyClientPlugin`] for it.
//! `examples/playground` is a whole game on it: rockets with rocket jumps, and a pistol.
//!
//! # A player's entities
//!
//! | marker | on | is |
//! |---|---|---|
//! | [`ServerPlayer`] | every peer | a player: the server's entity, replicated to every client |
//! | [`OwnPlayer`] / [`RemotePlayer`] | client | the `ServerPlayer` that is its own / any other |
//! | [`PredictedPlayer`] | client (the game's) | its own player, stepped ahead on its commands |
//! | [`OwnVisual`] / [`RemoteVisual`] | client (the game's) | draws its own / a remote player |
//!
//! [`OwnPlayerId`], a client resource, is the own player's [`PlayerId`] once the server accepts
//! the join. The predicted player carries the [`PendingCorrection`] the game eases its drawn body
//! by. [`debug`] has read-only readouts of the netcode for a HUD.
//!
//! # Features
//!
//! - `transport-client` and `transport-server`: the [`transport`] module's sessions,
//!   WebTransport first and WebSocket where it fails, natively and in the browser. Off by
//!   default; the server side is native only.

#![warn(missing_docs, unreachable_pub)]

mod client;
pub mod debug;
mod demo;
mod events;
mod game;
mod history;
mod lag_comp;
pub mod pmove;
mod poses;
mod protocol;
mod reconnect;
mod seen;
mod server;
mod stepper;
#[doc(hidden)]
pub mod testing;
#[cfg(any(feature = "transport-client", feature = "transport-server"))]
pub mod transport;
mod usercmd_wire;

// The character controller netahoy steps: a game spawns its `CharacterController` and reads its
// `MoveState` from here, so it builds against the same bevy_ahoy netahoy does.
pub use bevy_ahoy;

// The game plug-in.
pub use events::{EventId, NoEvents, RemoteEvent, StepEvent};
pub use game::{PlayerGame, PlayerState, Step};
pub use stepper::KccSchedule;

// The shared protocol: the tick, the wire and the world's layers.
pub use protocol::{
    BodySnapshot, Buttons, JoinRequest, LinkLossModel, MoveFlags, NetAhoyProtocolPlugin,
    OwnerSnapshot, PLAYER_COLLISION_LAYER, PlayerId, PlayerShape, PlayerSnapshot, RejoinToken,
    ServerPlayer, SubtickFire, TickRate, UserCmd, WORLD_COLLISION_LAYER, sequence_is_newer,
};

// Lag compensation: histories, poses and the world a command saw.
pub use history::{AppHistoryExt, History, Recorded, Sample};
pub use lag_comp::{LagCompensatedCast, LagCompensatedHit, PlayerPoses};
pub use poses::{BodyPose, RemoteFlags, RemotePose, RemoteRenderTime, body_at};
pub use seen::{StepWorld, fire_position};

// The client.
pub use client::{
    ClientInput, NetAhoyClientPlugin, NetAhoyClientSystems, OwnPlayer, OwnPlayerId, OwnVisual,
    PendingCorrection, PredictedPlayer, PresentedFrame, RemotePlayer, RemoteSampledFrame,
    RemoteVisual,
};

// The server.
#[cfg(not(target_family = "wasm"))]
pub use server::one_tick_per_frame;
pub use server::{
    NetAhoyServerPlugin, NetAhoyServerSystems, PlayerOwner, QueuedUserCmds, ServerTick,
};

// Sessions, reconnects and demos.
pub use demo::{
    DemoPlayback, DemoSeeked, DemoSource, NetAhoyDemoPlaybackPlugin, NetAhoyDemoRecordPlugin,
};
pub use reconnect::{ClientTimeout, JoinCredential, PROTOCOL_MISMATCH, Reconnect};
