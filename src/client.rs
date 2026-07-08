//! Lives on the player's machine: guesses where they're going (prediction),
//! fixes the guess when the server disagrees (reconciliation), draws everyone.

use std::collections::VecDeque;

use bevy::prelude::*;
use bevy_replicon::prelude::*;

use crate::{
    demo::DemoPlayback,
    math::{LagCompensationHistory, RemoteRenderTime, RemoteSnapshotSample, sample_buffer_at},
    step::{AhoyPredictionFrame, NetAhoyStepper},
    protocol::*,
    player::{NetAhoyPlayerEvents, NetAhoyPlayerState},
};

pub const USERCMD_BACKUP_COUNT: usize = 8;
pub const PREDICTION_HISTORY_CAPACITY: usize = 256;
pub const REMOTE_INTERPOLATION_CAPACITY: usize = 64;
/// 4 ticks = 200 ms at 20 Hz. Two ticks of headroom over the minimum pair to
/// interpolate between; capped extrapolation covers the gaps loss opens up.
pub const REMOTE_INTERPOLATION_DELAY_TICKS: u64 = 2;
pub const REMOTE_CLOCK_MAX_CATCHUP_RATE: f64 = 0.10;
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
    /// Library buttons plus any game-defined bits (e.g. weapon fire) the game
    /// sets in the high range. The library never interprets the game bits.
    pub buttons: AhoyButtons,
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

#[derive(Resource, Debug)]
pub struct ClientServerClock {
    pub latest_server_tick: u64,
    pub tick_hz: f64,
    pub interpolation_delay_seconds: f64,
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
        if self.render_server_time_seconds >= latest_renderable {
            self.render_server_time_seconds = latest_renderable;
            return;
        }

        let seconds_behind = latest_renderable - self.render_server_time_seconds;
        if seconds_behind > self.interpolation_delay_seconds.max(1.0 / self.tick_hz) {
            self.render_server_time_seconds = latest_renderable;
            return;
        }

        let catchup_rate = if seconds_behind > 1.0 / self.tick_hz {
            1.0 + REMOTE_CLOCK_MAX_CATCHUP_RATE
        } else {
            1.0
        };
        self.render_server_time_seconds = (self.render_server_time_seconds
            + delta_seconds.max(0.0) * catchup_rate)
            .min(latest_renderable);
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
    snapshots: Query<&AhoySnapshot, Changed<AhoySnapshot>>,
) {
    for snapshot in &snapshots {
        clock.observe_server_tick(snapshot.server_tick);
    }
    clock.advance(time.delta_secs_f64());
}

fn buffer_remote_snapshots(
    mut history: ResMut<LagCompensationHistory>,
    mut remotes: Query<
        (&PlayerId, &AhoySnapshot, &mut RemoteInterpolationBuffer),
        Changed<AhoySnapshot>,
    >,
) {
    for (player_id, snapshot, mut buffer) in &mut remotes {
        if snapshot.server_tick != 0 {
            let sample = RemoteSnapshotSample::from_snapshot(snapshot);
            buffer.push(sample);
            history.record(*player_id, sample);
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
    input: Res<ClientInput>,
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
    let command = AhoyUserCmd {
        sequence: input_state.next_sequence.wrapping_add(1),
        movement: input.movement.clamp_length_max(1.0),
        look: input.look,
        buttons: input.buttons,
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
    command_history: Res<LocalCommandHistory>,
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
    let local_state = ack_frame.as_ref().map(|ack_frame| {
        (
            &ack_frame.controller_state,
            &ack_frame.accumulated_input,
            &ack_frame.player_state,
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
