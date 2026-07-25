//! Takes the moves players send, runs them, and tells everyone what really
//! happened — plus a little history for lag compensation.

use std::collections::VecDeque;

use avian3d::prelude::*;
use bevy::prelude::*;
use bevy_ahoy::{prelude::*, CharacterLook};
use bevy_replicon::prelude::*;

use crate::{
    math::{LagCompensationHistory, RemoteFlags, RemoteRenderTime, RemoteSnapshotSample},
    step::NetAhoyStepper,
    protocol::*,
    player::{process_rocket_events, NetAhoyPlayerEvents, NetAhoyPlayerState},
};

pub const SERVER_USERCMD_BUDGET_PER_PLAYER: usize = 4;
pub const SERVER_CMD_QUEUE_CAPACITY: usize = 256;
/// Ticks without a processed usercmd before a player publishes
/// [`RemoteFlags::CONNECTION_INTERRUPTED`]: 1 s at 20 Hz — long enough that
/// ordinary loss bursts don't flicker the flag.
pub const CONNECTION_INTERRUPTED_TICKS: u64 = 20;

#[derive(SystemSet, Debug, Clone, Copy, Hash, PartialEq, Eq)]
pub enum ServerNetAhoySystems {
    /// Consume queued user commands and step player KCCs (`FixedPreUpdate`).
    ApplyCommands,
    /// Publish snapshots and record lag-compensation poses (`FixedLast`).
    Publish,
}

pub struct ServerNetAhoyPlugin;

impl Plugin for ServerNetAhoyPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<ServerTick>()
            .init_resource::<LagCompensationHistory>()
            .init_resource::<NetAhoyPlayerEvents>()
            .add_visibility_filter::<PlayerOwner>()
            .add_observer(queue_player_commands)
            .add_systems(FixedFirst, advance_server_tick)
            .add_systems(Update, log_cmd_cadence)
            .add_systems(
                FixedPreUpdate,
                (
                    apply_player_commands.in_set(ServerNetAhoySystems::ApplyCommands),
                    process_rocket_events.after(ServerNetAhoySystems::ApplyCommands),
                ),
            )
            .add_systems(
                FixedLast,
                (
                    publish_authoritative_player_snapshots,
                    publish_body_snapshots,
                    record_lag_compensation_history,
                )
                    .chain()
                    .in_set(ServerNetAhoySystems::Publish),
            );
    }
}

#[derive(Resource, Default)]
pub struct ServerTick(pub u64);

/// Which client connection owns this player entity. Immutable because it
/// doubles as the [`VisibilityFilter`] deciding who receives [`AhoySnapshot`].
#[derive(Component, Clone, Copy, Debug, PartialEq, Eq)]
#[component(immutable)]
pub struct PlayerOwner(pub Entity);

/// Only the owning client receives the reconcile snapshot; everyone else gets
/// just [`PlayerSnapshot`]. A player with no filter component is UNfiltered
/// (visible to all), so server bots must carry `PlayerOwner(Entity::PLACEHOLDER)`
/// — owned by nobody, reconcile data sent to nobody.
impl VisibilityFilter for PlayerOwner {
    /// Never present on client entities: visibility is decided purely by
    /// comparing the owning connection against the client entity itself.
    type ClientComponent = PlayerOwner;
    type Scope = SingleComponent<AhoySnapshot>;

    fn is_visible(&self, client: Entity, _: Option<&Self>) -> bool {
        self.0 == client
    }
}

/// Per-tick usercmd consumption histogram: how many ticks consumed 0, 1, 2,
/// or 3+ commands. A healthy client is all 1s; 0s and 2s in equal measure are
/// arrival jitter printing plateaus and double-steps into the published
/// timeline (the "remote player rubber bands" signature).
#[derive(Clone, Copy, Debug, Default)]
pub struct CmdCadence {
    pub ticks: [u32; 4],
}

impl CmdCadence {
    pub fn irregular(&self) -> u32 {
        self.ticks[0] + self.ticks[2] + self.ticks[3]
    }

    pub fn active(&self) -> u32 {
        self.ticks[1] + self.ticks[2] + self.ticks[3]
    }
}

#[derive(Component, Clone, Copy, Debug, Default)]
pub struct ServerCommandBuffer {
    pub last_processed_sequence: u32,
    pub last_buttons: AhoyButtons,
    pub cadence: CmdCadence,
    /// De-jitter reserve established: set once the queue first fills to two
    /// commands at join, after which consumption is paced at one per tick.
    pub primed: bool,
    /// Movement-stick magnitude of the last processed command, published in
    /// [`RemoteSnapshotSample`] so remote viewers can tell a skid from a run.
    pub last_has_move_input: bool,
    /// Server tick when a usercmd was last processed; drives
    /// [`RemoteFlags::CONNECTION_INTERRUPTED`]. Starts at 0, so a fresh player
    /// reads as interrupted until their first command lands.
    pub last_command_tick: u64,
}

#[derive(Component, Clone, Debug)]
pub struct QueuedUserCmds {
    pub commands: VecDeque<AhoyUserCmd>,
}

impl Default for QueuedUserCmds {
    fn default() -> Self {
        Self {
            commands: VecDeque::with_capacity(SERVER_CMD_QUEUE_CAPACITY),
        }
    }
}

impl QueuedUserCmds {
    pub fn push_packet(&mut self, packet: &AhoyUserCmdPacket, last_processed_sequence: u32) {
        for command in &packet.commands {
            if command.sequence == 0
                || !sequence_is_newer(command.sequence, last_processed_sequence)
                || self
                    .commands
                    .iter()
                    .any(|queued| queued.sequence == command.sequence)
            {
                continue;
            }

            self.commands.push_back(*command);
        }

        self.commands
            .make_contiguous()
            .sort_by(|a, b| sequence_cmp(a.sequence, b.sequence));

        while self.commands.len() > SERVER_CMD_QUEUE_CAPACITY {
            self.commands.pop_front();
        }
    }

    pub fn pop_next(&mut self) -> Option<AhoyUserCmd> {
        self.commands.pop_front()
    }
}

fn advance_server_tick(mut tick: ResMut<ServerTick>) {
    tick.0 = tick.0.wrapping_add(1);
}

fn queue_player_commands(
    packet: On<FromClient<AhoyUserCmdPacket>>,
    mut players: Query<(&PlayerOwner, &ServerCommandBuffer, &mut QueuedUserCmds)>,
) {
    let Some(client) = packet.client_id.entity() else {
        return;
    };

    for (owner, command_buffer, mut queued) in &mut players {
        if owner.0 == client {
            queued.push_packet(&packet, command_buffer.last_processed_sequence);
            return;
        }
    }
}

fn apply_player_commands(
    tick: Res<ServerTick>,
    mut players: Query<(
        Entity,
        &PlayerOwner,
        &mut ServerCommandBuffer,
        &mut QueuedUserCmds,
    )>,
    mut stepper: NetAhoyStepper,
) {
    let min_rewind_tick = tick.0.saturating_sub(crate::math::LAG_COMPENSATION_HISTORY_CAPACITY as u64);

    for (player, owner, mut command_buffer, mut queued) in &mut players {


        // Prime the reserve at join instead of on the first starve: hold the
        // first command one tick while the queue fills to two, so the
        // standing reserve exists before anyone watches this player move. A
        // demo's first impression shouldn't be the one starve that
        // self-priming would have let through. Server-driven bots
        // (PLACEHOLDER owner) feed their queue locally with zero jitter and
        // would deadlock waiting for depth two, so they skip priming.
        // !!! CONTROVERSIAL !!!
        // Adds a tick of input delay to server command processing. More latency, 
        // but less rubber-banding for other players on your screen. Movement 
        // unaffected thanks to prediction, however events like kills,
        // explosions, etc, come at an additional 50ms delay at 20hz. 
        if !command_buffer.primed && owner.0 != Entity::PLACEHOLDER {
            if queued.commands.len() < 2 {
                stepper.clear_transient(player);
                continue;
            }
            command_buffer.primed = true;
        }

        let mut processed = 0;

        // De-jitter: consume ONE command per tick in the steady state, so
        // transport arrival jitter (QUIC pacing, TCP retransmit bursts) never
        // prints plateaus/double-steps into the published position timeline.
        // The queue self-primes a one-command reserve after the first starve,
        // which then absorbs ±1 tick of jitter at the cost of 1 tick of
        // server-side input delay — invisible under client prediction. Bigger
        // backlogs (tab wake, burst delivery) drain gently at 2, floods at
        // the full budget.
        let tick_budget = match queued.commands.len() {
            0..=2 => 1,
            3..=8 => 2,
            _ => SERVER_USERCMD_BUDGET_PER_PLAYER,
        };

        while processed < tick_budget {
            let Some(mut command) = queued.pop_next() else {
                stepper.clear_transient(player);
                break;
            };

            // Sanitize the client's claimed view time: no further back than
            // the pose history holds, never into the server's future. A
            // clamped command can disagree with the client's prediction —
            // the rocket snapshot correction absorbs that.
            let seen = RemoteRenderTime::new(command.seen_server_tick, command.seen_alpha)
                .clamp_ticks(min_rewind_tick, tick.0);
            command.seen_server_tick = seen.tick;
            command.seen_alpha = seen.alpha;

            // Same trust boundary for the fire data: an honest client's values
            // pass through untouched (so both peers step identical bits); a
            // doctored command gets clamped or dropped and the client's
            // prediction eats the snapshot correction.
            if let Some(fire) = &mut command.fire {
                if fire.frac.is_finite() && fire.look.x.is_finite() && fire.look.y.is_finite() {
                    fire.frac = fire.frac.clamp(0.0, 1.0);
                    fire.look.y = fire.look.y.clamp(-1.5, 1.5);
                } else {
                    command.fire = None;
                }
            }

            if let Err(err) = stepper.player_move(
                player,
                command,
                command_buffer.last_processed_sequence,
                command_buffer.last_buttons,
            ) {
                warn!(
                    "failed to step server KCC for {player} command {}: {err}",
                    command.sequence
                );
            }

            if sequence_is_newer(command.sequence, command_buffer.last_processed_sequence) {
                command_buffer.last_processed_sequence = command.sequence;
            }
            command_buffer.last_buttons = command.buttons;
            command_buffer.last_has_move_input = command.movement.length() > 0.1;
            command_buffer.last_command_tick = tick.0;
            processed += 1;
        }

        command_buffer.cadence.ticks[processed.min(3)] += 1;

        if processed == SERVER_USERCMD_BUDGET_PER_PLAYER && !queued.commands.is_empty() {
            debug!(
                "server usercmd budget hit for {player}: {} queued",
                queued.commands.len()
            );
        }
    }
}

/// Every 30 s, log players whose command cadence was irregular — the direct
/// evidence for (or against) transport-induced rubber banding.
fn log_cmd_cadence(
    time: Res<Time>,
    mut window: Local<f32>,
    mut players: Query<(&PlayerId, &mut ServerCommandBuffer)>,
) {
    *window += time.delta_secs();
    if *window < 30.0 {
        return;
    }

    for (player_id, mut command_buffer) in &mut players {
        let cadence = command_buffer.cadence;
        // Skip idle players (all zeros) and perfectly steady ones.
        if cadence.active() > 0 && cadence.irregular() > 0 {
            info!(
                "player {} cmd cadence, last {:.0}s: 0x{} 1x{} 2x{} 3+x{}",
                player_id.0,
                *window,
                cadence.ticks[0],
                cadence.ticks[1],
                cadence.ticks[2],
                cadence.ticks[3],
            );
        }
        command_buffer.cadence = CmdCadence::default();
    }
    *window = 0.0;
}

fn publish_authoritative_player_snapshots(
    tick: Res<ServerTick>,
    mut players: Query<(
        &ServerCommandBuffer,
        &Position,
        &LinearVelocity,
        &CharacterLook,
        &CharacterControllerState,
        &NetAhoyPlayerState,
        &mut AhoySnapshot,
        &mut PlayerSnapshot,
    )>,
) {
    for (
        command_buffer,
        position,
        velocity,
        look,
        controller_state,
        player_state,
        mut snapshot,
        mut player_snapshot,
    ) in &mut players
    {
        // Q3's EF_CONNECTION. The KCC only steps per usercmd, so an
        // interrupted player's position is frozen — publish zero velocity too,
        // or a stored impulse (rocket knockback waiting for the tab to wake)
        // turns into oscillating Hermite tangents on every viewer's screen.
        let interrupted = tick.0.saturating_sub(command_buffer.last_command_tick)
            > CONNECTION_INTERRUPTED_TICKS;
        let mut flags = RemoteFlags::empty();
        flags.set(RemoteFlags::MOVE_INPUT, command_buffer.last_has_move_input);
        flags.set(RemoteFlags::CONNECTION_INTERRUPTED, interrupted);
        let sample = RemoteSnapshotSample {
            server_tick: tick.0,
            position: **position,
            velocity: if interrupted { Vec3::ZERO } else { **velocity },
            look: Vec2::new(look.yaw, look.pitch),
            state: NetAhoyMoveState::from_controller_state(controller_state),
            flags,
        };
        player_snapshot.0 = sample;

        snapshot.server_tick = sample.server_tick;
        snapshot.last_processed_sequence = command_buffer.last_processed_sequence;
        snapshot.last_processed_buttons = command_buffer.last_buttons;
        snapshot.position = sample.position;
        snapshot.velocity = sample.velocity;
        snapshot.look = sample.look;
        snapshot.state = sample.state;
        snapshot.player_state = *player_state;
    }
}

/// Keep each replicated body's [`BodySnapshot`] in sync with its physics, so
/// the pose crosses the wire as one atomic, tick-stamped sample instead of
/// live engine components. Only writes on change, so a parked body goes quiet.
fn publish_body_snapshots(
    tick: Res<ServerTick>,
    mut bodies: Query<(
        &mut BodySnapshot,
        &Position,
        &Rotation,
        &LinearVelocity,
        &AngularVelocity,
    )>,
) {
    for (mut snapshot, position, rotation, linear_velocity, angular_velocity) in &mut bodies {
        let next = BodySnapshot {
            tick: tick.0,
            position: position.0,
            rotation: rotation.0,
            linear_velocity: linear_velocity.0,
            angular_velocity: angular_velocity.0,
        };
        // Compare everything but the tick so an unchanged pose stays unwritten.
        if (BodySnapshot { tick: snapshot.tick, ..next }) != *snapshot {
            *snapshot = next;
        }
    }
}

fn record_lag_compensation_history(
    tick: Res<ServerTick>,
    mut history: ResMut<LagCompensationHistory>,
    players: Query<
        (
            &PlayerId,
            &Position,
            &LinearVelocity,
            &CharacterLook,
            &CharacterControllerState,
        ),
        With<NetworkedPlayer>,
    >,
) {
    for (player_id, position, velocity, look, state) in &players {
        history.record(
            *player_id,
            RemoteSnapshotSample {
                server_tick: tick.0,
                position: **position,
                velocity: **velocity,
                look: Vec2::new(look.yaw, look.pitch),
                state: NetAhoyMoveState::from_controller_state(state),
                // Lag-comp poses only rewind hitboxes; flags aren't consulted.
                flags: RemoteFlags::empty(),
            },
        );
    }
}
