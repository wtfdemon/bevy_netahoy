//! Takes the moves players send, runs them, and tells everyone what really
//! happened — plus a little history for lag compensation.

use std::collections::VecDeque;

use avian3d::prelude::*;
use bevy::prelude::*;
use bevy_ahoy::{CharacterLook, prelude::*};
use bevy_replicon::prelude::*;

use crate::{
    math::{LagCompensationHistory, RemoteFlags, RemoteRenderTime, RemoteSnapshotSample},
    player::{NetAhoyPlayerEvents, NetAhoyPlayerState, RocketId, process_rocket_events},
    protocol::*,
    step::{AhoyPredictionFrame, NetAhoyStepper},
};

/// Defensive queue bound, matched to the client's replay bound. Overflow
/// drops oldest input so a broken or malicious sender cannot grow memory or
/// make the server walk stale history.
pub const SERVER_CMD_QUEUE_CAPACITY: usize = 50;
/// Ticks without a processed usercmd before a player publishes
/// [`RemoteFlags::CONNECTION_INTERRUPTED`]: 1 s at 20 Hz — long enough that
/// ordinary loss bursts don't flicker the flag.
pub const CONNECTION_INTERRUPTED_TICKS: u64 = 20;
/// Enough corrected trajectory to cover the interpolation and lag-comp
/// windows without turning a stalled connection into unbounded server work.
const SERVER_INPUT_ROLLBACK_CAPACITY: usize = 64;
/// Comfortably past anything a 64-tick rollback window can re-emit.
const SEEN_SERVER_EVENTS_CAPACITY: usize = 256;

/// Event ids already drained to consumers. Input rollback re-simulates
/// received commands, and each re-simulated fire/blast pushes its event
/// again; without this filter a rollback spanning a fire double-broadcasts
/// it and double-applies damage. A replayed event is deterministic, so a
/// repeated id is always the same event — dropping it is lossless.
#[derive(Resource, Default)]
pub struct SeenServerEvents {
    fired: VecDeque<RocketId>,
    hit: VecDeque<RocketId>,
}

fn first_seen(ring: &mut VecDeque<RocketId>, id: RocketId) -> bool {
    if ring.contains(&id) {
        return false;
    }
    if ring.len() == SEEN_SERVER_EVENTS_CAPACITY {
        ring.pop_front();
    }
    ring.push_back(id);
    true
}

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
            .init_resource::<SeenServerEvents>()
            .init_resource::<RemoteSnapshotRevisions>()
            .add_visibility_filter::<PlayerOwner>()
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

#[derive(Component, Clone, Debug)]
pub struct ServerCommandBuffer {
    pub last_processed_sequence: u32,
    pub last_buttons: AhoyButtons,
    last_processed_buttons: AhoyButtons,
    last_command: Option<AhoyUserCmd>,
    last_simulated_sequence: u32,
    timeline: VecDeque<ServerInputFrame>,
    /// Movement-stick magnitude of the last processed command, published in
    /// [`RemoteSnapshotSample`] so remote viewers can tell a skid from a run.
    pub last_has_move_input: bool,
    /// Server tick when a real usercmd was last received; drives
    /// [`RemoteFlags::CONNECTION_INTERRUPTED`]. Starts at 0, so a fresh player
    /// reads as interrupted until their first command lands.
    pub last_command_tick: u64,
}

impl Default for ServerCommandBuffer {
    fn default() -> Self {
        Self {
            last_processed_sequence: 0,
            last_buttons: AhoyButtons::default(),
            last_processed_buttons: AhoyButtons::default(),
            last_command: None,
            last_simulated_sequence: 0,
            timeline: VecDeque::with_capacity(SERVER_INPUT_ROLLBACK_CAPACITY),
            last_has_move_input: false,
            last_command_tick: 0,
        }
    }
}

#[derive(Clone, Debug)]
struct ServerInputFrame {
    server_tick: u64,
    command: AhoyUserCmd,
    received: bool,
    previous_sequence: u32,
    previous_buttons: AhoyButtons,
    before: AhoyPredictionFrame,
    after: AhoyPredictionFrame,
    flags: RemoteFlags,
}

#[derive(Resource, Default)]
struct RemoteSnapshotRevisions {
    samples: Vec<(PlayerId, RemoteSnapshotSample)>,
    rollbacks: u32,
    revised_ticks: u32,
    max_depth: usize,
    last_report_tick: u64,
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

    /// The freshest queued input. Older states delivered in a join or
    /// recovery burst are obsolete; never replay them.
    pub fn pop_for_tick(&mut self) -> Option<AhoyUserCmd> {
        let newest = self.commands.pop_back();
        self.commands.clear();
        newest
    }

    fn take_sequence(&mut self, sequence: u32) -> Option<AhoyUserCmd> {
        let index = self
            .commands
            .iter()
            .position(|command| command.sequence == sequence)?;
        self.commands.remove(index)
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

fn extrapolate_usercmd(
    mut command: AhoyUserCmd,
    sequence: u32,
    min_rewind_tick: u64,
    server_tick: u64,
    interrupted: bool,
) -> AhoyUserCmd {
    command.sequence = sequence;
    let seen = RemoteRenderTime::new(command.seen_server_tick, command.seen_alpha);
    let seen = RemoteRenderTime::from_ticks_f64(seen.as_ticks_f64() + 1.0)
        .clamp_ticks(min_rewind_tick, server_tick);
    command.seen_server_tick = seen.tick;
    command.seen_alpha = seen.alpha;
    // Held state is a valid guess. These are one-shot payloads: inventing
    // them would make rollback repeat an event that never belonged to this
    // input tick. The real payload is restored if this slot is revised.
    command.fire = None;
    if interrupted {
        command.movement = Vec2::ZERO;
        command.buttons = AhoyButtons::empty();
    }
    command
}

fn sanitize_usercmd(
    player: Entity,
    command: &mut AhoyUserCmd,
    min_rewind_tick: u64,
    server_tick: u64,
) {
    let seen = RemoteRenderTime::new(command.seen_server_tick, command.seen_alpha)
        .clamp_ticks(min_rewind_tick, server_tick);
    if seen.tick != command.seen_server_tick {
        info!(
            "{player} cmd {} seen clamped {}:{:.3} -> {}:{:.3} (server tick {})",
            command.sequence,
            command.seen_server_tick,
            command.seen_alpha,
            seen.tick,
            seen.alpha,
            server_tick
        );
    }
    command.seen_server_tick = seen.tick;
    command.seen_alpha = seen.alpha;

    // Trust boundary: one NaN pose poisons history and other players.
    if !(command.movement.is_finite() && command.look.is_finite()) {
        command.movement = Vec2::ZERO;
        command.look = Vec2::ZERO;
        command.fire = None;
    }
    if let Some(fire) = &mut command.fire {
        if fire.frac.is_finite() && fire.look.x.is_finite() && fire.look.y.is_finite() {
            fire.frac = fire.frac.clamp(0.0, 1.0);
            fire.look.y = fire.look.y.clamp(-1.5, 1.5);
        } else {
            command.fire = None;
        }
    }
}

fn frame_flags(frame: &AhoyPredictionFrame, mut flags: RemoteFlags) -> RemoteFlags {
    flags.set(
        RemoteFlags::MOVE_INPUT,
        frame.command.movement.length() > 0.1,
    );
    flags
}

/// The flags published for `tick`. The frame stepped this tick already
/// carries the full set (`frame_flags` plus faked/interrupted) — the same
/// bits a rollback revision republishes — so the normal path can't disagree
/// with the revised path about what a pose was doing. No frame means nothing
/// stepped this tick (missing components); only the connection bits are
/// knowable then.
fn published_flags(command_buffer: &ServerCommandBuffer, tick: u64) -> RemoteFlags {
    match command_buffer.timeline.back() {
        Some(frame) if frame.server_tick == tick => frame.flags,
        _ => {
            let interrupted = tick.saturating_sub(command_buffer.last_command_tick)
                > CONNECTION_INTERRUPTED_TICKS;
            let mut flags = RemoteFlags::empty();
            flags.set(RemoteFlags::MOVE_INPUT, command_buffer.last_has_move_input);
            flags.set(RemoteFlags::CONNECTION_INTERRUPTED, interrupted);
            flags
        }
    }
}

fn sample_from_input_frame(frame: &ServerInputFrame) -> RemoteSnapshotSample {
    RemoteSnapshotSample {
        server_tick: frame.server_tick,
        position: frame.after.position,
        velocity: frame.after.velocity,
        look: frame.after.look,
        state: frame.after.state,
        flags: frame.flags,
    }
}

/// Drop the oldest frame from the rollback window. A guessed slot that is
/// leaving can never be revised, so it is acknowledged as-is: the ack must
/// not wait forever for a command whose tick is gone.
fn retire_front(command_buffer: &mut ServerCommandBuffer) {
    if let Some(front) = command_buffer.timeline.front_mut()
        && !front.received
    {
        front.received = true;
        advance_confirmation(command_buffer);
    }
    command_buffer.timeline.pop_front();
}

fn advance_confirmation(command_buffer: &mut ServerCommandBuffer) {
    if command_buffer.last_processed_sequence == 0 {
        let Some(first) = command_buffer.timeline.front().filter(|frame| frame.received) else {
            return;
        };
        command_buffer.last_processed_sequence = first.command.sequence;
        command_buffer.last_processed_buttons = first.command.buttons;
    }

    loop {
        let expected = command_buffer.last_processed_sequence.wrapping_add(1);
        let Some(frame) = command_buffer
            .timeline
            .iter()
            .find(|frame| frame.command.sequence == expected && frame.received)
        else {
            break;
        };
        command_buffer.last_processed_sequence = expected;
        command_buffer.last_processed_buttons = frame.command.buttons;
    }
}

fn apply_player_commands(
    tick: Res<ServerTick>,
    mut players: Query<(
        Entity,
        &PlayerId,
        &mut ServerCommandBuffer,
        &mut QueuedUserCmds,
    )>,
    mut revisions: ResMut<RemoteSnapshotRevisions>,
    mut stepper: NetAhoyStepper,
    mut seen: ResMut<SeenServerEvents>,
) {
    let min_rewind_tick = tick
        .0
        .saturating_sub(crate::math::LAG_COMPENSATION_HISTORY_CAPACITY as u64);

    for (player, player_id, mut command_buffer, mut queued) in &mut players {
        // Commands that arrive after their guessed tick revise that slot.
        // Packet backups may also repeat already-real slots; consume those as
        // duplicates instead of letting them pollute the live queue.
        let mut earliest_revision = None;
        let mut remaining = VecDeque::with_capacity(queued.commands.len());
        while let Some(mut command) = queued.commands.pop_front() {
            if let Some(index) = command_buffer
                .timeline
                .iter()
                .position(|frame| frame.command.sequence == command.sequence)
            {
                if !command_buffer.timeline[index].received {
                    sanitize_usercmd(player, &mut command, min_rewind_tick, tick.0);
                    command_buffer.timeline[index].command = command;
                    command_buffer.timeline[index].received = true;
                    command_buffer.last_command_tick = tick.0;
                    earliest_revision = Some(earliest_revision.map_or(index, |old: usize| old.min(index)));
                }
            } else if command_buffer.last_simulated_sequence == 0
                || sequence_is_newer(command.sequence, command_buffer.last_simulated_sequence)
            {
                remaining.push_back(command);
            }
        }
        queued.commands = remaining;

        if let Some(first) = earliest_revision {
            let depth = command_buffer.timeline.len() - first;
            revisions.rollbacks += 1;
            revisions.revised_ticks += depth as u32;
            revisions.max_depth = revisions.max_depth.max(depth);
            let rewind = command_buffer.timeline[first].before.clone();
            stepper.restore_frame(player, &rewind);
            let mut previous_sequence = command_buffer.timeline[first].previous_sequence;
            let mut previous_buttons = command_buffer.timeline[first].previous_buttons;
            let mut previous_command = rewind.command;

            for index in first..command_buffer.timeline.len() {
                let slot_tick = command_buffer.timeline[index].server_tick;
                let received = command_buffer.timeline[index].received;
                let interrupted = command_buffer.timeline[index]
                    .flags
                    .contains(RemoteFlags::CONNECTION_INTERRUPTED);
                let command = if received {
                    command_buffer.timeline[index].command
                } else {
                    extrapolate_usercmd(
                        previous_command,
                        command_buffer.timeline[index].command.sequence,
                        min_rewind_tick,
                        slot_tick,
                        interrupted,
                    )
                };
                let before = stepper
                    .capture_frame(player, previous_command)
                    .unwrap_or_else(|| rewind.clone());
                if let Err(err) =
                    stepper.player_move(player, command, previous_sequence, previous_buttons)
                {
                    warn!(
                        "failed to replay server KCC for {player} command {}: {err}",
                        command.sequence
                    );
                }
                let Some(after) = stepper.capture_frame(player, command) else {
                    break;
                };
                let old_flags = command_buffer.timeline[index].flags;
                let slot = &mut command_buffer.timeline[index];
                slot.command = command;
                slot.previous_sequence = previous_sequence;
                slot.previous_buttons = previous_buttons;
                slot.before = before;
                slot.after = after;
                slot.flags = frame_flags(&slot.after, old_flags);
                slot.flags.set(RemoteFlags::FAKED_INPUT, !received);
                revisions
                    .samples
                    .push((*player_id, sample_from_input_frame(slot)));
                previous_sequence = command.sequence;
                previous_buttons = command.buttons;
                previous_command = command;
            }

            if let Some(last_command) = command_buffer.timeline.back().map(|last| last.command) {
                command_buffer.last_simulated_sequence = last_command.sequence;
                command_buffer.last_buttons = last_command.buttons;
                command_buffer.last_command = Some(last_command);
                command_buffer.last_has_move_input = last_command.movement.length() > 0.1;
            }
            advance_confirmation(&mut command_buffer);
        }

        let expected_sequence = command_buffer.last_simulated_sequence.wrapping_add(1);
        let received = if command_buffer.last_simulated_sequence == 0 {
            queued.pop_for_tick()
        } else {
            queued.take_sequence(expected_sequence)
        };
        let mut command = if let Some(command) = received {
            command_buffer.last_command_tick = tick.0;
            command
        } else if let Some(command) = command_buffer.last_command {
            // The wire supplied no new intent, but simulation time still
            // advances. Repeat the last input; only real commands advance
            // the acknowledgement sequence.
            extrapolate_usercmd(
                command,
                expected_sequence,
                min_rewind_tick,
                tick.0,
                // A short stall holds intent; a dead client eventually
                // becomes neutral while gravity and timers keep stepping.
                tick.0.saturating_sub(command_buffer.last_command_tick)
                    > CONNECTION_INTERRUPTED_TICKS,
            )
        } else {
            stepper.clear_transient(player);
            continue;
        };

        if received.is_some() {
            sanitize_usercmd(player, &mut command, min_rewind_tick, tick.0);
        }

        let before = stepper.capture_frame(player, command_buffer.last_command.unwrap_or(command));
        let previous_sequence = command_buffer.last_simulated_sequence;
        let previous_buttons = command_buffer.last_buttons;

        if let Err(err) =
            stepper.player_move(player, command, previous_sequence, previous_buttons)
        {
            warn!(
                "failed to step server KCC for {player} command {}: {err}",
                command.sequence
            );
        }

        if let (Some(before), Some(after)) = (before, stepper.capture_frame(player, command)) {
            let interrupted = tick.0.saturating_sub(command_buffer.last_command_tick)
                > CONNECTION_INTERRUPTED_TICKS;
            let mut flags = RemoteFlags::empty();
            flags.set(RemoteFlags::CONNECTION_INTERRUPTED, interrupted);
            flags.set(RemoteFlags::FAKED_INPUT, received.is_none());
            flags = frame_flags(&after, flags);
            command_buffer.timeline.push_back(ServerInputFrame {
                server_tick: tick.0,
                command,
                received: received.is_some(),
                previous_sequence,
                previous_buttons,
                before,
                after,
                flags,
            });
            while command_buffer.timeline.len() > SERVER_INPUT_ROLLBACK_CAPACITY {
                retire_front(&mut command_buffer);
            }
        }
        command_buffer.last_simulated_sequence = command.sequence;
        command_buffer.last_buttons = command.buttons;
        command_buffer.last_has_move_input = command.movement.length() > 0.1;
        command_buffer.last_command = Some(command);
        advance_confirmation(&mut command_buffer);
    }

    // Same system as the steps that pushed them, so repeats are gone before
    // any `.after(ApplyCommands)` consumer (damage, broadcast) reads the
    // outbox.
    let events = stepper.events_mut();
    events.fired.retain(|fired| first_seen(&mut seen.fired, fired.id));
    events.hit.retain(|hit| first_seen(&mut seen.hit, hit.id));
}

fn publish_authoritative_player_snapshots(
    tick: Res<ServerTick>,
    mut frames: MessageWriter<ToClients<RemoteSnapshotFrame>>,
    mut revisions: ResMut<RemoteSnapshotRevisions>,
    mut history: ResMut<LagCompensationHistory>,
    mut players: Query<(
        &PlayerId,
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
    revisions
        .samples
        .sort_by_key(|(_, sample)| sample.server_tick);
    for (player_id, sample) in core::mem::take(&mut revisions.samples) {
        history.record(player_id, sample);
        frames.write(ToClients {
            targets: SendTargets::CLIENTS_ONLY,
            message: RemoteSnapshotFrame {
                server_tick: sample.server_tick,
                samples: vec![(player_id, sample)],
            },
        });
    }
    if tick.0.saturating_sub(revisions.last_report_tick) >= 30 * FIXED_TIMESTEP_HZ as u64 {
        if revisions.rollbacks != 0 {
            info!(
                "input rollback, last 30s: {} rollbacks, {} revised ticks, max depth {}",
                revisions.rollbacks, revisions.revised_ticks, revisions.max_depth
            );
        }
        revisions.rollbacks = 0;
        revisions.revised_ticks = 0;
        revisions.max_depth = 0;
        revisions.last_report_tick = tick.0;
    }

    let mut frame = RemoteSnapshotFrame {
        server_tick: tick.0,
        samples: Vec::new(),
    };
    for (
        player_id,
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
        // Q3's EF_CONNECTION. After one second, fallback input becomes neutral;
        // publish a quiet tangent while gravity keeps the KCC stepping.
        let interrupted =
            tick.0.saturating_sub(command_buffer.last_command_tick) > CONNECTION_INTERRUPTED_TICKS;
        let flags = published_flags(command_buffer, tick.0);
        let sample = RemoteSnapshotSample {
            server_tick: tick.0,
            position: **position,
            velocity: if interrupted { Vec3::ZERO } else { **velocity },
            look: Vec2::new(look.yaw, look.pitch),
            state: NetAhoyMoveState::from_controller_state(controller_state),
            flags,
        };
        player_snapshot.0 = sample;
        frame.samples.push((*player_id, sample));
        // The lag-comp pose IS the published sample: clients mirror this
        // history from the same bytes, so anything judged at a seen time
        // (hitboxes, flags) reads identically on both peers.
        history.record(*player_id, sample);

        snapshot.server_tick = sample.server_tick;
        snapshot.provisional = command_buffer.timeline.iter().any(|frame| !frame.received);
        snapshot.last_processed_sequence = command_buffer.last_processed_sequence;
        snapshot.last_processed_buttons = command_buffer.last_processed_buttons;
        snapshot.position = sample.position;
        snapshot.velocity = sample.velocity;
        snapshot.look = sample.look;
        snapshot.state = sample.state;
        snapshot.carry = NetAhoyCarryState::from_controller_state(controller_state);
        snapshot.player_state = *player_state;
    }

    if !frame.samples.is_empty() {
        frames.write(ToClients {
            targets: SendTargets::CLIENTS_ONLY,
            message: frame,
        });
    }
}

/// Keep each replicated body's [`BodySnapshot`] in sync with its physics, so
/// the pose crosses the wire as one atomic, tick-stamped sample instead of
/// live engine components. Only writes on change, so a parked body goes quiet.
///
/// A sleeping body publishes ZERO velocity whatever its components hold: the
/// silence that follows is only safe if the last sample says "at rest" —
/// the client's history preserves rest across a silent span, but a stale
/// non-zero velocity would be dead-reckoned across it instead (and fed to
/// the KCC as platform carry).
fn publish_body_snapshots(
    tick: Res<ServerTick>,
    mut bodies: Query<(
        &mut BodySnapshot,
        &Position,
        &Rotation,
        &LinearVelocity,
        &AngularVelocity,
        Has<Sleeping>,
    )>,
) {
    for (mut snapshot, position, rotation, linear_velocity, angular_velocity, asleep) in
        &mut bodies
    {
        let next = BodySnapshot {
            tick: tick.0,
            position: position.0,
            rotation: rotation.0,
            linear_velocity: if asleep { Vec3::ZERO } else { linear_velocity.0 },
            angular_velocity: if asleep { Vec3::ZERO } else { angular_velocity.0 },
        };
        // Compare everything but the tick so an unchanged pose stays unwritten.
        if (BodySnapshot {
            tick: snapshot.tick,
            ..next
        }) != *snapshot
        {
            *snapshot = next;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prediction_frame(command: AhoyUserCmd) -> AhoyPredictionFrame {
        AhoyPredictionFrame {
            command,
            position: Vec3::ZERO,
            velocity: Vec3::ZERO,
            look: Vec2::ZERO,
            state: NetAhoyMoveState::default(),
            controller_state: CharacterControllerState::default(),
            accumulated_input: Default::default(),
            player_state: NetAhoyPlayerState::default(),
        }
    }

    fn input_frame(sequence: u32, received: bool) -> ServerInputFrame {
        let command = AhoyUserCmd {
            sequence,
            ..default()
        };
        let frame = prediction_frame(command);
        ServerInputFrame {
            server_tick: u64::from(sequence),
            command,
            received,
            previous_sequence: sequence.saturating_sub(1),
            previous_buttons: AhoyButtons::empty(),
            before: frame.clone(),
            after: frame,
            flags: RemoteFlags::empty(),
        }
    }

    #[test]
    fn recovery_steps_the_freshest_command_only() {
        let mut queued = QueuedUserCmds::default();
        queued.commands.extend((1..=5).map(|sequence| AhoyUserCmd {
            sequence,
            ..default()
        }));

        assert_eq!(queued.pop_for_tick().unwrap().sequence, 5);
        assert!(queued.commands.is_empty());
    }

    #[test]
    fn retiring_a_guessed_frame_acknowledges_it() {
        let mut buffer = ServerCommandBuffer::default();
        buffer.timeline.extend([
            input_frame(10, true),
            input_frame(11, false),
            input_frame(12, true),
        ]);
        advance_confirmation(&mut buffer);
        assert_eq!(buffer.last_processed_sequence, 10);

        retire_front(&mut buffer);
        assert_eq!(buffer.last_processed_sequence, 10);
        retire_front(&mut buffer);
        assert_eq!(buffer.last_processed_sequence, 12);
        assert_eq!(buffer.timeline.len(), 1);
    }

    #[test]
    fn extrapolated_usercmd_repeats_the_last_input() {
        let command = AhoyUserCmd {
            sequence: 7,
            movement: Vec2::X,
            look: Vec2::new(0.5, -0.25),
            buttons: AhoyButtons::JUMP,
            fire: Some(SubtickFire {
                frac: 0.5,
                look: Vec2::Y,
            }),
            seen_server_tick: 20,
            seen_alpha: 0.25,
        };
        let next = extrapolate_usercmd(command, 8, 0, 100, false);
        assert_eq!(next.sequence, 8);
        assert_eq!(next.movement, command.movement);
        assert_eq!(next.look, command.look);
        assert_eq!(next.buttons, command.buttons);
        assert_eq!(next.fire, None);
        assert_eq!(next.seen_server_tick, 21);
        assert_eq!(next.seen_alpha, 0.25);

        let interrupted = extrapolate_usercmd(next, 9, 0, 100, true);
        assert_eq!(interrupted.movement, Vec2::ZERO);
        assert!(interrupted.buttons.is_empty());
        assert_eq!(interrupted.fire, None);
    }

    #[test]
    fn confirmation_waits_for_and_then_crosses_a_revised_gap() {
        let mut buffer = ServerCommandBuffer::default();
        buffer.timeline.extend([
            input_frame(10, true),
            input_frame(11, false),
            input_frame(12, true),
        ]);

        advance_confirmation(&mut buffer);
        assert_eq!(buffer.last_processed_sequence, 10);

        buffer.timeline[1].received = true;
        advance_confirmation(&mut buffer);
        assert_eq!(buffer.last_processed_sequence, 12);
    }

    #[test]
    fn published_flags_come_from_the_stepped_frame() {
        let mut buffer = ServerCommandBuffer::default();
        let mut frame = input_frame(10, true);
        frame.flags = RemoteFlags::MOVE_INPUT;
        buffer.timeline.push_back(frame);
        buffer.last_command_tick = 10;

        // The tick that stepped: the frame's full flag set, guard included.
        assert_eq!(
            published_flags(&buffer, 10),
            RemoteFlags::MOVE_INPUT
        );
        // A tick nothing stepped: only what the buffer itself knows.
        let fallback = published_flags(&buffer, 11);
        assert!(!fallback.contains(RemoteFlags::MOVE_INPUT));
        assert!(!fallback.contains(RemoteFlags::CONNECTION_INTERRUPTED));
        assert!(
            published_flags(&buffer, 11 + CONNECTION_INTERRUPTED_TICKS)
                .contains(RemoteFlags::CONNECTION_INTERRUPTED)
        );
    }

    #[test]
    fn seen_server_events_drop_repeats_and_bound_memory() {
        let mut ring = VecDeque::new();
        let id = |seq| RocketId {
            owner: crate::protocol::PlayerId(1),
            fired_sequence: seq,
        };
        assert!(first_seen(&mut ring, id(7)));
        assert!(!first_seen(&mut ring, id(7)), "rollback re-push must drop");
        for seq in 0..SEEN_SERVER_EVENTS_CAPACITY as u32 {
            first_seen(&mut ring, id(1000 + seq));
        }
        assert_eq!(ring.len(), SEEN_SERVER_EVENTS_CAPACITY);
        assert!(first_seen(&mut ring, id(7)), "evicted ids may recur");
    }
}
