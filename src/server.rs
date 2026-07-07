//! Takes the moves players send, runs them, and tells everyone what really
//! happened — plus a little history for lag compensation.

use std::collections::VecDeque;

use avian3d::prelude::*;
use bevy::prelude::*;
use bevy_ahoy::{prelude::*, CharacterLook};
use bevy_replicon::prelude::*;

use crate::{
    math::{LagCompensationHistory, RemoteRenderTime, RemoteSnapshotSample},
    step::NetAhoyStepper,
    protocol::*,
    player::{process_rocket_events, NetAhoyPlayerEvents, NetAhoyPlayerState},
};

pub const SERVER_USERCMD_BUDGET_PER_PLAYER: usize = 4;
pub const SERVER_CMD_QUEUE_CAPACITY: usize = 256;

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
            .add_observer(queue_player_commands)
            .add_systems(FixedFirst, advance_server_tick)
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

/// Which client connection owns this player entity.
#[derive(Component, Clone, Copy, Debug, PartialEq, Eq)]
pub struct PlayerOwner(pub Entity);

#[derive(Component, Clone, Copy, Debug, Default)]
pub struct ServerCommandBuffer {
    pub last_processed_sequence: u32,
    pub last_buttons: AhoyButtons,
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
    mut players: Query<(Entity, &mut ServerCommandBuffer, &mut QueuedUserCmds), With<PlayerOwner>>,
    mut stepper: NetAhoyStepper,
) {
    let min_rewind_tick = tick.0.saturating_sub(crate::math::LAG_COMPENSATION_HISTORY_CAPACITY as u64);

    for (player, mut command_buffer, mut queued) in &mut players {
        let mut processed = 0;

        while processed < SERVER_USERCMD_BUDGET_PER_PLAYER {
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
            processed += 1;
        }

        if processed == SERVER_USERCMD_BUDGET_PER_PLAYER && !queued.commands.is_empty() {
            debug!(
                "server usercmd budget hit for {player}: {} queued",
                queued.commands.len()
            );
        }
    }
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
    )>,
) {
    for (command_buffer, position, velocity, look, controller_state, player_state, mut snapshot) in
        &mut players
    {
        snapshot.server_tick = tick.0;
        snapshot.last_processed_sequence = command_buffer.last_processed_sequence;
        snapshot.last_processed_buttons = command_buffer.last_buttons;
        snapshot.position = **position;
        snapshot.velocity = **velocity;
        snapshot.look = Vec2::new(look.yaw, look.pitch);
        snapshot.state = NetAhoyMoveState::from_controller_state(controller_state);
        snapshot.weapon = player_state.weapon;
        snapshot.rockets = player_state.rockets;
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
            },
        );
    }
}
