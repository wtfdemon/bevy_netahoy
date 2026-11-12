//! Reconciliation: the prediction checked against the server's at each ack, rewound and
//! replayed when they disagree.

use bevy::prelude::*;

use crate::{
    events::Outbox, game::PlayerGame, protocol::*, server::SERVER_CMD_QUEUE_MIN_COMMANDS,
    stepper::Stepper,
};

use super::{
    OwnPlayerId, PredictedPlayer,
    predict::{LocalCommandHistory, PredictionHistory},
};

/// The most unacked input a correction replays (s). Past it acks have stopped, and replaying
/// would widen the window further: corrections park until the ack catches up
/// ([`CorrectionMode::Dropped`]). At least the server's queue (`SERVER_CMD_QUEUE_SECONDS`), or
/// burst recovery parks instead of rewinding.
pub(super) const MAX_REPLAY_SECONDS: f64 = 2.5;

/// The most commands one correction replays, at any rate ([`TickRate::capped_ticks`]):
/// hundreds of steps for one snapshot balloon frame time.
pub(super) const MAX_REPLAY_COMMANDS: u64 = 50;

/// The most commands one correction replays at `rate`: [`MAX_REPLAY_SECONDS`] of them, at most
/// [`MAX_REPLAY_COMMANDS`] and never fewer than the server's queue keeps at any rate
/// (`SERVER_CMD_QUEUE_MIN_COMMANDS`).
pub(crate) fn max_replay_commands(rate: TickRate) -> u64 {
    rate.capped_ticks(MAX_REPLAY_SECONDS, MAX_REPLAY_COMMANDS)
        .max(SERVER_CMD_QUEUE_MIN_COMMANDS)
}

/// Horizontal (and airborne vertical) error a prediction may keep without a rewind (m).
pub(super) const IGNORE_XZ_ERROR: f32 = 0.035;
/// Vertical error a grounded prediction may keep without a rewind (m).
pub(super) const IGNORE_GROUNDED_Y_ERROR: f32 = 0.20;
/// The snap gate's floor (m), for a player standing still.
pub(super) const SNAP_ERROR_DISTANCE: f32 = 2.25;
/// Speed-relative widening of the snap gate: a correction only pops past this many seconds of
/// travel at the current speed.
pub(super) const SNAP_ERROR_TRAVEL_SECS: f32 = 0.25;
/// Window-relative widening of the snap gate: a fraction of the ground the replay window covered.
/// It covers an error earned at speed but judged after slowing down (a mispredicted landing, then
/// friction), which the speed term alone would pop.
pub(super) const SNAP_WINDOW_FRACTION: f32 = 0.4;

/// What reconciliation did to the local prediction, on its [`PredictedPlayer`]: a read-only
/// readout for a debug HUD. The last rewind's verdict, distance and gate outlast the snapshots
/// that agree, for a "why did that pop" readout.
#[derive(Component, Debug)]
pub struct PredictionStatus {
    pub(super) last_error: f32,
    pub(super) last_ack_sequence: u32,
    pub(super) last_server_tick: u64,
    pub(super) replayed_commands: usize,
    pub(super) mode: CorrectionMode,
    pub(super) last_rewind_mode: CorrectionMode,
    pub(super) last_correction_distance: f32,
    pub(super) last_snap_gate: f32,
    /// The newest command sent, written as prediction sends it.
    pub(super) newest_sequence: u32,
    /// Set on [`CorrectionMode::Dropped`]: corrections park until the ack reaches this sequence,
    /// because everything below it is backlog the server is still draining.
    pub(super) resync_floor: Option<u32>,
}

impl Default for PredictionStatus {
    fn default() -> Self {
        Self {
            last_error: 0.0,
            last_ack_sequence: 0,
            last_server_tick: 0,
            replayed_commands: 0,
            mode: CorrectionMode::Waiting,
            last_rewind_mode: CorrectionMode::Waiting,
            last_correction_distance: 0.0,
            last_snap_gate: 0.0,
            newest_sequence: 0,
            resync_floor: None,
        }
    }
}

impl PredictionStatus {
    /// What the last owner snapshot did.
    pub fn mode(&self) -> CorrectionMode {
        self.mode
    }

    /// How far the prediction was from the server at the last ack it judged (m).
    pub fn last_error(&self) -> f32 {
        self.last_error
    }

    /// The last ack it judged.
    pub fn last_ack_sequence(&self) -> u32 {
        self.last_ack_sequence
    }

    /// The server tick of the last owner snapshot it judged.
    pub fn last_server_tick(&self) -> u64 {
        self.last_server_tick
    }

    /// Commands the last correction replayed; 0 if none ran.
    pub fn replayed_commands(&self) -> usize {
        self.replayed_commands
    }

    /// Commands sent after the last ack it judged: how far the acks trail the input.
    pub fn unacked(&self) -> u32 {
        self.newest_sequence.wrapping_sub(self.last_ack_sequence)
    }

    /// The last rewind's verdict: [`CorrectionMode::Replayed`], [`CorrectionMode::Snapped`] or
    /// [`CorrectionMode::MissingHistory`], [`CorrectionMode::Waiting`] before the first.
    pub fn last_rewind_mode(&self) -> CorrectionMode {
        self.last_rewind_mode
    }

    /// How far the last rewind moved the prediction (m).
    pub fn last_correction_distance(&self) -> f32 {
        self.last_correction_distance
    }

    /// The snap gate the last rewind was judged against (m).
    pub fn last_snap_gate(&self) -> f32 {
        self.last_snap_gate
    }
}

/// The corrections reconciliation made to the local prediction, on its [`PredictedPlayer`], for
/// the game's presentation to [`take`](Self::take) after
/// [`NetAhoyClientSystems::Reconcile`](crate::NetAhoyClientSystems::Reconcile). The prediction
/// takes the corrected state at once; the game adds the deltas to its drawn pose's error so the
/// drawn pose stays continuous. Whether a big correction is hidden or popped is the game's call:
/// netahoy only reports the gate it computed.
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub enum PendingCorrection {
    /// Nothing to fold.
    #[default]
    None,
    /// Reconcile moved the prediction by these deltas, measured across the restore and replay.
    Fold {
        /// Position before the correction minus after (m).
        position: Vec3,
        /// Velocity before the correction minus after (m/s).
        velocity: Vec3,
        /// Past this distance (m) the correction is teleport-class: the game may pop it, or
        /// fold it anyway.
        snap_gate: f32,
    },
    /// Not a correction: drop any drawn error and adopt the state as it is.
    Snap,
}

impl PendingCorrection {
    /// Stack a correction onto an unconsumed one: folds sum, a snap wins.
    pub fn merge(&mut self, next: PendingCorrection) {
        *self = match (*self, next) {
            (PendingCorrection::Snap, _) | (_, PendingCorrection::Snap) => PendingCorrection::Snap,
            (any, PendingCorrection::None) => any,
            (PendingCorrection::None, next) => next,
            (
                PendingCorrection::Fold {
                    position: p0,
                    velocity: v0,
                    snap_gate: g0,
                },
                PendingCorrection::Fold {
                    position: p1,
                    velocity: v1,
                    snap_gate: g1,
                },
            ) => PendingCorrection::Fold {
                position: p0 + p1,
                velocity: v0 + v1,
                snap_gate: g0.max(g1),
            },
        };
    }

    /// Takes the correction, leaving [`Self::None`].
    pub fn take(&mut self) -> PendingCorrection {
        core::mem::take(self)
    }
}

/// What reconciliation did with one owner snapshot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CorrectionMode {
    /// No verdict: no ack yet, the same ack again, or corrections parked after [`Self::Dropped`].
    Waiting,
    /// The ack had no local frame to compare with: rewound and replayed anyway.
    MissingHistory,
    /// The prediction agreed at the ack: nothing ran.
    Ignored,
    /// Rewound to the ack and replayed; the prediction moved less than the snap gate.
    Replayed,
    /// Rewound to the ack and replayed; the prediction moved past the snap gate.
    Snapped,
    /// The ack lagged past the replay cap: corrections park until it reaches the newest command
    /// sent then.
    Dropped,
}

pub(super) fn reconcile_local_prediction<G: PlayerGame>(
    local: Res<OwnPlayerId>,
    server_players: Query<(&PlayerId, &OwnerSnapshot<G>), With<ServerPlayer>>,
    mut predictions: Query<(
        Entity,
        &PredictedPlayer,
        &mut PredictionStatus,
        &mut PendingCorrection,
    )>,
    mut history: ResMut<PredictionHistory<G>>,
    command_history: Res<LocalCommandHistory>,
    mut events: ResMut<Outbox<G::Event>>,
    mut stepper: Stepper<G>,
) {
    let Some(local_id) = local.0 else {
        return;
    };
    let Ok((predicted_entity, prediction, mut status, mut pending)) = predictions.single_mut()
    else {
        return;
    };
    let Ok((player_id, snapshot)) = server_players.get(prediction.server_entity) else {
        return;
    };
    if *player_id != local_id {
        return;
    }

    if snapshot.server_tick <= status.last_server_tick {
        return;
    }

    if snapshot.last_processed_sequence == 0 {
        status.mode = CorrectionMode::Waiting;
        status.last_server_tick = snapshot.server_tick;
        status.last_error = 0.0;
        status.replayed_commands = 0;
        return;
    }

    // After a drop the server is still draining commands from before it, and acks in that
    // window judge nothing. It steps the commands this client already predicted, so its truth
    // converges on its own.
    if let Some(floor) = status.resync_floor {
        if sequence_is_newer(floor, snapshot.last_processed_sequence) {
            status.mode = CorrectionMode::Waiting;
            status.last_server_tick = snapshot.server_tick;
            status.last_ack_sequence = snapshot.last_processed_sequence;
            return;
        }
        status.resync_floor = None;
    }

    // A repeated ack: the server stepped a guess, not a new command, so this snapshot judges no
    // newer frame.
    if snapshot.last_processed_sequence == status.last_ack_sequence {
        status.mode = CorrectionMode::Waiting;
        status.last_server_tick = snapshot.server_tick;
        return;
    }

    let ack_frame = history.get(snapshot.last_processed_sequence).cloned();
    let history_error = ack_frame.as_ref().map(|ack_frame| {
        let delta = snapshot.position() - ack_frame.position();
        let xz_error = delta.xz().length();
        let y_error = delta.y.abs();
        let total_error = delta.length();
        // The rest of the state counts too: a disagreement there must rewind even at zero
        // position error.
        let state_mismatch = !moves_agree(&snapshot.moves, &ack_frame.state.moves)
            || !snapshot.game.agrees(&ack_frame.state.game);
        let ignore_y = snapshot.moves.ground.is_some() && y_error <= IGNORE_GROUNDED_Y_ERROR;
        (total_error, state_mismatch, xz_error, y_error, ignore_y)
    });

    if let Some((total_error, state_mismatch, xz_error, y_error, ignore_y)) = history_error
        && !state_mismatch
        && xz_error <= IGNORE_XZ_ERROR
        && (ignore_y || y_error <= IGNORE_XZ_ERROR)
    {
        history.retain_after(snapshot.last_processed_sequence);
        status.mode = CorrectionMode::Ignored;
        status.last_server_tick = snapshot.server_tick;
        status.last_ack_sequence = snapshot.last_processed_sequence;
        status.last_error = total_error;
        status.replayed_commands = 0;
        return;
    }

    match (&ack_frame, history_error) {
        (Some(frame), Some((total_error, state_mismatch, xz_error, y_error, _))) => {
            // Velocity is in no gate: a redirect (bounce, wall jump) diverging on one peer shows
            // here a tick before it becomes position error.
            info!(
                "mispredict ack {} err {:.3} (xz {:.3} y {:.3} vel {:.3} state_mismatch {}) seen {}:{:.3}",
                snapshot.last_processed_sequence,
                total_error,
                xz_error,
                y_error,
                (frame.velocity() - snapshot.velocity()).length(),
                state_mismatch,
                frame.command.seen_server_tick,
                frame.command.seen_alpha
            );
            debug!(
                "  pose: cli {:.6?} srv {:.6?} delta(srv-cli) {:.6?} | cmd move {:.3?} look {:.3?} buttons {:?} srv_buttons {:?}",
                frame.position(),
                snapshot.position(),
                snapshot.position() - frame.position(),
                frame.command.movement,
                frame.command.look,
                frame.command.buttons,
                snapshot.last_processed_buttons,
            );
            if !moves_agree(&snapshot.moves, &frame.state.moves) {
                debug!(
                    "  moves cli {:?} vs srv {:?}",
                    frame.state.moves, snapshot.moves
                );
            }
            if !snapshot.game.agrees(&frame.state.game) {
                debug!(
                    "  game cli {:?} vs srv {:?}",
                    frame.state.game, snapshot.game
                );
            }
        }
        _ => info!(
            "mispredict ack {}: no local frame for that sequence (MissingHistory rewind)",
            snapshot.last_processed_sequence
        ),
    }

    let current_position = stepper
        .position(predicted_entity)
        .unwrap_or(snapshot.position());
    let current_velocity = stepper
        .velocity(predicted_entity)
        .unwrap_or(snapshot.velocity());
    let replay_commands = command_history.after_sequence(snapshot.last_processed_sequence);
    let max_replay = max_replay_commands(stepper.tick_rate()) as usize;

    // A window past the cap means acks stopped. Clearing the history would leave the server
    // acking into the cleared range for its whole drain: keep predicting, and park corrections
    // until the ack reaches the newest command sent.
    if replay_commands.len() > max_replay {
        let floor = command_history
            .commands
            .back()
            .map(|command| command.sequence);
        debug!(
            "Dropped: ack {} lags {} cmds (> {max_replay}), parking corrections until ack {:?}",
            snapshot.last_processed_sequence,
            replay_commands.len(),
            floor,
        );
        status.resync_floor = floor;
        status.mode = CorrectionMode::Dropped;
        status.last_server_tick = snapshot.server_tick;
        status.last_ack_sequence = snapshot.last_processed_sequence;
        status.last_error = history_error
            .map(|(total_error, _, _, _, _)| total_error)
            .unwrap_or(0.0);
        status.replayed_commands = 0;
        return;
    }

    stepper.restore(predicted_entity, snapshot);
    let restored_position = stepper
        .position(predicted_entity)
        .unwrap_or(snapshot.position());

    // Replay captures only frames after the ack: keep the server's state as the frame at it.
    if ack_frame.is_none() {
        let baseline = UserCmd {
            sequence: snapshot.last_processed_sequence,
            buttons: snapshot.last_processed_buttons,
            ..Default::default()
        };
        if let Some(frame) = stepper.capture_frame(predicted_entity, baseline) {
            history.push(frame);
        }
    }

    let replayed = replay_commands.len();
    let mut previous_sequence = snapshot.last_processed_sequence;
    let mut previous_buttons = snapshot.last_processed_buttons;
    for command in replay_commands {
        let before = stepper
            .position(predicted_entity)
            .unwrap_or(snapshot.position());
        if let Err(err) = stepper.player_move(
            predicted_entity,
            &mut events,
            command,
            previous_sequence,
            previous_buttons,
        ) {
            warn!(
                "failed to replay predicted KCC for command {}: {err}",
                command.sequence
            );
        }
        let after = stepper
            .position(predicted_entity)
            .unwrap_or(snapshot.position());
        let velocity = stepper
            .velocity(predicted_entity)
            .unwrap_or(snapshot.velocity());
        debug!(
            "  replay cmd {}: pos {:.6?} -> {:.6?} delta {:.6?} vel {:.6?} move {:.3?} buttons {:?} seen {}:{:.3}",
            command.sequence,
            before,
            after,
            after - before,
            velocity,
            command.movement,
            command.buttons,
            command.seen_server_tick,
            command.seen_alpha,
        );
        if let Some(frame) = stepper.capture_frame(predicted_entity, command) {
            history.push(frame);
        }
        previous_sequence = command.sequence;
        previous_buttons = command.buttons;
    }

    history.retain_after(snapshot.last_processed_sequence);

    let new_position = stepper
        .position(predicted_entity)
        .unwrap_or(snapshot.position());
    let new_velocity = stepper
        .velocity(predicted_entity)
        .unwrap_or(snapshot.velocity());
    let correction_distance = current_position.distance(new_position);
    // Teleport-class is relative: 3 m at 30 m/s is ground the player was covering anyway, 3 m
    // standing still is a teleport. So the gate grows with speed and with the ground the replay
    // window covered.
    let window_travel = snapshot.position().distance(new_position);
    let snap_distance = SNAP_ERROR_DISTANCE
        .max(current_velocity.length().max(new_velocity.length()) * SNAP_ERROR_TRAVEL_SECS)
        .max(window_travel * SNAP_WINDOW_FRACTION);

    status.mode = if ack_frame.is_none() {
        CorrectionMode::MissingHistory
    } else if correction_distance >= snap_distance {
        CorrectionMode::Snapped
    } else {
        CorrectionMode::Replayed
    };
    // Only the rewind path logs: the ignore path runs on nearly every snapshot.
    let (xz_error, y_error, state_mismatch) = history_error
        .map(|(_, state_mismatch, xz_error, y_error, _)| (xz_error, y_error, state_mismatch))
        .unwrap_or_default();
    debug!(
        "{:?}: ack {}, replayed {} cmds, xz {:.3}m, y {:.3}m, state_mismatch {}, pose pre {:.6?} restore {:.6?} post {:.6?}, moved {:.3}m, gate {:.3}m",
        status.mode,
        snapshot.last_processed_sequence,
        replayed,
        xz_error,
        y_error,
        state_mismatch,
        current_position,
        restored_position,
        new_position,
        correction_distance,
        snap_distance,
    );
    status.last_rewind_mode = status.mode;
    status.last_correction_distance = correction_distance;
    status.last_snap_gate = snap_distance;
    status.last_server_tick = snapshot.server_tick;
    status.last_ack_sequence = snapshot.last_processed_sequence;
    status.last_error = history_error
        .map(|(total_error, _, _, _, _)| total_error)
        .unwrap_or(correction_distance);
    status.replayed_commands = replayed;
    // Over the gate too: the prediction is corrected either way, and pop or hide is the game's.
    pending.merge(PendingCorrection::Fold {
        position: current_position - new_position,
        velocity: current_velocity - new_velocity,
        snap_gate: snap_distance,
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        game::{PlayerState, test_game::TestGame},
        stepper::PredictionFrame,
    };

    const RATE: TickRate = TickRate::new(20);

    /// A game half of one float that keeps [`PlayerGame::agrees`]' default, `==`.
    #[derive(Clone, Copy, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
    struct Exact(f32);

    /// The same float, agreeing within a thousandth.
    #[derive(Clone, Copy, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
    struct Tolerant(f32);

    impl bevy::ecs::entity::MapEntities for Exact {
        fn map_entities<E: bevy::ecs::entity::EntityMapper>(&mut self, _: &mut E) {}
    }

    impl bevy::ecs::entity::MapEntities for Tolerant {
        fn map_entities<E: bevy::ecs::entity::EntityMapper>(&mut self, _: &mut E) {}
    }

    impl PlayerGame for Exact {
        type Event = crate::game::test_game::TestEvent;
        type Context = ();

        fn step(_: &mut PlayerState<Self>, _: &mut crate::game::Step<Self>) {}
    }

    impl PlayerGame for Tolerant {
        type Event = crate::game::test_game::TestEvent;
        type Context = ();

        fn step(_: &mut PlayerState<Self>, _: &mut crate::game::Step<Self>) {}

        fn agrees(&self, other: &Self) -> bool {
            (self.0 - other.0).abs() <= 1e-3
        }
    }

    /// The ack every case here starts from.
    const ACK: u32 = 5;

    /// One reconcile of a client that predicted `predicted` at the ack, against a server that
    /// has `server` there, the movement half the same on both and one command after the ack:
    /// what reconcile did, how many commands it replayed, and the game half it left.
    fn reconcile<G: PlayerGame>(predicted: G, server: G) -> (CorrectionMode, usize, G) {
        reconcile_at(RATE, predicted, server, 1)
    }

    /// [`reconcile`] at `rate`, with `unacked` commands sent after the ack.
    fn reconcile_at<G: PlayerGame>(
        rate: TickRate,
        predicted: G,
        server: G,
        unacked: u32,
    ) -> (CorrectionMode, usize, G) {
        let moves = PlayerState::<G>::at(Vec3::Y).moves;
        let state = |game| PlayerState { moves, game };
        let (mut app, prediction, _) =
            client_at(rate, Some(state(predicted)), state(server), unacked);
        let (mode, replayed, state) = judge::<G>(&mut app, prediction);
        (mode, replayed, state.game)
    }

    /// A client at `rate` whose prediction stood at `predicted` after the ack (`None`: it kept
    /// no frame there) and still stands there, `unacked` commands sent after it, and the
    /// server's player at `server` at the ack: the app, the prediction and the server's player.
    fn client_at<G: PlayerGame>(
        rate: TickRate,
        predicted: Option<PlayerState<G>>,
        server: PlayerState<G>,
        unacked: u32,
    ) -> (App, Entity, Entity) {
        use bevy_ahoy::CharacterController;

        let mut app = crate::client::stepping_app::<G>(rate);
        app.insert_resource(OwnPlayerId(Some(PlayerId(1))));

        let world = app.world_mut();
        let snapshot = OwnerSnapshot {
            server_tick: 10,
            last_processed_sequence: ACK,
            moves: server.moves,
            game: server.game,
            ..default()
        };
        let server_entity = world.spawn((PlayerId(1), ServerPlayer, snapshot)).id();
        let player = world
            .spawn((
                PlayerId(1),
                PredictedPlayer { server_entity },
                predicted.unwrap_or(server),
                CharacterController::default(),
            ))
            .id();
        let command = |sequence| UserCmd {
            sequence,
            ..default()
        };
        if let Some(state) = predicted {
            world
                .resource_mut::<PredictionHistory<G>>()
                .push(PredictionFrame {
                    command: command(ACK),
                    state,
                });
        }
        let mut sent = world.resource_mut::<LocalCommandHistory>();
        for sequence in ACK..=ACK + unacked {
            sent.push(command(sequence));
        }
        (app, player, server_entity)
    }

    /// One reconcile of `prediction` against its server player's snapshot: what it did, how
    /// many commands it replayed, and the prediction it left.
    fn judge<G: PlayerGame>(
        app: &mut App,
        prediction: Entity,
    ) -> (CorrectionMode, usize, PlayerState<G>) {
        use bevy::ecs::system::RunSystemOnce;

        let world = app.world_mut();
        world
            .run_system_once(reconcile_local_prediction::<G>)
            .unwrap();
        let status = world.get::<PredictionStatus>(prediction).unwrap();
        let state = *world.get::<PlayerState<G>>(prediction).unwrap();
        (status.mode, status.replayed_commands, state)
    }

    /// The server's next snapshot of `server`: one tick on, acking `ack`, the player unchanged.
    fn deliver<G: PlayerGame>(app: &mut App, server: Entity, ack: u32) {
        let mut snapshot = app.world_mut().get_mut::<OwnerSnapshot<G>>(server).unwrap();
        snapshot.server_tick += 1;
        snapshot.last_processed_sequence = ack;
    }

    /// A [`TestGame`] player at `position`, standing on the ground when `grounded`.
    fn standing(position: Vec3, grounded: bool) -> PlayerState<TestGame> {
        let mut state = PlayerState::<TestGame>::at(position);
        state.moves.ground = grounded.then_some(bevy_ahoy::moves::Ground {
            normal: Vec3::Y,
            friction: 0.6,
        });
        state
    }

    /// One reconcile of a [`TestGame`] client that predicted the player `off` the server's at the
    /// ack: what it did, how many commands it replayed, the prediction it left, and the
    /// prediction and the server's player it started from.
    fn judge_off(
        off: Vec3,
        grounded: bool,
    ) -> (
        CorrectionMode,
        usize,
        PlayerState<TestGame>,
        [PlayerState<TestGame>; 2],
    ) {
        let predicted = standing(Vec3::Y, grounded);
        let server = standing(Vec3::Y + off, grounded);
        let (mut app, prediction, _) = client_at(RATE, Some(predicted), server, 1);
        let (mode, replayed, state) = judge(&mut app, prediction);
        (mode, replayed, state, [predicted, server])
    }

    /// The prediction stands when it is within the thresholds of the server's: across, under
    /// [`IGNORE_XZ_ERROR`]; up or down, under it in the air and under
    /// [`IGNORE_GROUNDED_Y_ERROR`] on the ground. Past any of them it rewinds.
    #[test]
    fn a_prediction_within_the_thresholds_is_kept() {
        let kept = |off: Vec3, grounded| {
            let (mode, replayed, state, [predicted, _]) = judge_off(off, grounded);
            match mode {
                CorrectionMode::Ignored => {
                    assert_eq!((replayed, state), (0, predicted), "{off} kept as it was");
                    true
                }
                _ => false,
            }
        };
        assert!(kept(Vec3::new(0.02, 0.0, -0.02), false));
        assert!(kept(Vec3::Y * 0.03, false));
        assert!(kept(Vec3::Y * -0.15, true));
        assert!(!kept(Vec3::X * 0.04, false));
        assert!(!kept(Vec3::Y * 0.04, false));
        assert!(!kept(Vec3::Y * -0.25, true));
    }

    /// Past the thresholds the prediction rewinds to the server's player and replays the
    /// commands after the ack from there. A correction that moves it less than the snap gate
    /// ([`SNAP_ERROR_DISTANCE`] standing still) is Replayed, one past it Snapped; either way
    /// the presentation is handed the whole of it to fold.
    #[test]
    fn a_correction_past_the_snap_gate_snaps() {
        for (off, verdict) in [
            (0.5, CorrectionMode::Replayed),
            (3.0, CorrectionMode::Snapped),
        ] {
            let off = Vec3::X * off;
            let (mode, replayed, state, [predicted, server]) = judge_off(off, false);
            assert_eq!((mode, replayed), (verdict, 1), "{off}");
            assert_eq!(
                state.moves.position.xz(),
                server.moves.position.xz(),
                "{off}: replayed from the server's"
            );
            assert!(
                state.moves.position.y < predicted.moves.position.y,
                "and fell"
            );
        }
    }

    /// An ack the prediction kept no frame for still rewinds and replays, and keeps the
    /// server's player as the frame there.
    #[test]
    fn an_ack_with_no_local_frame_rewinds_and_keeps_the_servers() {
        let server = standing(Vec3::Y, false);
        let (mut app, prediction, _) = client_at(RATE, None, server, 1);
        let (mode, replayed, _) = judge::<TestGame>(&mut app, prediction);
        assert_eq!((mode, replayed), (CorrectionMode::MissingHistory, 1));
        let history = app.world().resource::<PredictionHistory<TestGame>>();
        assert_eq!(history.get(ACK).map(|frame| frame.state), Some(server));
    }

    /// Past the replay cap a correction is Dropped: nothing replays, and corrections park until
    /// the ack reaches the newest command sent then. Below it every snapshot waits and the
    /// prediction is left alone; at it, reconcile judges again.
    #[test]
    fn a_dropped_correction_parks_until_the_ack_reaches_the_newest_command() {
        let unacked = MAX_REPLAY_COMMANDS as u32 + 1;
        let predicted = standing(Vec3::Y, false);
        let server = standing(Vec3::new(1.0, 1.0, 0.0), false);
        let (mut app, prediction, server_entity) =
            client_at(RATE, Some(predicted), server, unacked);
        assert_eq!(
            judge::<TestGame>(&mut app, prediction),
            (CorrectionMode::Dropped, 0, predicted)
        );
        let floor = ACK + unacked;
        let parked = |app: &App| {
            app.world()
                .get::<PredictionStatus>(prediction)
                .unwrap()
                .resync_floor
        };
        assert_eq!(parked(&app), Some(floor));

        deliver::<TestGame>(&mut app, server_entity, floor - 1);
        assert_eq!(
            judge::<TestGame>(&mut app, prediction),
            (CorrectionMode::Waiting, 0, predicted),
            "below the floor"
        );
        deliver::<TestGame>(&mut app, server_entity, floor);
        let (mode, ..) = judge::<TestGame>(&mut app, prediction);
        assert_eq!(
            (mode, parked(&app)),
            (CorrectionMode::MissingHistory, None),
            "at the floor: judged, against a frame it never kept"
        );
    }

    /// Reconcile holds the game half to the game's own [`PlayerGame::agrees`]: a difference in
    /// the last bits that it accepts keeps the prediction, the default `==` rewinds to the
    /// server's and replays from there.
    #[test]
    fn reconcile_rewinds_only_on_a_game_half_the_game_disagrees_with() {
        let (ours, theirs) = (1.0, 1.0 + 1e-5);
        assert_eq!(
            reconcile(Tolerant(ours), Tolerant(theirs)),
            (CorrectionMode::Ignored, 0, Tolerant(ours))
        );
        assert_eq!(
            reconcile(Exact(ours), Exact(theirs)),
            (CorrectionMode::Replayed, 1, Exact(theirs))
        );
    }

    /// A correction replays up to its cap of commands, at 60 Hz as at 20; with one more unacked
    /// it parks until the ack catches up.
    #[test]
    fn a_correction_replays_up_to_its_cap_at_any_rate() {
        for rate in [RATE, TickRate::new(60)] {
            let (mode, replayed, _) = reconcile_at(rate, Exact(0.0), Exact(1.0), 50);
            assert_eq!((mode, replayed), (CorrectionMode::Replayed, 50));
            let (mode, replayed, _) = reconcile_at(rate, Exact(0.0), Exact(1.0), 51);
            assert_eq!((mode, replayed), (CorrectionMode::Dropped, 0));
        }
    }

    /// A correction's replay steps a predicted fire again, and the session passes it on once:
    /// the game reads the prediction's, and nothing more.
    #[test]
    fn a_replay_over_a_predicted_fire_passes_it_on_once() {
        use bevy::ecs::{message::Messages, system::RunSystemOnce};

        use crate::{
            client::predict::LocalTimeline, events::EventId, game::test_game::TestEvent,
            protocol::SubtickFire,
        };

        let predicted = standing(Vec3::Y, false);
        let server = standing(Vec3::Y + Vec3::X, false);
        let (mut app, prediction, _) = client_at(RATE, Some(predicted), server, 0);
        let fire = SubtickFire {
            frac: 0.5,
            look: Vec2::ZERO,
            origin: None,
        };
        let commands: Vec<UserCmd> = (ACK + 1..=ACK + 3)
            .map(|sequence| UserCmd {
                sequence,
                fire: (sequence == ACK + 2).then_some(fire),
                ..default()
            })
            .collect();
        // Predicted on the session's timeline, as the prediction steps them.
        let world = app.world_mut();
        world
            .run_system_once(
                move |mut stepper: Stepper<TestGame>, mut timeline: LocalTimeline<TestGame>| {
                    let mut previous = ACK;
                    for &command in &commands {
                        stepper
                            .player_move(
                                prediction,
                                &mut timeline.events,
                                command,
                                previous,
                                default(),
                            )
                            .unwrap();
                        timeline.sent.push(command);
                        previous = command.sequence;
                    }
                },
            )
            .unwrap();
        let written = |world: &mut World| -> Vec<TestEvent> {
            world
                .resource_mut::<Messages<TestEvent>>()
                .drain()
                .collect()
        };
        let fired = TestEvent::Fired(EventId {
            owner: PlayerId(1),
            sequence: ACK + 2,
        });
        assert_eq!(written(world), [fired], "the prediction's fire");

        let (mode, replayed, _) = judge::<TestGame>(&mut app, prediction);
        assert_eq!((mode, replayed), (CorrectionMode::Replayed, 3));
        assert_eq!(written(app.world_mut()), [], "the replay's is a repeat");
    }

    /// State follows the last run, events follow the first. At 60 Hz a predicted direct hit that
    /// the server's state says missed: the correction's replay moves the blast to the rocket's
    /// fuse, 81 commands on, past the 50 a replay reaches. The prediction blasts there, and the
    /// hit, already out, is not written again.
    #[test]
    fn a_hit_a_correction_moves_past_its_replays_is_written_once() {
        use bevy::{
            ecs::{message::Messages, system::RunSystemOnce},
            state::app::StatesPlugin,
        };
        use bevy_replicon::prelude::RepliconPlugins;

        use crate::{
            client::{
                ClientInput, ClientInputState, LinkProfile, PresentedFrame, RemoteClock,
                predict::{drive_prediction_and_send_input, record_prediction_state},
            },
            events::EventId,
            game::test_game::{Fuse, TestEvent},
            poses::RemoteRenderTime,
            protocol::SubtickFire,
        };

        let rate = TickRate::new(60);
        let start = PlayerState::<Fuse>::at(Vec3::Y);
        let (mut app, prediction, server) = client_at(rate, Some(start), start, 0);
        app.add_plugins((
            StatesPlugin,
            RepliconPlugins,
            NetAhoyProtocolPlugin::<Fuse>::new(rate),
        ))
        .insert_resource(RemoteClock::new(rate))
        .insert_resource(LinkProfile::new(rate))
        .init_resource::<ClientInput>()
        .init_resource::<PresentedFrame>();
        app.finish();
        let mut input = app.world_mut().resource_mut::<ClientInputState>();
        (input.next_sequence, input.previous_sequence) = (ACK, ACK);

        fn written(world: &mut World) -> Vec<TestEvent> {
            world
                .resource_mut::<Messages<TestEvent>>()
                .drain()
                .collect()
        }

        let (fire, flight) = (ACK + 1, ACK + 2);
        // One command through the prediction: the click on `fire`, the target seen on the
        // rocket's first command of flight. What it wrote.
        let predict = move |app: &mut App| -> Vec<TestEvent> {
            let world = app.world_mut();
            let sequence = world.resource::<ClientInputState>().next_sequence + 1;
            if sequence == fire {
                world.resource_mut::<ClientInput>().fire = Some(SubtickFire {
                    frac: 0.5,
                    look: Vec2::ZERO,
                    origin: None,
                });
            }
            let seen = if sequence == flight {
                Fuse::TARGET_TICK
            } else {
                0
            };
            world.resource_mut::<PresentedFrame>().remote = Some(RemoteRenderTime::new(seen, 0.0));
            world
                .run_system_once(drive_prediction_and_send_input::<Fuse>)
                .unwrap();
            world
                .run_system_once(record_prediction_state::<Fuse>)
                .unwrap();
            written(world)
        };
        let rocket = |app: &App| {
            app.world()
                .get::<PlayerState<Fuse>>(prediction)
                .unwrap()
                .game
                .rocket
        };
        let id = EventId {
            owner: PlayerId(1),
            sequence: fire,
        };
        let (fired, hit) = (TestEvent::Fired(id), TestEvent::Hit(id));

        let predicted: Vec<_> = (fire..=fire + 3).flat_map(|_| predict(&mut app)).collect();
        assert_eq!(predicted, [fired, hit], "a predicted direct hit");
        assert_eq!(rocket(&app), None);

        // The server's rocket missed: past its first command of flight it still flies.
        deliver::<Fuse>(&mut app, server, flight);
        app.world_mut()
            .get_mut::<OwnerSnapshot<Fuse>>(server)
            .unwrap()
            .game
            .rocket = Some(fire);
        let (mode, replayed, state) = judge::<Fuse>(&mut app, prediction);
        assert_eq!((mode, replayed), (CorrectionMode::Replayed, 2));
        assert_eq!(state.game.rocket, Some(fire), "the replay flies on");
        assert_eq!(written(app.world_mut()), [], "the replay is no first run");

        let due = fire + rate.ticks(Fuse::SECONDS) as u32;
        assert_eq!(due, 87);
        let later: Vec<_> = (fire + 4..=due + 10)
            .flat_map(|_| predict(&mut app))
            .collect();
        assert_eq!(rocket(&app), None, "the prediction blasted at the fuse");
        assert_eq!(later, [], "the moved blast is the hit already written");
    }
}
