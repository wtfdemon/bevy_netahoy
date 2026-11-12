//! Each player's input timeline as plain data: the slots stepped on real or guessed commands,
//! the rollback when a late command lands, and the ack.

use std::collections::VecDeque;

use bevy::prelude::*;

use crate::{
    client::INPUT_LEAD_BAND,
    events::Outbox,
    game::PlayerGame,
    history::HISTORY_SECONDS,
    poses::{RemoteFlags, RemoteRenderTime},
    protocol::*,
    stepper::PredictionFrame,
};

use super::intake::{QueuedUserCmds, queue_commands};

/// Time without a received usercmd before a player publishes
/// [`RemoteFlags::CONNECTION_INTERRUPTED`] (s): long enough that loss bursts don't flicker it.
pub(super) const CONNECTION_INTERRUPTED_SECONDS: f64 = 1.0;

/// How far back the server keeps each player's input timeline (s): enough corrected trajectory
/// to cover the interpolation and lag-comp windows without turning a stalled connection into
/// unbounded server work.
const SERVER_INPUT_ROLLBACK_SECONDS: f64 = 3.2;

/// The most slots the timeline keeps, at any rate ([`TickRate::capped_ticks`]): a late command
/// re-steps at most this many ticks in one go.
const SERVER_INPUT_ROLLBACK_MAX_TICKS: u64 = 64;

/// How long a guessed slot waits for its real command (s), before the guess is acknowledged as
/// the input that ran: else the owner's ack, and every correction, stalls behind one lost
/// command for the whole rollback window. Longer than a stream link's retransmit stall
/// (1.5-2x RTT) plus jitter; on a datagram link the redundant tail fills holes within an RTT.
pub(super) const GUESS_RETIRE_SECONDS: f64 = 0.8;

/// How early even the latest command in a full lead window may land before the server
/// re-anchors (s). Past it the client runs far ahead (a server hitch dropped ticks, or its clock
/// outran the nudge), and that backlog is pure input latency: skip it. Well past the client's
/// nudge band, so it only fires when nudging can't keep up. Never under
/// [`INPUT_LEAD_RESYNC_AHEAD_MIN_TICKS`], however few ticks it rounds to.
pub(super) const INPUT_LEAD_RESYNC_AHEAD_SECONDS: f64 = 0.4;

/// The fewest ticks the re-anchor waits for, at rates where its seconds round to fewer (10 and
/// 11 Hz): two past the top of the client's [`INPUT_LEAD_BAND`]. A client clock that outruns the
/// server's by d settles where the nudge slows it by d, at a lead of 3 + (d - 1%) / 2% ticks: 4
/// at 3% drift, which a 5-tick re-anchor (10 to 13 Hz) never reaches, and 4.5 at the client's
/// whole 4% slowdown, which touches 5 and re-anchors now and then. Real clocks drift far less.
pub(crate) const INPUT_LEAD_RESYNC_AHEAD_MIN_TICKS: i32 = INPUT_LEAD_BAND.1 + 2;

/// One player's input timeline on the server: the commands stepped on, real or guessed, kept
/// for rollback, and what the owner is acknowledged.
#[derive(Component, Clone, Debug)]
pub(crate) struct ServerCommandBuffer<G: PlayerGame> {
    pub(crate) last_processed_sequence: u32,
    pub(crate) last_buttons: Buttons,
    pub(super) last_processed_buttons: Buttons,
    last_command: Option<UserCmd>,
    last_simulated_sequence: u32,
    /// Highest sequence ever received from this client. A guessed slot below it is late or
    /// lost; one above it the client hasn't sent yet, and must never be retired: acking unsent
    /// sequences would make every real command after a clock hitch a duplicate of an acked slot.
    newest_received_sequence: u32,
    pub(crate) timeline: VecDeque<ServerInputFrame<G>>,
    /// The sequence the next slot runs, when a re-anchor skipped past
    /// `last_simulated_sequence + 1`.
    skip_to: Option<u32>,
    pub(crate) lead: InputLeadWindow,
    /// Whether the last stepped command moved the stick, published as
    /// [`RemoteFlags::MOVE_INPUT`] so remote viewers can tell a skid from a run.
    pub(crate) last_has_move_input: bool,
    /// Server tick a real usercmd was last received on, for
    /// [`RemoteFlags::CONNECTION_INTERRUPTED`]. Starts at 0: a new player reads as interrupted
    /// until its first command lands.
    pub(crate) last_command_tick: u64,
    /// Its steps' events, filtered because input rollback reruns its slots (with it off the
    /// server never reruns a step). A new buffer is a new run of sequences, with nothing passed
    /// on yet.
    pub(crate) events: Outbox<G::Event>,
}

impl<G: PlayerGame> Default for ServerCommandBuffer<G> {
    fn default() -> Self {
        Self {
            last_processed_sequence: 0,
            last_buttons: Buttons::default(),
            last_processed_buttons: Buttons::default(),
            last_command: None,
            last_simulated_sequence: 0,
            newest_received_sequence: 0,
            timeline: VecDeque::new(),
            skip_to: None,
            lead: InputLeadWindow::default(),
            last_has_move_input: false,
            last_command_tick: 0,
            events: Outbox::default(),
        }
    }
}

impl<G: PlayerGame> ServerCommandBuffer<G> {
    /// The sequence the next slot carries. Slots never leave holes except across a re-anchor:
    /// the command sequence is the step's clock (cooldowns, fuses).
    fn expected_sequence(&self) -> u32 {
        self.skip_to
            .unwrap_or(self.last_simulated_sequence.wrapping_add(1))
    }
}

/// Rolling [`InputLead`] samples, one per server tick that received a command from this owner.
#[derive(Clone, Debug, Default)]
pub(crate) struct InputLeadWindow {
    samples: VecDeque<(u64, i32)>,
}

impl InputLeadWindow {
    /// Records `lead` on `tick`, keeping the last `window` ticks.
    fn record(&mut self, tick: u64, lead: i32, window: u64) {
        self.samples.push_back((tick, lead));
        while self
            .samples
            .front()
            .is_some_and(|(at, _)| at + window <= tick)
        {
            self.samples.pop_front();
        }
    }

    /// The lead over the `window` ticks up to `tick`, if any command arrived in them.
    pub(crate) fn report(&self, tick: u64, window: u64) -> Option<InputLead> {
        let mut live = self
            .samples
            .iter()
            .filter(|(at, _)| at + window > tick)
            .map(|(_, lead)| *lead);
        let first = live.next()?;
        let (mut min, mut max, mut samples) = (first, first, 1u32);
        for lead in live {
            min = min.min(lead);
            max = max.max(lead);
            samples += 1;
        }
        let clamp = |lead: i32| lead.clamp(i16::MIN.into(), i16::MAX.into()) as i16;
        Some(InputLead {
            min: clamp(min),
            max: clamp(max),
            samples: samples.min(u16::MAX.into()) as u16,
        })
    }

    fn clear(&mut self) {
        self.samples.clear();
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ServerInputFrame<G> {
    pub(crate) server_tick: u64,
    pub(crate) command: UserCmd,
    pub(crate) received: bool,
    previous_sequence: u32,
    previous_buttons: Buttons,
    before: PredictionFrame<G>,
    after: PredictionFrame<G>,
    flags: RemoteFlags,
}

/// Server-wide input rollback counters for the 30 s `input rollback` line.
#[derive(Resource, Default)]
pub(crate) struct RollbackStats {
    pub(crate) rollbacks: u32,
    /// Slots those rollbacks stepped again.
    pub(crate) restepped_ticks: u32,
    pub(crate) max_depth: usize,
    /// Guessed slots acknowledged early because an external edit landed after them.
    pub(super) floors: u32,
    /// Ahead re-anchors and the commands they skipped.
    pub(crate) reanchors: u32,
    pub(super) reanchor_skipped: u32,
    pub(super) last_report_tick: u64,
}

/// Whether something besides the command stream wrote this player since its last step: a
/// teleport, another player's blast, a game script. A rollback that restored an earlier
/// `before` would silently erase it. Exact on purpose: nothing else touches the state between
/// steps, so any difference is an edit.
fn externally_edited<G: PlayerGame>(after: &PredictionFrame<G>, live: &PredictionFrame<G>) -> bool {
    after.state != live.state
}

/// An external edit is a rollback floor: every guessed slot behind it is acknowledged as the
/// input that ran, so no rewind can cross the edit. Returns whether anything was acknowledged.
fn floor_rollback<G: PlayerGame>(command_buffer: &mut ServerCommandBuffer<G>) -> bool {
    let mut acked = false;
    for frame in &mut command_buffer.timeline {
        if !frame.received {
            frame.received = true;
            acked = true;
        }
    }
    if acked {
        advance_confirmation(command_buffer);
    }
    acked
}

fn extrapolate_usercmd(
    mut command: UserCmd,
    sequence: u32,
    min_rewind_tick: u64,
    server_tick: u64,
    interrupted: bool,
) -> UserCmd {
    command.sequence = sequence;
    let seen = RemoteRenderTime::new(command.seen_server_tick, command.seen_alpha);
    let seen = RemoteRenderTime::from_ticks_f64(seen.as_ticks_f64() + 1.0)
        .clamp_ticks(min_rewind_tick, server_tick);
    command.seen_server_tick = seen.tick;
    command.seen_alpha = seen.alpha;
    // Held state is a valid guess, a fire isn't: a guessed one is an event the input never
    // made. The real one comes back if its command replaces the guess.
    command.fire = None;
    if interrupted {
        command.movement = Vec2::ZERO;
        command.buttons = Buttons::empty();
    }
    command
}

fn sanitize_usercmd(player: Entity, command: &mut UserCmd, min_rewind_tick: u64, server_tick: u64) {
    let seen = RemoteRenderTime::new(command.seen_server_tick, command.seen_alpha)
        .clamp_ticks(min_rewind_tick, server_tick);
    if seen.tick != command.seen_server_tick {
        debug!(
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
    command.fire = command.fire.and_then(SubtickFire::sanitized);
}

fn frame_flags<G>(frame: &PredictionFrame<G>, mut flags: RemoteFlags) -> RemoteFlags {
    flags.set(
        RemoteFlags::MOVE_INPUT,
        frame.command.movement.length() > 0.1,
    );
    flags
}

/// The flags published for `tick`: those of the frame stepped on it (re-derived when a rollback
/// re-steps it), or, if nothing stepped, only what the buffer knows.
pub(super) fn published_flags<G: PlayerGame>(
    command_buffer: &ServerCommandBuffer<G>,
    tick: u64,
    rate: TickRate,
) -> RemoteFlags {
    match command_buffer.timeline.back() {
        Some(frame) if frame.server_tick == tick => frame.flags,
        _ => {
            let interrupted = tick.saturating_sub(command_buffer.last_command_tick)
                > rate.ticks(CONNECTION_INTERRUPTED_SECONDS);
            let mut flags = RemoteFlags::empty();
            flags.set(RemoteFlags::MOVE_INPUT, command_buffer.last_has_move_input);
            flags.set(RemoteFlags::CONNECTION_INTERRUPTED, interrupted);
            flags
        }
    }
}

/// Drops the oldest frame from the rollback window. A guessed slot leaving can never be
/// replaced, so it is acknowledged as it ran: the ack can't wait for a command whose tick is
/// gone. That holds even for a sequence the client hasn't sent yet (a long outage): the ack then
/// runs ahead of the client, its cue to jump its sequence forward instead of feeding commands
/// the window has left behind.
fn retire_front<G: PlayerGame>(command_buffer: &mut ServerCommandBuffer<G>) {
    if let Some(front) = command_buffer.timeline.front_mut()
        && !front.received
    {
        front.received = true;
        advance_confirmation(command_buffer);
    }
    command_buffer.timeline.pop_front();
}

fn note_received<G: PlayerGame>(command_buffer: &mut ServerCommandBuffer<G>, sequence: u32) {
    if command_buffer.newest_received_sequence == 0
        || sequence_is_newer(sequence, command_buffer.newest_received_sequence)
    {
        command_buffer.newest_received_sequence = sequence;
    }
}

/// Gives up on guessed slots older than [`GUESS_RETIRE_SECONDS`] that a newer received command
/// has overtaken: the guess becomes that sequence's input (its real command, if it comes, is a
/// duplicate and dropped) and the ack crosses it. Slots the client hasn't sent yet wait however
/// long it takes. Returns whether anything retired.
fn retire_stale_guesses<G: PlayerGame>(
    command_buffer: &mut ServerCommandBuffer<G>,
    tick: u64,
    rate: TickRate,
) -> bool {
    retire_stale_guesses_after(command_buffer, tick, rate.ticks(GUESS_RETIRE_SECONDS))
}

/// [`retire_stale_guesses`] with the wait as a parameter: 0 with rollback off, where a guess can
/// never be replaced.
fn retire_stale_guesses_after<G: PlayerGame>(
    command_buffer: &mut ServerCommandBuffer<G>,
    tick: u64,
    wait_ticks: u64,
) -> bool {
    let newest_received = command_buffer.newest_received_sequence;
    let mut retired = false;
    for frame in &mut command_buffer.timeline {
        if !frame.received
            && sequence_is_newer(newest_received, frame.command.sequence)
            && tick.saturating_sub(frame.server_tick) >= wait_ticks
        {
            frame.received = true;
            retired = true;
        }
    }
    if retired {
        advance_confirmation(command_buffer);
    }
    retired
}

/// The state the owner is reconciled against, if not the live head: the `before` frame of the
/// earliest guessed slot, the last state that is purely a function of received commands.
/// Nothing at or before it is ever re-stepped, and it is captured at the start of that tick, so
/// impulses applied between steps are in it.
pub(super) fn confirmed_frame<G: PlayerGame>(
    command_buffer: &ServerCommandBuffer<G>,
) -> Option<&PredictionFrame<G>> {
    command_buffer
        .timeline
        .iter()
        .find(|frame| !frame.received)
        .map(|frame| &frame.before)
}

/// Moves the ack across the final prefix of the timeline: every slot up to the first guess
/// still waiting for its command. Slots carry increasing sequences, so the ack is the newest in
/// that prefix, which also crosses the hole a re-anchor leaves (those sequences never run).
fn advance_confirmation<G: PlayerGame>(command_buffer: &mut ServerCommandBuffer<G>) {
    let mut acked = None;
    for frame in &command_buffer.timeline {
        if !frame.received {
            break;
        }
        acked = Some((frame.command.sequence, frame.command.buttons));
    }
    if let Some((sequence, buttons)) = acked
        && (command_buffer.last_processed_sequence == 0
            || sequence_is_newer(sequence, command_buffer.last_processed_sequence))
    {
        command_buffer.last_processed_sequence = sequence;
        command_buffer.last_processed_buttons = buttons;
    }
}

/// What one player's input timeline needs from the pmove: the server hands it the real
/// [`Stepper`](crate::stepper::Stepper), the recovery tests a toy integrator, so minutes of a bad link
/// exercise the exact bookkeeping without a physics world.
pub(crate) trait TimelineStepper<G: PlayerGame> {
    fn capture_frame(&mut self, player: Entity, command: UserCmd) -> Option<PredictionFrame<G>>;
    fn restore_frame(&mut self, player: Entity, frame: &PredictionFrame<G>);
    fn player_move(
        &mut self,
        player: Entity,
        outbox: &mut Outbox<G::Event>,
        command: UserCmd,
        previous_sequence: u32,
        previous_buttons: Buttons,
    ) -> Result<()>;
}

/// [`INPUT_LEAD_RESYNC_AHEAD_SECONDS`] in ticks, never under
/// [`INPUT_LEAD_RESYNC_AHEAD_MIN_TICKS`]: at the slowest rates the seconds round to a lead the
/// client's nudge holds a drifting clock at.
fn resync_ahead_ticks(rate: TickRate) -> i32 {
    i32::try_from(rate.ticks(INPUT_LEAD_RESYNC_AHEAD_SECONDS))
        .unwrap_or(i32::MAX)
        .max(INPUT_LEAD_RESYNC_AHEAD_MIN_TICKS)
}

/// How many sequences a re-anchor skips when even the latest arrival in the window led by `min`
/// ticks: all but [`INPUT_LEAD_TARGET`] of them, if `min` reaches [`resync_ahead_ticks`].
fn reanchor_skip(min: i32, rate: TickRate) -> Option<u32> {
    if min < resync_ahead_ticks(rate) {
        return None;
    }
    u32::try_from(min - INPUT_LEAD_TARGET)
        .ok()
        .filter(|&skip| skip > 0)
}

/// Samples this tick's arrival lead, then re-anchors if the client has run far ahead for a
/// whole window: skips the backlog so the next step runs the command that leaves
/// [`INPUT_LEAD_TARGET`] ticks in hand. Returns how many sequences were skipped.
fn pace_input<G: PlayerGame>(
    command_buffer: &mut ServerCommandBuffer<G>,
    queued: &mut QueuedUserCmds,
    tick: u64,
    rate: TickRate,
) -> Option<u32> {
    let arrived = queued.newest_arrival.take();
    if command_buffer.last_simulated_sequence == 0 {
        return None;
    }
    let expected = command_buffer.expected_sequence();
    let window = rate.ticks(INPUT_LEAD_WINDOW_SECONDS);
    if let Some(arrived) = arrived {
        command_buffer
            .lead
            .record(tick, arrived.wrapping_sub(expected) as i32, window);
    }

    let report = command_buffer.lead.report(tick, window)?;
    if u64::from(report.samples) < window / 2 {
        return None;
    }
    let skip = reanchor_skip(i32::from(report.min), rate)?;
    let next = expected.wrapping_add(skip);
    queued
        .commands
        .retain(|command| !sequence_is_newer(next, command.sequence));
    command_buffer.skip_to = Some(next);
    // Every sample predates the skip; judge the new anchor on fresh ones.
    command_buffer.lead.clear();
    Some(skip)
}

/// One server tick of one player's input timeline: puts late commands into their guessed slots
/// and steps them again from there (input rollback), then steps exactly one new slot. A new
/// timeline's first tick steps a slot for every sequence from the oldest command queued for it
/// to the newest, at most [`join_burst_span`] of them.
///
/// A rollback rewrites the server's own record of the past: the timeline's frames, so the
/// owner's ack ([`confirmed_frame`]) is the replayed state, and the live player, so every later
/// pose follows from it. Never what was published: the lag-comp history mirrors what clients
/// were sent, so a shot is judged against what its shooter could have seen.
///
/// `rollback` off
/// ([`NetAhoyServerPlugin::input_rollback`](crate::NetAhoyServerPlugin::input_rollback)): a late
/// command for a guessed slot is dropped and the guess acknowledged as that sequence's input;
/// nothing is re-stepped.
pub(crate) fn run_player_tick<G: PlayerGame>(
    stepper: &mut impl TimelineStepper<G>,
    player: Entity,
    tick: u64,
    command_buffer: &mut ServerCommandBuffer<G>,
    queued: &mut QueuedUserCmds,
    stats: &mut RollbackStats,
    rollback: bool,
    rate: TickRate,
) {
    let min_rewind_tick = tick.saturating_sub(rate.ticks(HISTORY_SECONDS));

    // Judged before any rewind: if the live state is not what the last step left, someone
    // edited it and the guesses behind the edit are final.
    if let Some(last) = command_buffer.timeline.back()
        && let Some(live) = stepper.capture_frame(player, last.command)
        && externally_edited(&last.after, &live)
        && floor_rollback(command_buffer)
    {
        stats.floors += 1;
    }

    // A command arriving after its guessed tick replaces that slot's guess. Backups repeating
    // real slots are duplicates, and anything older than the next slot without a slot of its
    // own (skipped by a re-anchor, or behind the window) can never run: both are dropped.
    let mut earliest_late = None;
    let mut accepted_guess = false;
    let mut remaining = VecDeque::with_capacity(queued.commands.len());
    while let Some(mut command) = queued.commands.pop_front() {
        if let Some(index) = command_buffer
            .timeline
            .iter()
            .position(|frame| frame.command.sequence == command.sequence)
        {
            if !command_buffer.timeline[index].received && !rollback {
                // The guess stays the input that ran; the real command only proves the client
                // sent it, which lets the ack cross the slot.
                command_buffer.timeline[index].received = true;
                command_buffer.last_command_tick = tick;
                note_received(command_buffer, command.sequence);
                accepted_guess = true;
            } else if !command_buffer.timeline[index].received {
                sanitize_usercmd(player, &mut command, min_rewind_tick, tick);
                command_buffer.timeline[index].command = command;
                command_buffer.timeline[index].received = true;
                command_buffer.last_command_tick = tick;
                note_received(command_buffer, command.sequence);
                earliest_late = Some(earliest_late.map_or(index, |old: usize| old.min(index)));
            }
        } else if command_buffer.last_simulated_sequence == 0
            || !sequence_is_newer(command_buffer.expected_sequence(), command.sequence)
        {
            remaining.push_back(command);
        }
    }
    queued.commands = remaining;
    if accepted_guess {
        advance_confirmation(command_buffer);
    }

    if let Some(first) = earliest_late {
        let depth = command_buffer.timeline.len() - first;
        stats.rollbacks += 1;
        stats.restepped_ticks += depth as u32;
        stats.max_depth = stats.max_depth.max(depth);
        let rewind = command_buffer.timeline[first].before;
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
                .unwrap_or(rewind);
            if let Err(err) = stepper.player_move(
                player,
                &mut command_buffer.events,
                command,
                previous_sequence,
                previous_buttons,
            ) {
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
        advance_confirmation(command_buffer);
    }

    // After the rollback, so a real command arriving this tick still wins over the guess.
    if rollback {
        retire_stale_guesses(command_buffer, tick, rate);
    } else {
        retire_stale_guesses_after(command_buffer, tick, 0);
    }

    if let Some(skipped) = pace_input(command_buffer, queued, tick, rate) {
        stats.reanchors += 1;
        stats.reanchor_skipped += skipped;
        debug!(
            "{player} input re-anchor: client ran {skipped} commands ahead, skipping to {}",
            command_buffer.expected_sequence()
        );
    }

    // A new timeline's first queued commands are the ones its client predicted before this tick
    // (a stalled first packet, a long first frame), and it judges the timeline against all of
    // them: step them all, a slot each, as a rollback does. The newest stays the head the next
    // tick follows, so the lead is what it would have been had they come one per tick. The
    // sequences are the client's to pick, so the span stepped is at most a queue's worth: older
    // commands are dropped.
    if command_buffer.last_simulated_sequence == 0
        && let Some((_, newest)) = queued_span(queued)
    {
        let span = join_burst_span(rate);
        queued
            .commands
            .retain(|command| newest.wrapping_sub(command.sequence) < span);
        if let Some((oldest, newest)) = queued_span(queued) {
            command_buffer.skip_to = Some(oldest);
            while sequence_is_newer(newest, command_buffer.expected_sequence()) {
                let received = queued.take_sequence(command_buffer.expected_sequence());
                step_next_slot(stepper, player, tick, command_buffer, received, rate);
            }
        }
    }

    let received = queued.take_sequence(command_buffer.expected_sequence());
    step_next_slot(stepper, player, tick, command_buffer, received, rate);
}

/// The most sequences a new timeline's first tick steps: what the queue holds, and never more
/// than the timeline keeps.
fn join_burst_span(rate: TickRate) -> u32 {
    let queue = queue_commands(rate);
    let timeline = rate.capped_ticks(
        SERVER_INPUT_ROLLBACK_SECONDS,
        SERVER_INPUT_ROLLBACK_MAX_TICKS,
    );
    queue.min(timeline) as u32
}

/// The oldest and the newest sequence queued, if any.
fn queued_span(queued: &QueuedUserCmds) -> Option<(u32, u32)> {
    let sequences = || queued.commands.iter().map(|command| command.sequence);
    Some((
        sequences().min_by(|a, b| sequence_cmp(*a, *b))?,
        sequences().max_by(|a, b| sequence_cmp(*a, *b))?,
    ))
}

/// Steps the timeline's next slot on `received`, its real command, or without it on a guess
/// from the last command stepped. Nothing to step before the first command.
fn step_next_slot<G: PlayerGame>(
    stepper: &mut impl TimelineStepper<G>,
    player: Entity,
    tick: u64,
    command_buffer: &mut ServerCommandBuffer<G>,
    received: Option<UserCmd>,
    rate: TickRate,
) {
    let min_rewind_tick = tick.saturating_sub(rate.ticks(HISTORY_SECONDS));
    let interrupted_after = rate.ticks(CONNECTION_INTERRUPTED_SECONDS);
    let expected_sequence = command_buffer.expected_sequence();
    let mut command = if let Some(command) = received {
        command_buffer.last_command_tick = tick;
        note_received(command_buffer, command.sequence);
        command
    } else if let Some(command) = command_buffer.last_command {
        // No command, but time still advances: repeat the last input. Only real commands
        // advance the ack.
        extrapolate_usercmd(
            command,
            expected_sequence,
            min_rewind_tick,
            tick,
            // A short stall holds intent; a dead client goes neutral while gravity and timers
            // keep stepping.
            tick.saturating_sub(command_buffer.last_command_tick) > interrupted_after,
        )
    } else {
        return;
    };

    if received.is_some() {
        sanitize_usercmd(player, &mut command, min_rewind_tick, tick);
    }

    let before = stepper.capture_frame(player, command_buffer.last_command.unwrap_or(command));
    // The last command stepped, even across a re-anchor: a game's sweeps cover the whole gap
    // instead of jumping it.
    let previous_sequence = command_buffer.last_simulated_sequence;
    let previous_buttons = command_buffer.last_buttons;

    if let Err(err) = stepper.player_move(
        player,
        &mut command_buffer.events,
        command,
        previous_sequence,
        previous_buttons,
    ) {
        warn!(
            "failed to step server KCC for {player} command {}: {err}",
            command.sequence
        );
    }

    if let (Some(before), Some(after)) = (before, stepper.capture_frame(player, command)) {
        let interrupted = tick.saturating_sub(command_buffer.last_command_tick) > interrupted_after;
        let mut flags = RemoteFlags::empty();
        flags.set(RemoteFlags::CONNECTION_INTERRUPTED, interrupted);
        flags.set(RemoteFlags::FAKED_INPUT, received.is_none());
        flags = frame_flags(&after, flags);
        command_buffer.timeline.push_back(ServerInputFrame {
            server_tick: tick,
            command,
            received: received.is_some(),
            previous_sequence,
            previous_buttons,
            before,
            after,
            flags,
        });
        let slots = rate.capped_ticks(
            SERVER_INPUT_ROLLBACK_SECONDS,
            SERVER_INPUT_ROLLBACK_MAX_TICKS,
        );
        while command_buffer.timeline.len() > slots as usize {
            retire_front(command_buffer);
        }
    }
    command_buffer.skip_to = None;
    command_buffer.last_simulated_sequence = command.sequence;
    command_buffer.last_buttons = command.buttons;
    command_buffer.last_has_move_input = command.movement.length() > 0.1;
    command_buffer.last_command = Some(command);
    advance_confirmation(command_buffer);
}

#[cfg(test)]
mod tests;
