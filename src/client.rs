//! Lives on the player's machine: guesses where they're going (prediction),
//! fixes the guess when the server disagrees (reconciliation), draws everyone.

use std::collections::VecDeque;

use bevy::prelude::*;
use bevy_replicon::prelude::*;

use crate::{
    demo::DemoPlayback,
    math::{
        LagCompensationHistory, REMOTE_EXTRAPOLATION_SECONDS, RemoteRenderTime,
        RemoteSnapshotSample, sample_buffer_at,
    },
    step::{AhoyPredictionFrame, NetAhoyStepper},
    protocol::*,
    player::{NetAhoyPlayerEvents, NetAhoyPlayerState},
};

pub const USERCMD_BACKUP_COUNT: usize = 8;
pub const PREDICTION_HISTORY_CAPACITY: usize = 256;
/// Rewind+replay bail-out: past ~1.5s of unacked input (20Hz cmds) the replay
/// itself becomes the problem — hundreds of KCC steps per snapshot balloon
/// frame time, which delays outgoing cmds and widens the window further (the
/// death spiral). Cheaper to eat one visible snap and start fresh.
pub const MAX_REPLAY_COMMANDS: usize = 32;
pub const REMOTE_INTERPOLATION_CAPACITY: usize = 64;
/// 4 ticks = 200 ms at 20 Hz. Two ticks of headroom over the minimum pair to
/// interpolate between; capped extrapolation covers the gaps loss opens up.
pub const REMOTE_INTERPOLATION_DELAY_TICKS: u64 = 3;
pub const REMOTE_CLOCK_MAX_CATCHUP_RATE: f64 = 0.10;
/// Proportional gain of the render-clock rate servo: rate = 1 - error * gain.
/// 2.0/s means a 50 ms error changes playback speed by 10% (the clamp), and
/// errors decay with a ~0.5 s time constant — fast enough to track clock
/// drift and arrival jitter, slow enough to be invisible.
pub const REMOTE_CLOCK_SERVO_GAIN: f64 = 2.0;
pub const IGNORE_XZ_ERROR: f32 = 0.035;
pub const IGNORE_GROUNDED_Y_ERROR: f32 = 0.20;
pub const SNAP_ERROR_DISTANCE: f32 = 2.25;
pub const PRESENTATION_RESPONSE: f32 = 10.0;

#[derive(SystemSet, Debug, Clone, Copy, Hash, PartialEq, Eq)]
pub enum ClientNetAhoySystems {
    /// Rewind + replay against fresh server snapshots (`FixedPreUpdate`).
    Reconcile,
    /// Build this tick's user command, predict it, send it (`FixedPreUpdate`).
    Predict,
    /// Per-frame bookkeeping and remote interpolation (`Update`).
    Interpolate,
}

pub struct ClientNetAhoyPlugin;

impl Plugin for ClientNetAhoyPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<LocalPlayerId>()
            .init_resource::<ClientInput>()
            .init_resource::<ClientInputState>()
            .init_resource::<PredictionHistory>()
            .init_resource::<LocalCommandHistory>()
            .init_resource::<ClientServerClock>()
            // Client-side mirror of the server's pose history, fed from
            // received snapshots — the rocket sweep samples it during
            // prediction and replay so direct hits predict.
            .init_resource::<LagCompensationHistory>()
            // The predicted outbox: what the local step fired/blasted. The
            // game drains it for instant fire/explosion presentation,
            // deduping by RocketId (replay pushes duplicates).
            .init_resource::<NetAhoyPlayerEvents>()
            .add_observer(set_local_player_id)
            .add_systems(OnEnter(ClientState::Connected), announce_join)
            .configure_sets(
                FixedPreUpdate,
                (ClientNetAhoySystems::Reconcile, ClientNetAhoySystems::Predict).chain(),
            )
            .add_systems(
                FixedPreUpdate,
                reconcile_local_prediction
                    .run_if(in_state(ClientState::Connected))
                    .in_set(ClientNetAhoySystems::Reconcile),
            )
            .add_systems(
                FixedPreUpdate,
                drive_prediction_and_send_input
                    .run_if(in_state(ClientState::Connected))
                    .in_set(ClientNetAhoySystems::Predict),
            )
            .add_systems(FixedLast, record_prediction_state)
            .add_systems(
                Update,
                (
                    mark_server_truth_ghost,
                    tag_remote_players,
                    cleanup_client_prediction_kcc,
                    cleanup_local_presentation_player,
                    cleanup_remote_player_visuals,
                    update_server_clock,
                    buffer_remote_snapshots,
                    interpolate_remote_players,
                    update_local_presentation_from_prediction,
                )
                    .chain()
                    .in_set(ClientNetAhoySystems::Interpolate),
            );
    }
}

#[derive(Resource, Default, Clone, Copy, Debug)]
pub struct LocalPlayerId(pub Option<u64>);

impl LocalPlayerId {
    pub fn is_assigned_to(self, player_id: u64) -> bool {
        self.0 == Some(player_id)
    }

    pub fn label(self) -> String {
        self.0
            .map(|player_id| format!("player {player_id}"))
            .unwrap_or_else(|| "joining".to_string())
    }
}

/// Written by the game every frame; consumed once per fixed tick.
#[derive(Resource, Clone, Copy, Debug, Default)]
pub struct ClientInput {
    pub movement: Vec2,
    pub look: Vec2,
    /// Held state, overwritten by the game every frame. Library buttons plus
    /// any game-defined bits in the high range; the library never interprets
    /// the game bits.
    pub buttons: AhoyButtons,
    /// Press edges, OR-ed in by the game every frame (`just_pressed` bits) and
    /// drained into the next command — a tap shorter than a tick window still
    /// lands as one command with the bit set, so no edge is ever lost.
    pub pressed: AhoyButtons,
    /// Movement latch, the same idea for held direction keys: the game writes
    /// the last non-zero `movement` here every frame, drained into the next
    /// command. Without it `movement` is a point-sample on tick frames only —
    /// a tap that starts and ends between ticks would never move the player.
    pub tap_movement: Vec2,
    /// Latched by the game on a fire click ([`SubtickFire`] carries the click's
    /// sub-tick fraction and exact look); drained into exactly one command.
    pub fire: Option<SubtickFire>,
}

#[derive(Resource, Default)]
pub struct ClientInputState {
    pub next_sequence: u32,
    pub pending_record_command: Option<AhoyUserCmd>,
    pub previous_buttons: AhoyButtons,
}

/// The replicated entity for the local player: server truth, used only as the
/// reconciliation reference (and optionally as a debug ghost).
#[derive(Component)]
pub struct ServerTruthGhost;

/// The locally simulated KCC the camera and gameplay should treat as the
/// player. Spawned by the game when its [`ServerTruthGhost`] appears.
#[derive(Component)]
#[require(PredictionCorrection, NetAhoyPlayerState)]
pub struct ClientPredictionKcc {
    pub server_entity: Entity,
}

/// Smoothed visual for the local player; trails the prediction KCC by the
/// decaying correction offset so corrections never pop.
#[derive(Component)]
pub struct LocalPresentationPlayer {
    pub prediction_entity: Entity,
}

/// Visual entity for a remote player, driven by interpolation.
#[derive(Component)]
pub struct RemotePlayerVisual {
    pub server_entity: Entity,
    pub player_id: PlayerId,
}

#[derive(Component, Debug)]
pub struct PredictionCorrection {
    pub presentation_offset: Vec3,
    pub last_error: f32,
    pub last_ack_sequence: u32,
    pub last_server_tick: u64,
    pub replayed_commands: usize,
    pub mode: CorrectionMode,
}

impl Default for PredictionCorrection {
    fn default() -> Self {
        Self {
            presentation_offset: Vec3::ZERO,
            last_error: 0.0,
            last_ack_sequence: 0,
            last_server_tick: 0,
            replayed_commands: 0,
            mode: CorrectionMode::Waiting,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CorrectionMode {
    Waiting,
    MissingHistory,
    Ignored,
    Replayed,
    Snapped,
}

#[derive(Resource)]
pub struct PredictionHistory {
    pub frames: VecDeque<AhoyPredictionFrame>,
}

impl Default for PredictionHistory {
    fn default() -> Self {
        Self {
            frames: VecDeque::with_capacity(PREDICTION_HISTORY_CAPACITY),
        }
    }
}

impl PredictionHistory {
    pub fn push(&mut self, frame: AhoyPredictionFrame) {
        if let Some(existing) = self
            .frames
            .iter_mut()
            .find(|existing| existing.command.sequence == frame.command.sequence)
        {
            *existing = frame;
            return;
        }

        if self.frames.len() == PREDICTION_HISTORY_CAPACITY {
            self.frames.pop_front();
        }
        self.frames.push_back(frame);
    }

    pub fn get(&self, sequence: u32) -> Option<&AhoyPredictionFrame> {
        self.frames
            .iter()
            .rev()
            .find(|frame| frame.command.sequence == sequence)
    }

    pub fn retain_after(&mut self, sequence: u32) {
        // Keep the frame AT `sequence`: a later snapshot can repeat the same
        // ack (dropped inputs, external impulses) and must re-compare against it.
        self.frames.retain(|frame| {
            frame.command.sequence == sequence
                || sequence_is_newer(frame.command.sequence, sequence)
        });
    }

    pub fn clear(&mut self) {
        self.frames.clear();
    }
}

#[derive(Resource)]
pub struct LocalCommandHistory {
    pub commands: VecDeque<AhoyUserCmd>,
}

impl Default for LocalCommandHistory {
    fn default() -> Self {
        Self {
            commands: VecDeque::with_capacity(PREDICTION_HISTORY_CAPACITY),
        }
    }
}

impl LocalCommandHistory {
    pub fn push(&mut self, command: AhoyUserCmd) {
        if self.commands.len() == PREDICTION_HISTORY_CAPACITY {
            self.commands.pop_front();
        }
        self.commands.push_back(command);
    }

    pub fn recent(&self, count: usize) -> Vec<AhoyUserCmd> {
        let start = self.commands.len().saturating_sub(count);
        self.commands.iter().skip(start).copied().collect()
    }

    pub fn after_sequence(&self, sequence: u32) -> Vec<AhoyUserCmd> {
        self.commands
            .iter()
            .copied()
            .filter(|command| sequence_is_newer(command.sequence, sequence))
            .collect()
    }
}

#[derive(Component, Clone, Debug)]
pub struct RemoteInterpolationBuffer {
    pub samples: VecDeque<RemoteSnapshotSample>,
    pub delay_ticks: u64,
}

impl Default for RemoteInterpolationBuffer {
    fn default() -> Self {
        Self {
            samples: VecDeque::with_capacity(REMOTE_INTERPOLATION_CAPACITY),
            delay_ticks: REMOTE_INTERPOLATION_DELAY_TICKS,
        }
    }
}

impl RemoteInterpolationBuffer {
    pub fn push(&mut self, sample: RemoteSnapshotSample) {
        if let Some(last) = self.samples.back().copied()
            && sample.server_tick > last.server_tick
            && sample.starts_new_motion_segment_after(last)
        {
            self.samples.clear();
        }

        if let Some(index) = self
            .samples
            .iter()
            .position(|existing| existing.server_tick >= sample.server_tick)
        {
            if self.samples[index].server_tick == sample.server_tick {
                self.samples[index] = sample;
            } else {
                self.samples.insert(index, sample);
            }
        } else {
            self.samples.push_back(sample);
        }

        while self.samples.len() > REMOTE_INTERPOLATION_CAPACITY {
            self.samples.pop_front();
        }
    }

    pub fn sample(&self, render_time: RemoteRenderTime) -> Option<RemoteSnapshotSample> {
        sample_buffer_at(&self.samples, render_time)
    }
}

/// Diagnostics for the remote render clock, logged periodically so field
/// reports ("remotes wobble sometimes") can be attributed to loss vs snaps
/// vs render hitches instead of guessed at.
#[derive(Debug, Default, Clone, Copy)]
pub struct RemoteClockStats {
    /// Clock fell more than the interp delay behind and jumped forward.
    pub snaps: u32,
    /// Frames rendered past the newest snapshot (dead-reckoned): late/lost
    /// head packets that would have been stalls before extrapolation.
    pub extrapolated_frames: u32,
    /// Extrapolated frames that hit the hard cap and held position.
    pub capped_frames: u32,
}

impl RemoteClockStats {
    pub fn any(self) -> bool {
        self.snaps > 0 || self.extrapolated_frames > 0 || self.capped_frames > 0
    }
}

#[derive(Resource, Debug)]
pub struct ClientServerClock {
    pub latest_server_tick: u64,
    pub tick_hz: f64,
    pub interpolation_delay_seconds: f64,
    pub stats: RemoteClockStats,
    render_server_time_seconds: f64,
    initialized: bool,
}

impl Default for ClientServerClock {
    fn default() -> Self {
        Self {
            latest_server_tick: 0,
            tick_hz: FIXED_TIMESTEP_HZ,
            interpolation_delay_seconds: REMOTE_INTERPOLATION_DELAY_TICKS as f64
                / FIXED_TIMESTEP_HZ,
            stats: RemoteClockStats::default(),
            render_server_time_seconds: 0.0,
            initialized: false,
        }
    }
}

impl ClientServerClock {
    pub fn observe_server_tick(&mut self, server_tick: u64) {
        if server_tick == 0 || server_tick < self.latest_server_tick {
            return;
        }

        self.latest_server_tick = server_tick;
        if !self.initialized {
            self.render_server_time_seconds = self.latest_renderable_server_time_seconds();
            self.initialized = true;
        }
    }

    pub fn advance(&mut self, delta_seconds: f64) {
        if !self.initialized {
            return;
        }

        let latest_renderable = self.latest_renderable_server_time_seconds();
        // Positive: rendering ahead of the arrival stream; negative: behind.
        let error = self.render_server_time_seconds - latest_renderable;

        // Genuinely far behind (hidden tab, long outage): jump rather than
        // spend seconds crawling through stale motion.
        if -error > self.interpolation_delay_seconds.max(1.0 / self.tick_hz) {
            self.render_server_time_seconds = latest_renderable;
            self.stats.snaps += 1;
            return;
        }

        // Rate servo. A hard clamp prints arrival jitter to the screen as
        // stall-then-crawl; a free-running clock drifts against the server's
        // (different oscillators) until it parks at the extrapolation cap.
        // Instead, nudge playback speed a few percent toward zero error so the
        // clock breathes with the arrival stream. Late heads still render as
        // capped dead-reckoning (`sample_buffer_at`) instead of freezing.
        let rate = (1.0 - error * REMOTE_CLOCK_SERVO_GAIN).clamp(
            1.0 - REMOTE_CLOCK_MAX_CATCHUP_RATE,
            1.0 + REMOTE_CLOCK_MAX_CATCHUP_RATE,
        );
        let max_render_time = latest_renderable + f64::from(REMOTE_EXTRAPOLATION_SECONDS);
        let next = (self.render_server_time_seconds + delta_seconds.max(0.0) * rate)
            .min(max_render_time);

        // Dead-reckoning frames: rendering past the newest sample we hold.
        let newest_sample_seconds = self.latest_server_tick as f64 / self.tick_hz;
        if next > newest_sample_seconds {
            self.stats.extrapolated_frames += 1;
            if next >= max_render_time {
                self.stats.capped_frames += 1;
            }
        }
        self.render_server_time_seconds = next;
    }

    pub fn target_time(&self) -> Option<RemoteRenderTime> {
        self.initialized
            .then(|| RemoteRenderTime::from_seconds(self.render_server_time_seconds, self.tick_hz))
    }

    pub fn target_tick(&self) -> u64 {
        self.target_time().unwrap_or_default().tick
    }

    pub fn target_alpha(&self) -> f32 {
        self.target_time().unwrap_or_default().alpha
    }

    fn latest_renderable_server_time_seconds(&self) -> f64 {
        (self.latest_server_tick as f64 / self.tick_hz - self.interpolation_delay_seconds).max(0.0)
    }
}

fn announce_join(mut commands: Commands) {
    commands.client_trigger(JoinRequest);
}

fn set_local_player_id(
    accepted: On<JoinAccepted>,
    mut local: ResMut<LocalPlayerId>,
    playback: Option<ResMut<DemoPlayback>>,
) {
    // In demo playback the recorded JoinAccepted replays, but nobody is local —
    // remember whose demo this is (spectator cam target) and keep LocalPlayerId
    // unset so no prediction path ever engages.
    if let Some(mut playback) = playback {
        playback.recorded_player = Some(accepted.player_id);
        info!("demo recorded by player {}", accepted.player_id);
        return;
    }

    local.0 = Some(accepted.player_id);
    info!("joined as player {}", accepted.player_id);
}

fn mark_server_truth_ghost(
    mut commands: Commands,
    local: Res<LocalPlayerId>,
    players: Query<(Entity, &PlayerId), (With<NetworkedPlayer>, Without<ServerTruthGhost>)>,
) {
    let Some(local_id) = local.0 else {
        return;
    };

    for (entity, player_id) in &players {
        if player_id.0 == local_id {
            commands.entity(entity).insert(ServerTruthGhost);
        }
    }
}

fn tag_remote_players(
    mut commands: Commands,
    local: Res<LocalPlayerId>,
    playback: Option<Res<DemoPlayback>>,
    players: Query<
        (Entity, &PlayerId),
        (
            With<NetworkedPlayer>,
            Without<ServerTruthGhost>,
            Without<RemoteInterpolationBuffer>,
        ),
    >,
) {
    // In demo playback nobody is local: every player interpolates.
    let local_id = if playback.is_some() {
        None
    } else {
        let Some(local_id) = local.0 else {
            return;
        };
        Some(local_id)
    };

    for (entity, player_id) in &players {
        if Some(player_id.0) != local_id {
            commands
                .entity(entity)
                .insert(RemoteInterpolationBuffer::default());
        }
    }
}

fn cleanup_client_prediction_kcc(
    mut commands: Commands,
    predictions: Query<(Entity, &ClientPredictionKcc)>,
    local_players: Query<(), With<ServerTruthGhost>>,
) {
    for (entity, prediction) in &predictions {
        if local_players.get(prediction.server_entity).is_err() {
            commands.entity(entity).despawn();
        }
    }
}

fn cleanup_local_presentation_player(
    mut commands: Commands,
    presentations: Query<(Entity, &LocalPresentationPlayer)>,
    predictions: Query<(), With<ClientPredictionKcc>>,
) {
    for (entity, presentation) in &presentations {
        if predictions.get(presentation.prediction_entity).is_err() {
            commands.entity(entity).despawn();
        }
    }
}

fn cleanup_remote_player_visuals(
    mut commands: Commands,
    visuals: Query<(Entity, &RemotePlayerVisual)>,
    remotes: Query<(), With<RemoteInterpolationBuffer>>,
) {
    for (entity, visual) in &visuals {
        if remotes.get(visual.server_entity).is_err() {
            commands.entity(entity).despawn();
        }
    }
}

fn update_server_clock(
    time: Res<Time>,
    mut clock: ResMut<ClientServerClock>,
    snapshots: Query<&PlayerSnapshot, Changed<PlayerSnapshot>>,
    mut window: Local<f32>,
) {
    for snapshot in &snapshots {
        clock.observe_server_tick(snapshot.server_tick);
    }
    clock.advance(time.delta_secs_f64());

    // Attribute wobble reports to real events instead of vibes.
    *window += time.delta_secs();
    if *window >= 30.0 {
        let stats = clock.stats;
        if stats.any() {
            info!(
                "remote clock, last {:.0}s: {} extrapolated frames ({} capped), {} snaps",
                *window, stats.extrapolated_frames, stats.capped_frames, stats.snaps,
            );
        }
        clock.stats = RemoteClockStats::default();
        *window = 0.0;
    }
}

fn buffer_remote_snapshots(
    mut history: ResMut<LagCompensationHistory>,
    mut remotes: Query<
        (&PlayerId, &PlayerSnapshot, &mut RemoteInterpolationBuffer),
        Changed<PlayerSnapshot>,
    >,
) {
    for (player_id, snapshot, mut buffer) in &mut remotes {
        if snapshot.server_tick != 0 {
            buffer.push(snapshot.0);
            history.record(*player_id, snapshot.0);
        }
    }
}

fn interpolate_remote_players(
    clock: Res<ClientServerClock>,
    remotes: Query<&RemoteInterpolationBuffer>,
    mut visuals: Query<(
        &RemotePlayerVisual,
        &mut Transform,
        Option<&mut bevy_ahoy::CharacterLook>,
    )>,
) {
    let Some(render_time) = clock.target_time() else {
        return;
    };

    for (visual, mut transform, look) in &mut visuals {
        let Ok(buffer) = remotes.get(visual.server_entity) else {
            continue;
        };
        let Some(sample) = buffer.sample(render_time) else {
            continue;
        };

        transform.translation = sample.position;
        if let Some(mut look) = look {
            look.yaw = sample.look.x;
            look.pitch = sample.look.y;
        }
    }
}

fn drive_prediction_and_send_input(
    mut commands: Commands,
    mut input: ResMut<ClientInput>,
    clock: Res<ClientServerClock>,
    mut input_state: ResMut<ClientInputState>,
    mut command_history: ResMut<LocalCommandHistory>,
    predictions: Query<Entity, With<ClientPredictionKcc>>,
    mut stepper: NetAhoyStepper,
) {
    let Ok(predicted_entity) = predictions.single() else {
        return;
    };

    // The render time the remote capsules were drawn at this frame — what the
    // player is actually aiming at. It rides the command so the shared step
    // samples the same poses on both peers.
    let seen = clock.target_time().unwrap_or_default();
    // Drain the tap latch every tick; it only decides the command when the
    // held state is zero (key already released by the tick frame).
    let tap_movement = std::mem::take(&mut input.tap_movement);
    let movement = if input.movement != Vec2::ZERO {
        input.movement
    } else {
        tap_movement
    };
    let command = AhoyUserCmd {
        sequence: input_state.next_sequence.wrapping_add(1),
        movement: movement.clamp_length_max(1.0),
        look: input.look,
        // Held bits plus the press edges accumulated since the last tick, so
        // sub-tick taps still register on this command.
        buttons: input.buttons | std::mem::take(&mut input.pressed),
        fire: input.fire.take(),
        seen_server_tick: seen.tick,
        seen_alpha: seen.alpha,
    };

    if let Err(err) = stepper.player_move(
        predicted_entity,
        command,
        input_state.next_sequence,
        input_state.previous_buttons,
    ) {
        warn!(
            "failed to step predicted KCC for command {}: {err}",
            command.sequence
        );
    }

    input_state.next_sequence = command.sequence;
    input_state.previous_buttons = command.buttons;
    input_state.pending_record_command = Some(command);
    command_history.push(command);

    commands.client_trigger(AhoyUserCmdPacket {
        commands: command_history.recent(USERCMD_BACKUP_COUNT),
    });
}

fn record_prediction_state(
    mut input_state: ResMut<ClientInputState>,
    mut history: ResMut<PredictionHistory>,
    predictions: Query<Entity, With<ClientPredictionKcc>>,
    mut stepper: NetAhoyStepper,
) {
    let Some(command) = input_state.pending_record_command.take() else {
        return;
    };
    let Ok(predicted_entity) = predictions.single() else {
        return;
    };

    if let Some(frame) = stepper.capture_frame(predicted_entity, command) {
        history.push(frame);
    }
}

fn reconcile_local_prediction(
    local: Res<LocalPlayerId>,
    server_players: Query<(&PlayerId, &AhoySnapshot), With<NetworkedPlayer>>,
    mut predictions: Query<(Entity, &ClientPredictionKcc, &mut PredictionCorrection)>,
    mut history: ResMut<PredictionHistory>,
    mut command_history: ResMut<LocalCommandHistory>,
    mut stepper: NetAhoyStepper,
) {
    let Some(local_id) = local.0 else {
        return;
    };
    let Ok((predicted_entity, prediction, mut correction)) = predictions.single_mut() else {
        return;
    };
    let Ok((player_id, snapshot)) = server_players.get(prediction.server_entity) else {
        return;
    };
    if player_id.0 != local_id {
        return;
    }

    if snapshot.server_tick <= correction.last_server_tick {
        return;
    }

    if snapshot.last_processed_sequence == 0 {
        correction.mode = CorrectionMode::Waiting;
        correction.last_server_tick = snapshot.server_tick;
        correction.last_error = 0.0;
        correction.replayed_commands = 0;
        return;
    }

    let ack_frame = history.get(snapshot.last_processed_sequence).cloned();
    let history_error = ack_frame.as_ref().map(|ack_frame| {
        let delta = snapshot.position - ack_frame.position;
        let xz_error = delta.xz().length();
        let y_error = delta.y.abs();
        let total_error = delta.length();
        // Weapon and rocket state count as a mismatch too: a server-declined
        // fire or a disputed direct hit must force the rewind path even when
        // the position error is zero.
        let state_mismatch =
            snapshot.state != ack_frame.state || snapshot.player_state != ack_frame.player_state;
        let ignore_y = snapshot.state.grounded && y_error <= IGNORE_GROUNDED_Y_ERROR;
        (total_error, state_mismatch, xz_error, y_error, ignore_y)
    });

    if let Some((total_error, state_mismatch, xz_error, y_error, ignore_y)) = history_error
        && !state_mismatch
        && xz_error <= IGNORE_XZ_ERROR
        && (ignore_y || y_error <= IGNORE_XZ_ERROR)
    {
        history.retain_after(snapshot.last_processed_sequence);
        correction.mode = CorrectionMode::Ignored;
        correction.last_server_tick = snapshot.server_tick;
        correction.last_ack_sequence = snapshot.last_processed_sequence;
        correction.last_error = total_error;
        correction.replayed_commands = 0;
        return;
    }

    let current_position = stepper.position(predicted_entity).unwrap_or(snapshot.position);
    let old_visible_position = current_position + correction.presentation_offset;
    let replay_commands = command_history.after_sequence(snapshot.last_processed_sequence);

    // Death-spiral bound: don't replay a runaway window — snap to server truth,
    // drop all pending prediction, and let the next snapshots start clean.
    if replay_commands.len() > MAX_REPLAY_COMMANDS {
        stepper.restore(predicted_entity, snapshot, None);
        history.clear();
        command_history.commands.clear();
        correction.mode = CorrectionMode::Snapped;
        correction.last_server_tick = snapshot.server_tick;
        correction.last_ack_sequence = snapshot.last_processed_sequence;
        correction.last_error = history_error
            .map(|(total_error, _, _, _, _)| total_error)
            .unwrap_or(0.0);
        correction.replayed_commands = 0;
        correction.presentation_offset = Vec3::ZERO;
        return;
    }

    let local_state = ack_frame.as_ref().map(|ack_frame| {
        (
            &ack_frame.controller_state,
            &ack_frame.accumulated_input,
        )
    });

    stepper.restore(predicted_entity, snapshot, local_state);

    let replayed = replay_commands.len();
    let mut previous_sequence = snapshot.last_processed_sequence;
    let mut previous_buttons = snapshot.last_processed_buttons;
    for command in replay_commands {
        if let Err(err) =
            stepper.player_move(predicted_entity, command, previous_sequence, previous_buttons)
        {
            warn!(
                "failed to replay predicted KCC for command {}: {err}",
                command.sequence
            );
        }
        if let Some(frame) = stepper.capture_frame(predicted_entity, command) {
            history.push(frame);
        }
        previous_sequence = command.sequence;
        previous_buttons = command.buttons;
    }

    history.retain_after(snapshot.last_processed_sequence);

    let new_position = stepper.position(predicted_entity).unwrap_or(snapshot.position);
    let correction_distance = current_position.distance(new_position);

    correction.mode = if ack_frame.is_none() {
        CorrectionMode::MissingHistory
    } else if correction_distance >= SNAP_ERROR_DISTANCE {
        CorrectionMode::Snapped
    } else {
        CorrectionMode::Replayed
    };
    correction.last_server_tick = snapshot.server_tick;
    correction.last_ack_sequence = snapshot.last_processed_sequence;
    correction.last_error = history_error
        .map(|(total_error, _, _, _, _)| total_error)
        .unwrap_or(correction_distance);
    correction.replayed_commands = replayed;
    correction.presentation_offset = if correction_distance >= SNAP_ERROR_DISTANCE {
        Vec3::ZERO
    } else {
        old_visible_position - new_position
    };
}

fn update_local_presentation_from_prediction(
    time: Res<Time>,
    mut predictions: Query<
        (&Transform, &mut PredictionCorrection),
        (With<ClientPredictionKcc>, Without<LocalPresentationPlayer>),
    >,
    mut presentations: Query<
        (&LocalPresentationPlayer, &mut Transform),
        (Without<ClientPredictionKcc>, Without<ServerTruthGhost>),
    >,
) {
    let alpha = 1.0 - (-PRESENTATION_RESPONSE * time.delta_secs()).exp();

    for (presentation, mut presentation_transform) in &mut presentations {
        let Ok((prediction_transform, mut correction)) =
            predictions.get_mut(presentation.prediction_entity)
        else {
            continue;
        };

        correction.presentation_offset = correction.presentation_offset.lerp(Vec3::ZERO, alpha);
        if correction.presentation_offset.length_squared() <= 0.0001 {
            correction.presentation_offset = Vec3::ZERO;
        }

        presentation_transform.translation =
            prediction_transform.translation + correction.presentation_offset;
        // Rotation is owned by the game's animation layer, which faces the model
        // toward the run direction (CharacterLook.yaw). The KCC transform stays at
        // identity, so syncing it here would stomp that facing every frame.
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clock_at(latest_tick: u64) -> ClientServerClock {
        let mut clock = ClientServerClock::default();
        clock.observe_server_tick(latest_tick);
        clock
    }

    #[test]
    fn clock_extrapolates_through_late_head_instead_of_stalling() {
        let mut clock = clock_at(100);
        let start = clock.target_time().unwrap().as_ticks_f64();

        // No new snapshots for 400 ms: the clock keeps running (servo slows
        // it to the floor rate, never freezes) into dead-reckoning past the
        // newest sample instead of stalling at the clamp.
        for _ in 0..40 {
            clock.advance(0.01);
        }
        let end = clock.target_time().unwrap().as_ticks_f64();
        // Floor-rate advance, but never past the extrapolation cap.
        let floor_rate = 0.4 * (1.0 - REMOTE_CLOCK_MAX_CATCHUP_RATE) * FIXED_TIMESTEP_HZ;
        let cap = f64::from(REMOTE_EXTRAPOLATION_SECONDS) * FIXED_TIMESTEP_HZ;
        let min_expected = floor_rate.min(cap);
        assert!(
            end - start >= min_expected - 1e-6,
            "clock should keep advancing: moved {} ticks, expected at least {min_expected}",
            end - start
        );
        assert!(clock.stats.extrapolated_frames > 0);
        assert_eq!(clock.stats.snaps, 0);
    }

    #[test]
    fn clock_extrapolation_is_capped() {
        let mut clock = clock_at(100);
        // A full second with no snapshots: overrun must stop at the cap.
        for _ in 0..100 {
            clock.advance(0.01);
        }
        let latest_renderable =
            100.0 - REMOTE_INTERPOLATION_DELAY_TICKS as f64;
        let max = latest_renderable + f64::from(REMOTE_EXTRAPOLATION_SECONDS) * FIXED_TIMESTEP_HZ;
        let end = clock.target_time().unwrap().as_ticks_f64();
        assert!(end <= max + 1e-6, "clock overran the cap: {end} > {max}");
        assert!(clock.stats.capped_frames > 0);
    }

    #[test]
    fn clock_snaps_after_long_outage() {
        let mut clock = clock_at(100);
        for _ in 0..100 {
            clock.advance(0.01);
        }
        // Outage ends: a burst of fresh snapshots far ahead of the held
        // render time. The clock jumps instead of crawling.
        clock.observe_server_tick(140);
        clock.advance(0.01);
        assert_eq!(clock.stats.snaps, 1);
        let expected = 140.0 - REMOTE_INTERPOLATION_DELAY_TICKS as f64;
        let end = clock.target_time().unwrap().as_ticks_f64();
        assert!((end - expected).abs() < 0.5, "clock should snap to {expected}, got {end}");
    }

    #[test]
    fn clock_catches_up_gently_when_behind() {
        let mut clock = clock_at(100);
        // Fall one tick behind (fresh snapshots arrived, clock hasn't moved).
        clock.observe_server_tick(101);
        clock.advance(0.1);
        // The servo runs faster than real time but bounded by the max rate.
        let advanced = clock.target_time().unwrap().as_ticks_f64()
            - (100.0 - REMOTE_INTERPOLATION_DELAY_TICKS as f64);
        let real_time = 0.1 * FIXED_TIMESTEP_HZ;
        let max_expected = real_time * (1.0 + REMOTE_CLOCK_MAX_CATCHUP_RATE);
        assert!(advanced > real_time, "behind clock should run fast: {advanced}");
        assert!(advanced <= max_expected + 1e-6);
        assert_eq!(clock.stats.snaps, 0);
    }
}
