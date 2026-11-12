//! What the server publishes each tick: every player's pose, the owner's snapshot and the
//! bodies' poses.

use avian3d::prelude::*;
use bevy::prelude::*;

use crate::{
    game::{PlayerGame, PlayerState},
    history::History,
    protocol::*,
};

use super::{
    ServerTick,
    timeline::{RollbackStats, ServerCommandBuffer, confirmed_frame, published_flags},
};

pub(super) fn publish_authoritative_player_snapshots<G: PlayerGame>(
    mut commands: Commands,
    tick: Res<ServerTick>,
    tick_rate: Res<TickRate>,
    mut stats: ResMut<RollbackStats>,
    mut players: Query<(
        Entity,
        &ServerCommandBuffer<G>,
        &PlayerState<G>,
        &mut OwnerSnapshot<G>,
        Option<(&mut PlayerSnapshot, &mut History<PlayerSnapshot>)>,
    )>,
) {
    if tick.0.saturating_sub(stats.last_report_tick) >= tick_rate.ticks(30.0) {
        if stats.rollbacks != 0 || stats.floors != 0 || stats.reanchors != 0 {
            info!(
                "input rollback, last 30s: {} rollbacks, {} re-stepped ticks, max depth {}, {} external-edit floors, {} re-anchors ({} skipped)",
                stats.rollbacks,
                stats.restepped_ticks,
                stats.max_depth,
                stats.floors,
                stats.reanchors,
                stats.reanchor_skipped
            );
        }
        *stats = RollbackStats {
            last_report_tick: tick.0,
            ..default()
        };
    }

    for (entity, command_buffer, state, mut snapshot, published) in &mut players {
        let flags = published_flags(command_buffer, tick.0, *tick_rate);
        let sample = PlayerSnapshot {
            server_tick: tick.0,
            position: state.moves.position,
            look: crate::pmove::look(state.moves.orientation),
            move_flags: MoveFlags::from_moves(&state.moves),
            flags,
        };
        // The lag-comp pose is the published sample: clients record the same values into the
        // same history, so a lag-compensated hit judges identically on both peers.
        match published {
            Some((mut published, mut history)) => {
                *published = sample;
                history.record(tick.0, sample);
            }
            // Inserted with its first value, so no placeholder pose ever reaches a client.
            None => {
                let mut history = History::new(*tick_rate);
                history.record(tick.0, sample);
                commands.entity(entity).insert((sample, history));
            }
        }

        // The owner is judged against real input only. While a command is missing the head
        // runs on guesses, so publish the last fully real state (`confirmed_frame`); the client
        // restores to it and replays everything after the ack, guessed ticks included.
        snapshot.server_tick = sample.server_tick;
        snapshot.last_processed_sequence = command_buffer.last_processed_sequence;
        snapshot.last_processed_buttons = command_buffer.last_processed_buttons;
        snapshot.input_lead = command_buffer
            .lead
            .report(tick.0, tick_rate.ticks(INPUT_LEAD_WINDOW_SECONDS));
        match confirmed_frame(command_buffer) {
            Some(confirmed) => {
                snapshot.provisional = true;
                snapshot.moves = confirmed.state.moves;
                snapshot.game = confirmed.state.game;
            }
            None => {
                snapshot.provisional = false;
                snapshot.moves = state.moves;
                snapshot.game = state.game;
            }
        }
    }
}

/// Writes each body's [`BodySnapshot`] from its physics: one tick-stamped sample on the wire,
/// written only on change, so a parked body goes quiet. Its history gets the pose every tick:
/// a client's history fills the wire's silence with the held value the same way.
pub(super) fn publish_body_snapshots(
    tick: Res<ServerTick>,
    mut bodies: Query<(
        &mut BodySnapshot,
        &mut History<BodySnapshot>,
        &Position,
        &Rotation,
    )>,
) {
    for (mut snapshot, mut history, position, rotation) in &mut bodies {
        let since_previous = if snapshot.tick == 0 {
            0
        } else {
            tick.0
                .saturating_sub(snapshot.tick)
                .min(u64::from(u16::MAX)) as u16
        };
        let next = BodySnapshot {
            tick: tick.0,
            position: position.0,
            rotation: rotation.0,
            since_previous,
        };
        // Compare the pose only, so an unchanged one stays unwritten.
        if (snapshot.position, snapshot.rotation) != (next.position, next.rotation) {
            *snapshot = next;
        }
        history.record(tick.0, next);
    }
}
