//! Takes the moves players send, runs them, and tells everyone what really
//! happened — plus a little history for lag compensation.

use std::collections::{HashMap, VecDeque};

use avian3d::prelude::*;
use bevy::prelude::*;
use bevy_ahoy::{prelude::*, CharacterLook};
use bevy_replicon::prelude::*;

use crate::{
    math::{
        ray_segment_capsule_distance, sample_buffer_at, RemoteRenderTime, RemoteSnapshotSample,
    },
    step::NetAhoyStepper,
    protocol::*,
    world::{splash_other_players, NetAhoyWorld},
};

pub const SERVER_USERCMD_BUDGET_PER_PLAYER: usize = 4;
pub const SERVER_CMD_QUEUE_CAPACITY: usize = 256;
pub const LAG_COMPENSATION_HISTORY_CAPACITY: usize = 128;

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
            .init_resource::<NetAhoyWorld>()
            .add_observer(queue_player_commands)
            .add_systems(FixedFirst, advance_server_tick)
            .add_systems(
                FixedPreUpdate,
                apply_player_commands.in_set(ServerNetAhoySystems::ApplyCommands),
            )
            .add_systems(
                FixedPreUpdate,
                splash_other_players.after(ServerNetAhoySystems::ApplyCommands),
            )
            .add_systems(
                FixedLast,
                (
                    publish_authoritative_player_snapshots,
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

#[derive(Resource, Debug)]
pub struct LagCompensationHistory {
    pub max_frames: usize,
    pub poses: HashMap<PlayerId, VecDeque<RemoteSnapshotSample>>,
}

impl Default for LagCompensationHistory {
    fn default() -> Self {
        Self {
            max_frames: LAG_COMPENSATION_HISTORY_CAPACITY,
            poses: HashMap::new(),
        }
    }
}

impl LagCompensationHistory {
    pub fn record(&mut self, player_id: PlayerId, sample: RemoteSnapshotSample) {
        let samples = self
            .poses
            .entry(player_id)
            .or_insert_with(|| VecDeque::with_capacity(self.max_frames));
        if samples
            .back()
            .is_some_and(|last| last.server_tick == sample.server_tick)
        {
            *samples.back_mut().unwrap() = sample;
            return;
        }

        if samples.len() == self.max_frames {
            samples.pop_front();
        }
        samples.push_back(sample);
    }

    pub fn pose_at_time(
        &self,
        player_id: PlayerId,
        server_time: RemoteRenderTime,
    ) -> Option<RemoteSnapshotSample> {
        sample_buffer_at(self.poses.get(&player_id)?, server_time)
    }

    pub fn raycast_capsules_at_time(
        &self,
        cast: LagCompensatedCapsuleCast,
    ) -> Option<LagCompensatedCapsuleHit> {
        let direction = cast.direction.try_normalize()?;

        self.poses
            .keys()
            .copied()
            .filter(|player_id| cast.ignored_player != Some(*player_id))
            .filter_map(|player_id| {
                let pose = self.pose_at_time(player_id, cast.server_time)?;
                let distance = ray_segment_capsule_distance(
                    cast.origin,
                    direction,
                    cast.max_distance,
                    pose.position - Vec3::Y * cast.half_height,
                    pose.position + Vec3::Y * cast.half_height,
                    cast.radius,
                )?;
                Some(LagCompensatedCapsuleHit {
                    player_id,
                    position: cast.origin + direction * distance,
                    distance,
                })
            })
            .min_by(|a, b| a.distance.total_cmp(&b.distance))
    }
}

#[derive(Clone, Copy, Debug)]
pub struct LagCompensatedCapsuleCast {
    pub server_time: RemoteRenderTime,
    pub origin: Vec3,
    pub direction: Vec3,
    pub max_distance: f32,
    pub radius: f32,
    pub half_height: f32,
    pub ignored_player: Option<PlayerId>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LagCompensatedCapsuleHit {
    pub player_id: PlayerId,
    pub position: Vec3,
    pub distance: f32,
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
    mut players: Query<(Entity, &mut ServerCommandBuffer, &mut QueuedUserCmds), With<PlayerOwner>>,
    mut stepper: NetAhoyStepper,
) {
    for (player, mut command_buffer, mut queued) in &mut players {
        let mut processed = 0;

        while processed < SERVER_USERCMD_BUDGET_PER_PLAYER {
            let Some(command) = queued.pop_next() else {
                stepper.clear_transient(player);
                break;
            };

            if let Err(err) = stepper.step(player, command, command_buffer.last_buttons) {
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
        &mut AhoySnapshot,
    )>,
) {
    for (command_buffer, position, velocity, look, controller_state, mut snapshot) in &mut players {
        snapshot.server_tick = tick.0;
        snapshot.last_processed_sequence = command_buffer.last_processed_sequence;
        snapshot.last_processed_buttons = command_buffer.last_buttons;
        snapshot.position = **position;
        snapshot.velocity = **velocity;
        snapshot.look = Vec2::new(look.yaw, look.pitch);
        snapshot.state = NetAhoyMoveState::from_controller_state(controller_state);
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
