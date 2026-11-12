//! Commands arriving: each player's queue, filled from the packets its client sends.

use std::collections::VecDeque;

use bevy::prelude::*;
use bevy_replicon::prelude::*;

use crate::{game::PlayerGame, protocol::*};

use super::{
    timeline::{INPUT_LEAD_RESYNC_AHEAD_MIN_TICKS, ServerCommandBuffer},
    visibility::PlayerOwner,
};

/// How much queued input the server keeps per player (s): the client's replay bound
/// (`MAX_REPLAY_SECONDS`). Past it the oldest commands drop, so a broken or malicious sender
/// can't grow memory or make the server walk stale history.
pub(super) const SERVER_CMD_QUEUE_SECONDS: f64 = 2.5;

/// The most commands the queue keeps, at any rate ([`TickRate::capped_ticks`]): the client's
/// replay cap (`MAX_REPLAY_COMMANDS`).
pub(crate) const SERVER_CMD_QUEUE_MAX_COMMANDS: u64 = 50;

/// The fewest commands the queue keeps: a client [`INPUT_LEAD_RESYNC_AHEAD_MIN_TICKS`] ahead,
/// where the re-anchor starts, has that many queued past the one due, and dropping the oldest
/// would drop that one.
pub(crate) const SERVER_CMD_QUEUE_MIN_COMMANDS: u64 = INPUT_LEAD_RESYNC_AHEAD_MIN_TICKS as u64 + 1;

/// How many commands the queue keeps at `rate`: [`SERVER_CMD_QUEUE_SECONDS`] of them, at most
/// [`SERVER_CMD_QUEUE_MAX_COMMANDS`] and at least [`SERVER_CMD_QUEUE_MIN_COMMANDS`].
pub(crate) fn queue_commands(rate: TickRate) -> u64 {
    rate.capped_ticks(SERVER_CMD_QUEUE_SECONDS, SERVER_CMD_QUEUE_MAX_COMMANDS)
        .max(SERVER_CMD_QUEUE_MIN_COMMANDS)
}

/// One player's commands waiting for their ticks: the owner's packets fill it, and a game queues
/// a bot's commands into it with [`Self::push`].
#[derive(Component, Clone, Debug, Default)]
pub struct QueuedUserCmds {
    /// The waiting commands, oldest first.
    pub(crate) commands: VecDeque<UserCmd>,
    /// Freshest sequence any packet carried since the last tick, stale or not: the arrival-lead
    /// sample. Bot commands go straight into `commands` and never set it, so bots are never paced.
    pub(super) newest_arrival: Option<u32>,
}

impl QueuedUserCmds {
    /// Queues `command` for a bot the game steps on commands of its own, after the ones queued
    /// already: one a tick, its sequence one more each. Never paced, unlike a client's.
    pub fn push(&mut self, command: UserCmd) {
        self.commands.push_back(command);
    }

    /// Queues the packet's commands newer than `last_processed_sequence` and not queued yet, in
    /// sequence order, keeping the newest [`queue_commands`] at `rate`.
    pub(crate) fn push_packet(
        &mut self,
        packet: &UserCmdPacket,
        last_processed_sequence: u32,
        rate: TickRate,
    ) {
        for command in &packet.commands {
            if command.sequence != 0
                && self
                    .newest_arrival
                    .is_none_or(|newest| sequence_is_newer(command.sequence, newest))
            {
                self.newest_arrival = Some(command.sequence);
            }
        }
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

        let limit = queue_commands(rate) as usize;
        while self.commands.len() > limit {
            self.commands.pop_front();
        }
    }

    pub(super) fn take_sequence(&mut self, sequence: u32) -> Option<UserCmd> {
        let index = self
            .commands
            .iter()
            .position(|command| command.sequence == sequence)?;
        self.commands.remove(index)
    }
}

pub(super) fn queue_player_commands<G: PlayerGame>(
    packet: On<FromClient<UserCmdPacket>>,
    tick_rate: Res<TickRate>,
    mut players: Query<(&PlayerOwner, &ServerCommandBuffer<G>, &mut QueuedUserCmds)>,
) {
    let Some(client) = packet.client_id.entity() else {
        return;
    };

    for (owner, command_buffer, mut queued) in &mut players {
        if owner.0 == client {
            queued.push_packet(&packet, command_buffer.last_processed_sequence, *tick_rate);
            return;
        }
    }
}
