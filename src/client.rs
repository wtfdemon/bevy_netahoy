//! Lives on the player's machine: guesses where they're going (prediction),
//! fixes the guess when the server disagrees (reconciliation), draws everyone.

use std::collections::VecDeque;

use avian3d::prelude::{LinearVelocity, TransformEasingSystems};
use bevy::{
    app::{RunFixedMainLoop, RunFixedMainLoopSystems},
    prelude::*,
};
use bevy_replicon::prelude::*;

use crate::{
    demo::DemoPlayback,
    math::{
        LagCompensationHistory, REMOTE_EXTRAPOLATION_SECONDS,
        REMOTE_INTERPOLATION_TELEPORT_DISTANCE, RemoteRenderTime, RemoteSnapshotSample,
        sample_buffer_at,
    },
    player::{NetAhoyPlayerEvents, NetAhoyPlayerState},
    protocol::*,
    step::{AhoyPredictionFrame, NetAhoyStepper},
};

/// Link-dependent send knobs. Auto-derived from the [`LinkLossModel`] tag
/// the game puts on its connecting session entity ([`apply_link_profile`]);
/// settable directly for exotic setups. The default is the stream profile:
/// TCP redelivers everything, so redundant command copies buy nothing — and
/// their bandwidth is itself the failure mode on throttled uplinks, flooding
/// the bufferedAmount door (tests/link_sim.rs, the 96kbps rows). On lossy
/// datagram transports the redundant tail IS the recovery path: a lost
/// packet's commands ride the next one, ~50ms later.
#[derive(Resource, Clone, Copy, Debug)]
pub struct NetAhoyLinkProfile {
    /// Cap on commands per usercmd packet. Each packet carries every
    /// still-unacked command up to this cap, so the tail self-scales to the
    /// link's real RTT (~2 for Virginia, ~5 for Sydney) instead of a flat
    /// worst case. 1 = newest only (reliable transports).
    pub usercmd_backup_cap: usize,
}

impl Default for NetAhoyLinkProfile {
    fn default() -> Self {
        Self { usercmd_backup_cap: 1 }
    }
}

/// The datagram-link cap: deep enough for an ocean of unacked commands
/// plus a congestion stall's backlog, small enough that even a full tail is
/// ~13 kB/s upstream at 20Hz.
pub const DATAGRAM_USERCMD_BACKUP: usize = 16;

/// Derive the link profile from the game's [`LinkLossModel`] tag on the
/// connecting session entity.
fn apply_link_profile(
    models: Query<&LinkLossModel, Added<LinkLossModel>>,
    mut profile: ResMut<NetAhoyLinkProfile>,
) {
    for model in &models {
        profile.usercmd_backup_cap = match model {
            LinkLossModel::Stream => 1,
            LinkLossModel::Datagram => DATAGRAM_USERCMD_BACKUP,
        };
    }
}
pub const PREDICTION_HISTORY_CAPACITY: usize = 256;
/// Rewind+replay bail-out: past ~1.5s of unacked input (20Hz cmds) the replay
/// itself becomes the problem — hundreds of KCC steps per snapshot balloon
/// frame time, which delays outgoing cmds and widens the window further (the
/// death spiral). Cheaper to eat one visible snap and start fresh.
// Moves together with SERVER_CMD_QUEUE_CAPACITY: the client must be willing
// to replay at least as deep as the server is willing to queue, or burst
// recovery trips the Dropped/resync path instead of a normal rewind.
pub const MAX_REPLAY_COMMANDS: usize = 50;
pub const REMOTE_INTERPOLATION_CAPACITY: usize = 64;
/// Three snapshots (150 ms at 20 Hz). The margin against delivery stalls is
/// `delay - burst_length`, and production is browser websockets: TCP turns
/// loss into a retransmit stall then an in-order burst of 2-4 ticks, so two
/// ticks of delay left ~zero margin and routine dead-reckoning at the head.
pub const REMOTE_INTERPOLATION_DELAY_TICKS: u64 = 3;
/// If playback falls this far behind the delayed head, resync in one jump.
/// Normal operation advances by exactly one server tick per local fixed tick.
const REMOTE_CLOCK_RESET_TICKS: f64 = 10.0;
/// Catch-up slew: when playback lags the delayed head (local fixed clock
/// running slower than the server's, or a burst backing up the buffer),
/// replay slightly fast instead of letting lag grow until the reset snap.
/// A few percent reads as smooth motion; a snap reads as a teleport.
const REMOTE_CLOCK_SLEW_TICKS: f64 = 1.0;
const REMOTE_CLOCK_SLEW_RATE: f64 = 1.05;
const REMOTE_CLOCK_FAST_SLEW_TICKS: f64 = 4.0;
const REMOTE_CLOCK_FAST_SLEW_RATE: f64 = 1.10;
pub const IGNORE_XZ_ERROR: f32 = 0.035;
pub const IGNORE_GROUNDED_Y_ERROR: f32 = 0.20;
pub const SNAP_ERROR_DISTANCE: f32 = 2.25;
/// Speed-relative widening of the snap gate: a correction only pops (skips
/// smoothing) past this many seconds of travel at the current speed. At
/// 30 m/s that's 7.5 m of smoothed correction where the absolute floor
/// alone would pop at 2.25 m.
pub const SNAP_ERROR_TRAVEL_SECS: f32 = 0.25;
/// Window-relative widening of the snap gate: also fold when the error is
/// under this fraction of the ground the replay window itself covered.
/// Catches errors EARNED at speed but JUDGED after slowing down (mispredict
/// a landing at 25 m/s, friction kills the speed, correction arrives at
/// walking pace — the speed term alone shrinks back to the floor and pops
/// a correction that was modest relative to the meters it spans).
pub const SNAP_WINDOW_FRACTION: f32 = 0.4;

#[derive(SystemSet, Debug, Clone, Copy, Hash, PartialEq, Eq)]
pub enum ClientNetAhoySystems {
    /// Rewind + replay against fresh server snapshots (`FixedPreUpdate`).
    Reconcile,
    /// Build this tick's user command, predict it, send it (`FixedPreUpdate`).
    Predict,
    /// Per-tick remote pose sampling (`FixedLast`).
    SampleRemotes,
    /// Per-frame bookkeeping and the local player's presentation bridge
    /// (`Update`). Game presentation that reads the local visual (camera,
    /// HUD) orders after this.
    Interpolate,
}

pub struct ClientNetAhoyPlugin;

impl Plugin for ClientNetAhoyPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<LocalPlayerId>()
            .init_resource::<NetAhoyLinkProfile>()
            .add_systems(Update, apply_link_profile)
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
            .add_systems(
                OnEnter(ClientState::Connected),
                (reset_server_clock, announce_join).chain(),
            )
            .add_systems(
                PreUpdate,
                (buffer_remote_snapshot_frames, buffer_remote_snapshots)
                    .chain()
                    .after(ClientSystems::Receive),
            )
            // Freeze one remote-render time before prediction/replay. Every
            // fixed step and every Update presentation samples this same time.
            .add_systems(
                RunFixedMainLoop,
                update_server_clock.in_set(RunFixedMainLoopSystems::BeforeFixedMainLoop),
            )
            // Same window avian's transform easing uses, so every Update
            // system reads this frame's final remote pose.
            .add_systems(
                RunFixedMainLoop,
                ease_remote_players_from_buffer
                    .in_set(RunFixedMainLoopSystems::AfterFixedMainLoop)
                    .after(TransformEasingSystems::Ease),
            )
            .configure_sets(
                FixedPreUpdate,
                (
                    ClientNetAhoySystems::Reconcile,
                    ClientNetAhoySystems::Predict,
                )
                    .chain(),
            )
            .add_systems(FixedFirst, advance_server_clock_fixed)
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
            .add_systems(
                FixedLast,
                (
                    record_prediction_state,
                    interpolate_remote_players
                        .in_set(ClientNetAhoySystems::SampleRemotes)
                        .before(TransformEasingSystems::UpdateEnd),
                ),
            )
            .add_systems(
                Update,
                (
                    mark_server_truth_ghost,
                    tag_remote_players,
                    cleanup_client_prediction_kcc,
                    cleanup_local_presentation_player,
                    cleanup_remote_player_visuals,
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

/// Visual root for the local player. Reconciliation only snaps the sim and
/// reports each correction on [`PredictionCorrection::pending`]; by default
/// [`update_local_presentation_from_prediction`] drives this entity through
/// the same critically damped error bridge remote visuals use, so the sim
/// inhabits the corrected state immediately while the rendered pose stays
/// continuous. A game that wants its own policy reads (and takes) `pending`
/// right after [`ClientNetAhoySystems::Reconcile`] in `FixedPreUpdate`,
/// before the default consumes it in `Update`.
#[derive(Component)]
pub struct LocalPresentationPlayer {
    pub prediction_entity: Entity,
}

/// Visual entity for a remote player, driven once per fixed tick. The game
/// can use transform interpolation for render-rate smoothing.
#[derive(Component)]
#[require(RemotePlaybackStatus)]
pub struct RemotePlayerVisual {
    pub server_entity: Entity,
    pub player_id: PlayerId,
}

/// Viewer-local state of a remote player's playback cursor. Unlike
/// [`RemoteFlags`], this depends on this client's packet arrival history.
#[derive(Component, Debug, Default, Clone, Copy)]
pub struct RemotePlaybackStatus {
    pub extrapolating: bool,
    pub frozen: bool,
    last_render_ticks: Option<f64>,
}

/// The time-sampled frame the remote visual was drawn at this render frame.
/// Pose-coupled presentation (animation state, squash, bounce FX edges) must
/// read this, not the raw `PlayerSnapshot`: the snapshot is the freshest
/// server state, a full interp delay ahead of what's on screen. Link-status
/// UI (CONNECTION_INTERRUPTED) is the exception — warnings should be fresh.
/// Inserted on the first interpolation sample, so it also gates "has ever
/// been sampled".
#[derive(Component, Debug, Default, Clone, Copy)]
pub struct RemoteSampledFrame(pub RemoteSnapshotSample);

#[derive(Component, Debug)]
pub struct PredictionCorrection {
    /// What reconciliation did to the sim this tick, for the game's
    /// presentation layer to consume (right after
    /// [`ClientNetAhoySystems::Reconcile`], same fixed tick).
    pub pending: PendingCorrection,
    pub last_error: f32,
    pub last_ack_sequence: u32,
    pub last_server_tick: u64,
    pub replayed_commands: usize,
    pub mode: CorrectionMode,
    /// Sticky diagnostics from the most recent REWIND (the ignore path,
    /// ~every clean snapshot, doesn't overwrite them): what the correction
    /// moved, the snap gate it was judged against, and its verdict. For
    /// the on-screen "why did that pop" readout.
    pub last_rewind_mode: CorrectionMode,
    pub last_correction_distance: f32,
    pub last_snap_gate: f32,
    /// Set on [`CorrectionMode::Dropped`]: corrections are parked until the
    /// server's ack reaches this sequence, because everything below it is
    /// stale backlog the server is still draining.
    pub resync_floor: Option<u32>,
}

impl Default for PredictionCorrection {
    fn default() -> Self {
        Self {
            pending: PendingCorrection::None,
            last_error: 0.0,
            last_ack_sequence: 0,
            last_server_tick: 0,
            replayed_commands: 0,
            mode: CorrectionMode::Waiting,
            last_rewind_mode: CorrectionMode::Waiting,
            last_correction_distance: 0.0,
            last_snap_gate: 0.0,
            resync_floor: None,
        }
    }
}

/// A correction reconciliation applied to the sim, published for the game's
/// presentation layer. Deltas are `old sim − new sim` measured across
/// restore + replay: the game adds them to its visual-error state so the
/// rendered pose stays continuous while the sim inhabits the corrected
/// state immediately. Whether a big correction is hidden or shown as a pop
/// is a PERCEPTUAL call, so reconciliation only reports the gate it computed
/// (`snap_gate`, see [`SNAP_ERROR_DISTANCE`]/[`SNAP_ERROR_TRAVEL_SECS`]/
/// [`SNAP_WINDOW_FRACTION`]); the default bridge pops past it and folds
/// under it, and a game may substitute its own verdict.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum PendingCorrection {
    #[default]
    None,
    /// Reconcile moved the sim by these deltas; `snap_gate` is the
    /// teleport-class threshold to judge `position.length()` against (or
    /// ignore, and fold everything).
    Fold {
        position: Vec3,
        velocity: Vec3,
        snap_gate: f32,
    },
}

impl PendingCorrection {
    /// Stack a correction onto an unconsumed one: folds sum.
    pub fn merge(&mut self, next: PendingCorrection) {
        *self = match (*self, next) {
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

    pub fn take(&mut self) -> PendingCorrection {
        core::mem::take(self)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CorrectionMode {
    Waiting,
    MissingHistory,
    Ignored,
    Replayed,
    Snapped,
    Dropped,
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
    /// Fixed-tick snapshot messages received this window.
    pub snapshot_frames: u32,
    /// Extra messages delivered in the same client frame. Component
    /// replication would expose only the newest one to game systems.
    pub component_coalesced_frames: u32,
    pub max_snapshot_burst: u32,
    /// Fixed endpoint intervals measured in server ticks. Avian assumes 1.0.
    pub endpoint_intervals: u32,
    pub endpoint_duplicate_times: u32,
    pub endpoint_min_delta_ticks: f32,
    pub endpoint_max_delta_ticks: f32,
    /// Largest jump of the newest observed server tick in one message batch:
    /// the snapshot arrival cadence in ticks (1 = every tick, 4 = batchy).
    pub head_jump_max: u32,
}

impl RemoteClockStats {
    pub fn any(self) -> bool {
        self.snaps > 0
            || self.extrapolated_frames > 0
            || self.capped_frames > 0
            || self.snapshot_frames > 0
            || self.endpoint_intervals > 0
    }
}

#[derive(Resource, Debug)]
pub struct ClientServerClock {
    pub latest_server_tick: u64,
    pub tick_hz: f64,
    pub interpolation_delay_seconds: f64,
    pub stats: RemoteClockStats,
    render_server_ticks: f64,
    /// Ticks the cursor moved this fixed step (the slew rate, or the snap
    /// distance). Sub-tick render sampling scales the overstep fraction by
    /// this so render time is continuous through slewed ticks.
    last_advance_ticks: f64,
    /// Consecutive ticks the cursor has been ahead of the delayed head;
    /// gates the down-slew so bursty snapshot arrival can't trigger it.
    ahead_ticks_streak: u32,
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
            render_server_ticks: 0.0,
            last_advance_ticks: 1.0,
            ahead_ticks_streak: 0,
            initialized: false,
        }
    }
}

impl ClientServerClock {
    pub fn observe_server_tick(&mut self, server_tick: u64) {
        // One replicated snapshot exists per player. Several may report the
        // same server tick in one frame; discipline the clock only once.
        if server_tick == 0 || server_tick <= self.latest_server_tick {
            return;
        }

        if self.initialized {
            let jump = server_tick.saturating_sub(self.latest_server_tick) as u32;
            self.stats.head_jump_max = self.stats.head_jump_max.max(jump);
        }
        self.latest_server_tick = server_tick;
        if !self.initialized {
            self.render_server_ticks = self.latest_renderable_server_ticks();
            self.initialized = true;
        }
    }

    pub fn advance_fixed(&mut self) {
        if !self.initialized {
            return;
        }

        let delayed_head = self.latest_renderable_server_ticks();
        let lag = delayed_head - self.render_server_ticks;
        if lag.abs() > REMOTE_CLOCK_RESET_TICKS {
            self.render_server_ticks = delayed_head;
            self.last_advance_ticks = 1.0;
            self.stats.snaps += 1;
        } else {
            // Symmetric slew, but the down direction needs persistence:
            // instantaneous negative lag is routine when snapshots arrive in
            // bursts (the head goes stale between batches), and slowing on
            // every dip plays remotes in slow motion. A genuinely-ahead
            // cursor (frame hitch ran catch-up fixed steps with no new
            // snapshots) stays ahead EVERY tick, so require a full second of
            // it. Without the down direction at all the cursor would peg at
            // the extrapolation cap forever — nothing else pulls it back.
            if lag < -REMOTE_CLOCK_SLEW_TICKS {
                self.ahead_ticks_streak += 1;
            } else {
                self.ahead_ticks_streak = 0;
            }
            let ahead_persistent = f64::from(self.ahead_ticks_streak) >= self.tick_hz;
            let rate = if lag > REMOTE_CLOCK_FAST_SLEW_TICKS {
                REMOTE_CLOCK_FAST_SLEW_RATE
            } else if lag > REMOTE_CLOCK_SLEW_TICKS {
                REMOTE_CLOCK_SLEW_RATE
            } else if ahead_persistent && lag < -REMOTE_CLOCK_FAST_SLEW_TICKS {
                2.0 - REMOTE_CLOCK_FAST_SLEW_RATE
            } else if ahead_persistent && lag < -REMOTE_CLOCK_SLEW_TICKS {
                2.0 - REMOTE_CLOCK_SLEW_RATE
            } else {
                1.0
            };
            let extrapolation_ticks = f64::from(REMOTE_EXTRAPOLATION_SECONDS) * self.tick_hz;
            let next = (self.render_server_ticks + rate)
                .min(self.latest_server_tick as f64 + extrapolation_ticks);
            self.last_advance_ticks = next - self.render_server_ticks;
            self.render_server_ticks = next;
        }

        let newest = self.latest_server_tick as f64;
        let max_render = newest + f64::from(REMOTE_EXTRAPOLATION_SECONDS) * self.tick_hz;
        // Same window-start convention as RemotePlaybackStatus: count a tick
        // as extrapolated when the pose actually RENDERED is past the head,
        // not when the cursor is about to be.
        let render_window_start = self.render_server_ticks - self.last_advance_ticks;
        if render_window_start >= max_render {
            self.stats.capped_frames += 1;
        }
        if render_window_start > newest {
            self.stats.extrapolated_frames += 1;
        }
    }

    pub fn target_time(&self) -> Option<RemoteRenderTime> {
        self.initialized
            .then(|| RemoteRenderTime::from_ticks_f64(self.render_server_ticks))
    }

    /// The sub-tick render-frame sampling time, reaching the frozen per-tick
    /// target exactly at the end of the tick (overstep 1.0) — one cursor-step
    /// BEHIND it before that, like transform easing between the last two tick
    /// poses. Sampling ahead of the target instead routinely passes the
    /// newest snapshot into extrapolation and pops on every correction, and
    /// renders remotes ahead of the `seen` time lag compensation uses.
    /// Scaled by the cursor's actual step so slewed ticks stay continuous.
    pub fn render_time(&self, overstep_fraction: f32) -> Option<RemoteRenderTime> {
        self.initialized.then(|| {
            RemoteRenderTime::from_ticks_f64(
                self.render_server_ticks
                    + (f64::from(overstep_fraction) - 1.0) * self.last_advance_ticks,
            )
        })
    }

    pub fn target_tick(&self) -> u64 {
        self.target_time().unwrap_or_default().tick
    }

    pub fn target_alpha(&self) -> f32 {
        self.target_time().unwrap_or_default().alpha
    }

    fn latest_renderable_server_ticks(&self) -> f64 {
        (self.latest_server_tick as f64 - self.interpolation_delay_seconds * self.tick_hz).max(0.0)
    }
}

fn advance_server_clock_fixed(mut clock: ResMut<ClientServerClock>) {
    clock.advance_fixed();
}

fn reset_server_clock(mut clock: ResMut<ClientServerClock>) {
    *clock = ClientServerClock::default();
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
    mut frames: MessageReader<RemoteSnapshotFrame>,
    snapshots: Query<&PlayerSnapshot, Changed<PlayerSnapshot>>,
    mut window: Local<f32>,
) {
    let mut frame_count = 0;
    for frame in frames.read() {
        clock.observe_server_tick(frame.server_tick);
        frame_count += 1;
    }
    if frame_count != 0 {
        clock.stats.snapshot_frames += frame_count;
        clock.stats.component_coalesced_frames += frame_count.saturating_sub(1);
        clock.stats.max_snapshot_burst = clock.stats.max_snapshot_burst.max(frame_count);
    }
    // Component fallback keeps demos and initial replication usable.
    for snapshot in &snapshots {
        clock.observe_server_tick(snapshot.server_tick);
    }
    // Attribute wobble reports to real events instead of vibes.
    *window += time.delta_secs();
    if *window >= 30.0 {
        let stats = clock.stats;
        if stats.any() {
            info!(
                "remote clock, last {:.0}s: {} snapshot ticks ({} preserved from component coalescing, max burst {}); {} extrapolated frames ({} capped), {} resets; fixed endpoint dt {:.3}..{:.3} ticks ({} duplicate, {} intervals; Avian expects 1.000)",
                *window,
                stats.snapshot_frames,
                stats.component_coalesced_frames,
                stats.max_snapshot_burst,
                stats.extrapolated_frames,
                stats.capped_frames,
                stats.snaps,
                stats.endpoint_min_delta_ticks,
                stats.endpoint_max_delta_ticks,
                stats.endpoint_duplicate_times,
                stats.endpoint_intervals,
            );
        }
        clock.stats = RemoteClockStats::default();
        *window = 0.0;
    }
}

fn buffer_remote_snapshot_frames(
    mut frames: MessageReader<RemoteSnapshotFrame>,
    clock: Res<ClientServerClock>,
    mut history: ResMut<LagCompensationHistory>,
    mut remotes: Query<(&PlayerId, &mut RemoteInterpolationBuffer)>,
) {
    for frame in frames.read() {
        for (player_id, sample) in &frame.samples {
            let Some((_, mut buffer)) = remotes
                .iter_mut()
                .find(|(remote_id, _)| **remote_id == *player_id)
            else {
                continue;
            };
            log_remote_revision(*player_id, &buffer, *sample, clock.target_time());
            buffer.push(*sample);
            history.record(*player_id, *sample);
        }
    }
}

fn buffer_remote_snapshots(
    mut history: ResMut<LagCompensationHistory>,
    clock: Res<ClientServerClock>,
    mut remotes: Query<
        (&PlayerId, &PlayerSnapshot, &mut RemoteInterpolationBuffer),
        Changed<PlayerSnapshot>,
    >,
) {
    for (player_id, snapshot, mut buffer) in &mut remotes {
        if snapshot.server_tick != 0 {
            log_remote_revision(*player_id, &buffer, snapshot.0, clock.target_time());
            buffer.push(snapshot.0);
            history.record(*player_id, snapshot.0);
        }
    }
}

fn log_remote_revision(
    player: PlayerId,
    buffer: &RemoteInterpolationBuffer,
    replacement: RemoteSnapshotSample,
    render_time: Option<RemoteRenderTime>,
) {
    let Some(old) = buffer
        .samples
        .iter()
        .find(|sample| sample.server_tick == replacement.server_tick)
    else {
        return;
    };
    let position_error = old.position.distance(replacement.position);
    let velocity_error = old.velocity.distance(replacement.velocity);
    if position_error < 0.001 && velocity_error < 0.01 && old.flags == replacement.flags {
        return;
    }
    let ahead = render_time.map_or(0.0, |time| {
        replacement.server_tick as f64 - time.as_ticks_f64()
    });
    info!(
        "remote {} revised tick {} ({:+.3} ticks from playback): pos {:.3}m vel {:.3}m/s flags {:?}->{:?}",
        player.0,
        replacement.server_tick,
        ahead,
        position_error,
        velocity_error,
        old.flags,
        replacement.flags,
    );
}

fn interpolate_remote_players(
    mut commands: Commands,
    mut clock: ResMut<ClientServerClock>,
    remotes: Query<&RemoteInterpolationBuffer>,
    mut visuals: Query<(
        Entity,
        &RemotePlayerVisual,
        &mut Transform,
        Option<&mut LinearVelocity>,
        Option<&mut bevy_ahoy::CharacterLook>,
        Option<&mut RemoteSampledFrame>,
        &mut RemotePlaybackStatus,
    )>,
) {
    let Some(render_time) = clock.target_time() else {
        return;
    };

    for (entity, visual, mut transform, velocity, look, frame, mut status) in &mut visuals {
        let Ok(buffer) = remotes.get(visual.server_entity) else {
            continue;
        };
        let Some(sample) = buffer.sample(render_time) else {
            continue;
        };
        let render_ticks = render_time.as_ticks_f64();
        if let Some(previous) = status.last_render_ticks {
            let delta = (render_ticks - previous).max(0.0) as f32;
            let stats = &mut clock.stats;
            if stats.endpoint_intervals == 0 {
                stats.endpoint_min_delta_ticks = delta;
                stats.endpoint_max_delta_ticks = delta;
            } else {
                stats.endpoint_min_delta_ticks = stats.endpoint_min_delta_ticks.min(delta);
                stats.endpoint_max_delta_ticks = stats.endpoint_max_delta_ticks.max(delta);
            }
            stats.endpoint_intervals += 1;
            stats.endpoint_duplicate_times += u32::from(delta <= 1e-6);
        }
        status.last_render_ticks = Some(render_ticks);
        let newest_tick = buffer.samples.back().map_or(0, |sample| sample.server_tick);
        // Judge extrapolation at the START of this tick's render window
        // (render-rate sampling spans [cursor - advance, cursor]): the flag
        // means "the pose on screen is dead-reckoned", not "the cursor is
        // about to pass the head". Judged at the cursor it flips a full tick
        // early and flickers EXTRAP while rendering is still interpolating.
        let ahead_ticks = (render_time.as_ticks_f64()
            - clock.last_advance_ticks
            - newest_tick as f64)
            .max(0.0);
        status.extrapolating = ahead_ticks > 0.0;
        status.frozen = ahead_ticks / FIXED_TIMESTEP_HZ
            >= f64::from(REMOTE_EXTRAPOLATION_SECONDS) - 1e-6;
        transform.translation = sample.position;
        if let Some(mut velocity) = velocity {
            velocity.0 = sample.velocity;
        }
        if let Some(mut look) = look {
            look.yaw = sample.look.x;
            look.pitch = sample.look.y;
        }
        if let Some(mut frame) = frame {
            frame.0 = sample;
        } else {
            commands.entity(entity).insert(RemoteSampledFrame(sample));
        }
    }
}

/// Peak correction acceleration the error bridges aim for; sizes the spring
/// stiffness per fold. Shared by the local player's presentation bridge and
/// every remote visual's bridge.
pub const BRIDGE_MAX_ACCEL: f32 = 40.0;
/// Settle-window clamp for the bridges (95% converged ≈ 4.7/ω).
pub const BRIDGE_SETTLE_MIN_SECS: f32 = 0.08;
pub const BRIDGE_SETTLE_MAX_SECS: f32 = 0.35;
/// A render-time jump past this many ticks in one frame is a cursor resync,
/// not frame pacing; the bridge folds the traversal instead of fast-forwarding.
const REMOTE_BRIDGE_TIME_JUMP_TICKS: f64 = 3.0;

/// Critically damped second-order error bridge: `rendered = truth + offset`,
/// offset decaying along x(t) = (A + Bt)·e^(−ωt) (exact step,
/// framerate-independent). Folding a discontinuity preserves both offset
/// and its velocity, so the rendered path stays C1 through every
/// correction. One spring serves the local player's corrections and remote
/// visuals' revisions alike.
#[derive(Default, Clone, Copy)]
pub struct ErrorBridge {
    pub offset: Vec3,
    pub velocity: Vec3,
    omega: f32,
}

/// [`ErrorBridge`] plus the previous frame's sample, so a remote visual can
/// isolate what changed under it.
#[derive(Default, Clone, Copy)]
struct RemoteBridge {
    spring: ErrorBridge,
    /// Previous frame's (render ticks, sampled position, sampled velocity).
    prev: Option<(f64, Vec3, Vec3)>,
}

impl ErrorBridge {
    pub fn advance(&mut self, dt: f32) {
        if self.offset.length_squared() < 1e-6 && self.velocity.length_squared() < 1e-4 {
            self.offset = Vec3::ZERO;
            self.velocity = Vec3::ZERO;
            return;
        }
        let a = self.offset;
        let b = self.velocity + self.omega * self.offset;
        let decay = (-self.omega * dt).exp();
        self.offset = (a + b * dt) * decay;
        self.velocity = (b - self.omega * (a + b * dt)) * decay;
    }

    /// Fold an error delta in, re-sizing ω from the comfort accel so big
    /// corrections take longer instead of yanking: peak accel is ω²·e₀
    /// (position term) and 2ω·v₀ (velocity term).
    pub fn fold(&mut self, de: Vec3, dv: Vec3) {
        self.offset += de;
        self.velocity += dv;
        let e0 = self.offset.length().max(1e-5);
        let v0 = self.velocity.length().max(1e-4);
        let omega_pos = (BRIDGE_MAX_ACCEL / e0).sqrt();
        let omega_vel = BRIDGE_MAX_ACCEL / (2.0 * v0);
        self.omega = omega_pos
            .min(omega_vel)
            .clamp(4.7 / BRIDGE_SETTLE_MAX_SECS, 4.7 / BRIDGE_SETTLE_MIN_SECS);
    }
}

/// Default local-player presentation: the visual follows the prediction KCC
/// through an [`ErrorBridge`]. Each reconciliation's deltas
/// ([`PendingCorrection::Fold`]) fold into the spring so the sim jumps to the
/// corrected state while the rendered pose eases there; a correction past
/// its `snap_gate` is teleport-class and pops instead (masking metres as a
/// swoosh reads worse than the pop).
pub fn update_local_presentation_from_prediction(
    time: Res<Time>,
    mut predictions: Query<
        (&Transform, &mut PredictionCorrection),
        (With<ClientPredictionKcc>, Without<LocalPresentationPlayer>),
    >,
    mut presentations: Query<
        (Entity, &LocalPresentationPlayer, &mut Transform),
        (Without<ClientPredictionKcc>, Without<ServerTruthGhost>),
    >,
    mut bridges: Local<bevy::ecs::entity::EntityHashMap<ErrorBridge>>,
) {
    let mut live = bevy::ecs::entity::EntityHashSet::default();
    for (entity, presentation, mut presentation_transform) in &mut presentations {
        live.insert(entity);
        let Ok((prediction_transform, mut correction)) =
            predictions.get_mut(presentation.prediction_entity)
        else {
            continue;
        };
        let bridge = bridges.entry(entity).or_default();
        if let PendingCorrection::Fold {
            position,
            velocity,
            snap_gate,
        } = correction.pending.take()
        {
            if position.length() >= snap_gate {
                *bridge = ErrorBridge::default();
            } else {
                bridge.fold(position, velocity);
            }
        }
        bridge.advance(time.delta_secs());
        presentation_transform.translation = prediction_transform.translation + bridge.offset;
        // Rotation is owned by the game's animation layer, which faces the
        // model toward the run direction (CharacterLook.yaw). The KCC
        // transform stays at identity, so syncing it here would stomp that
        // facing every frame.
    }
    bridges.retain(|entity, _| live.contains(entity));
}

/// Render-rate remote translation: sample the snapshot buffer directly at the
/// sub-tick render time every frame. This replaces transform easing
/// (avian's `TranslationInterpolation`/`TranslationHermiteEasing`) for remote
/// visuals: easing between per-tick freezes carries state that ANY external
/// `Transform` write (knockback spin, cosmetic rotation) resets as a
/// "teleport", freezing translation for the rest of the tick — the remote
/// judder this replaces. The buffer's own Hermite between snapshot endpoints
/// is the interpolation source, so continuity holds by construction and there
/// is no state to wipe. `interpolate_remote_players` (FixedLast) remains the
/// per-tick pose for fixed-schedule consumers; this refines translation only.
///
/// On top of the raw sample rides the [`RemoteBridge`]: re-sampling the
/// CURRENT buffer at the PREVIOUS frame's render time isolates exactly what
/// changed out from under the viewer (revisions, extrapolation recovery) —
/// unchanged data reproduces last frame's target bit-for-bit, so genuine
/// motion (landings, redirects) never enters the spring and stays crisp.
/// Real teleports (respawns) still snap: masking 8 m as a swoosh reads worse.
fn ease_remote_players_from_buffer(
    time: Res<Time>,
    fixed_time: Res<Time<Fixed>>,
    clock: Res<ClientServerClock>,
    remotes: Query<&RemoteInterpolationBuffer>,
    mut visuals: Query<(
        Entity,
        &RemotePlayerVisual,
        &mut Transform,
        Option<&mut bevy_ahoy::CharacterLook>,
    )>,
    mut bridges: Local<bevy::ecs::entity::EntityHashMap<RemoteBridge>>,
) {
    let Some(render_time) = clock.render_time(fixed_time.overstep_fraction()) else {
        return;
    };
    let render_ticks = render_time.as_ticks_f64();

    let mut live = bevy::ecs::entity::EntityHashSet::default();
    for (entity, visual, mut transform, look) in &mut visuals {
        live.insert(entity);
        let Ok(buffer) = remotes.get(visual.server_entity) else {
            continue;
        };
        let Some(sample) = buffer.sample(render_time) else {
            continue;
        };
        // Render-rate look too: the sampler lerps yaw wrap-aware between
        // snapshot endpoints, so remote facing turns smoothly instead of
        // stepping at tick rate.
        if let Some(mut look) = look {
            look.yaw = sample.look.x;
            look.pitch = sample.look.y;
        }

        let bridge = bridges.entry(entity).or_default();
        if let Some((prev_ticks, prev_pos, prev_vel)) = bridge.prev {
            // What the previous frame's pose looks like on TODAY's data.
            let revised = buffer
                .sample(RemoteRenderTime::from_ticks_f64(prev_ticks))
                .unwrap_or(sample);
            let mut de = prev_pos - revised.position;
            let mut dv = prev_vel - revised.velocity;
            // Cursor resync: the render clock skipped, don't fast-forward
            // the whole skipped path in one frame — hold and spring along it.
            if (render_ticks - prev_ticks).abs() > REMOTE_BRIDGE_TIME_JUMP_TICKS {
                de = prev_pos - sample.position;
                dv = prev_vel - sample.velocity;
            }
            if de.length() > REMOTE_INTERPOLATION_TELEPORT_DISTANCE {
                *bridge = RemoteBridge::default();
            } else if de.length_squared() > 1e-10 {
                bridge.spring.fold(de, dv);
            }
        }
        bridge.spring.advance(time.delta_secs());
        bridge.prev = Some((render_ticks, sample.position, sample.velocity));
        transform.translation = sample.position + bridge.spring.offset;
    }
    bridges.retain(|entity, _| live.contains(entity));
}

#[allow(clippy::too_many_arguments)]
fn drive_prediction_and_send_input(
    mut commands: Commands,
    mut input: ResMut<ClientInput>,
    clock: Res<ClientServerClock>,
    mut input_state: ResMut<ClientInputState>,
    mut command_history: ResMut<LocalCommandHistory>,
    profile: Res<NetAhoyLinkProfile>,
    predictions: Query<(Entity, &PredictionCorrection), With<ClientPredictionKcc>>,
    mut stepper: NetAhoyStepper,
) {
    let Ok((predicted_entity, correction)) = predictions.single() else {
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

    // Every still-unacked command, newest-last, capped by the transport
    // profile. On the reliable transport the cap is 1 and this degenerates
    // to "send the newest"; on datagrams the tail is what heals holes.
    let mut unacked = command_history.after_sequence(correction.last_ack_sequence);
    let skip = unacked.len().saturating_sub(profile.usercmd_backup_cap);
    commands.client_trigger(AhoyUserCmdPacket {
        commands: unacked.split_off(skip),
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

/// Field-wise diff of two carry states for the mispredict log; empty when
/// they agree to epsilon. The carry rides no reconciliation gate, so this
/// line is the only place a diverged event stamp (`last_bounce`, `last_jump`
/// a tick apart = the event fired on one peer only) or pre-projection
/// velocity ever becomes visible — the usual author
/// of a large `vel` error with `state_mismatch false`.
fn carry_diff(cli: &NetAhoyCarryState, srv: &NetAhoyCarryState) -> String {
    // One-tick disagreement is 0.05 s; ulp/f32-roundtrip drift is orders
    // below 0.02, same rationale as NetAhoyMoveState::agrees_with.
    const EPS: f32 = 0.02;
    let mut parts: Vec<String> = Vec::new();
    macro_rules! scalar {
        ($f:ident) => {
            if (cli.$f - srv.$f).abs() > EPS {
                parts.push(format!(
                    "{} cli {:.3} srv {:.3}",
                    stringify!($f),
                    cli.$f,
                    srv.$f
                ));
            }
        };
    }
    macro_rules! vector {
        ($f:ident) => {
            if !cli.$f.abs_diff_eq(srv.$f, EPS) {
                parts.push(format!(
                    "{} cli {:.3?} srv {:.3?}",
                    stringify!($f),
                    cli.$f,
                    srv.$f
                ));
            }
        };
    }
    if cli.sliding != srv.sliding {
        parts.push(format!("sliding cli {} srv {}", cli.sliding, srv.sliding));
    }
    scalar!(tac_velocity);
    scalar!(land_speed);
    vector!(last_velocity);
    vector!(platform_velocity);
    vector!(platform_angular_velocity);
    scalar!(last_ground);
    scalar!(last_land);
    scalar!(last_slide);
    scalar!(last_bounce);
    scalar!(last_jump);
    scalar!(last_tac);
    scalar!(last_step_up);
    scalar!(last_step_down);
    parts.join(" | ")
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

    // The server advanced this head with guessed input and may still replace
    // its historical ticks. It is useful remote presentation data, but not an
    // authoritative verdict against the owner's real command history yet.
    if snapshot.provisional {
        correction.mode = CorrectionMode::Waiting;
        correction.last_server_tick = snapshot.server_tick;
        correction.last_error = 0.0;
        correction.replayed_commands = 0;
        return;
    }

    if snapshot.last_processed_sequence == 0 {
        correction.mode = CorrectionMode::Waiting;
        correction.last_server_tick = snapshot.server_tick;
        correction.last_error = 0.0;
        correction.replayed_commands = 0;
        return;
    }

    // Post-Dropped resync: the server is still draining commands from before
    // the drop, so its acks land in that stale window and correcting against
    // them is meaningless — it was the MissingHistory rollback storm. The
    // server replays the same commands we already predicted, so truth
    // converges to us on its own; park until the ack reaches the floor.
    if let Some(floor) = correction.resync_floor {
        if sequence_is_newer(floor, snapshot.last_processed_sequence) {
            correction.mode = CorrectionMode::Waiting;
            correction.last_server_tick = snapshot.server_tick;
            correction.last_ack_sequence = snapshot.last_processed_sequence;
            return;
        }
        correction.resync_floor = None;
    }

    // A repeated ack means the server advanced by extrapolating our previous
    // intent, not by consuming another real command. That snapshot is not a
    // verdict on any newer prediction-history frame; wait for a real ack.
    if snapshot.last_processed_sequence == correction.last_ack_sequence {
        correction.mode = CorrectionMode::Waiting;
        correction.last_server_tick = snapshot.server_tick;
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
        let state_mismatch = !snapshot.state.agrees_with(&ack_frame.state)
            || snapshot.player_state != ack_frame.player_state;
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

    // Divergence logging: we are taking a real correction. Print what we
    // disagreed about and what every moving snapshot body looked like to US
    // at the acked command's view time — diff against the server's
    // "srv body cmd N" lines for the same sequence.
    match (&ack_frame, history_error) {
        (Some(frame), Some((total_error, state_mismatch, xz_error, y_error, _))) => {
            // `vel` is the velocity disagreement — it is in NO gate, so a
            // discrete redirect (bounce, wall jump) diverging on one peer
            // shows up as a large vel with state_mismatch false, a tick
            // before it integrates into position error.
            debug!(
                "mispredict ack {} err {:.3} (xz {:.3} y {:.3} vel {:.3} state_mismatch {}) seen {}:{:.3}",
                snapshot.last_processed_sequence,
                total_error,
                xz_error,
                y_error,
                (frame.velocity - snapshot.velocity).length(),
                state_mismatch,
                frame.command.seen_server_tick,
                frame.command.seen_alpha
            );
            debug!(
                "  pose: cli {:.6?} srv {:.6?} delta(srv-cli) {:.6?} | cmd move {:.3?} look {:.3?} buttons {:?} srv_buttons {:?}",
                frame.position,
                snapshot.position,
                snapshot.position - frame.position,
                frame.command.movement,
                frame.command.look,
                frame.command.buttons,
                snapshot.last_processed_buttons,
            );
            // WHICH state forced the rewind: the KCC move state or the
            // player-think POD (rockets/weapon).
            if !snapshot.state.agrees_with(&frame.state) {
                debug!("  move state cli {:?} vs srv {:?}", frame.state, snapshot.state);
            }
            // Platform carry on both sides (rides NetAhoyCarryState) plus
            // the ground each peer stood on: a constant xz error with
            // state_mismatch false is a velocity term outside the reconciled
            // velocity, and this is the first place to look for it.
            debug!(
                "  carry: platform_velocity cli {:.3?} srv {:.3?} | velocity cli {:.3?} srv {:.3?} | ground cli {:?} srv_grounded {}",
                frame.controller_state.platform_velocity,
                snapshot.carry.platform_velocity,
                frame.velocity,
                snapshot.velocity,
                frame.controller_state.grounded.as_ref().map(|g| g.entity),
                snapshot.state.grounded
            );
            // The reconciliation-only carry (event stopwatches + the
            // pre-projection velocities) is in NO gate: a bounce or wall
            // jump that fired on one peer only diverges an event stamp here
            // before any gate trips. Print only the fields that disagree.
            let carry_diff = carry_diff(
                &NetAhoyCarryState::from_controller_state(&frame.controller_state),
                &snapshot.carry,
            );
            if !carry_diff.is_empty() {
                debug!("  carry diff: {carry_diff}");
            }
            if snapshot.player_state != frame.player_state {
                let cli = &frame.player_state;
                let srv = &snapshot.player_state;
                debug!(
                    "  player_state diff: rockets {} weapon {}",
                    cli.rockets != srv.rockets,
                    cli.weapon != srv.weapon,
                );
            }
        }
        _ => debug!(
            "mispredict ack {}: no local frame for that sequence (MissingHistory rewind)",
            snapshot.last_processed_sequence
        ),
    }

    let current_position = stepper
        .position(predicted_entity)
        .unwrap_or(snapshot.position);
    let current_velocity = stepper
        .velocity(predicted_entity)
        .unwrap_or(snapshot.velocity);
    let replay_commands = command_history.after_sequence(snapshot.last_processed_sequence);

    // Death-spiral bound: a runaway window means acknowledgements stopped.
    // Snapping and clearing here manufactured
    // a MissingHistory storm — the server kept acking into the cleared range
    // for the whole drain. Instead keep predicting and park corrections until
    // the ack catches up to the newest command we've sent.
    if replay_commands.len() > MAX_REPLAY_COMMANDS {
        let floor = command_history
            .commands
            .back()
            .map(|command| command.sequence);
        info!(
            "Dropped: ack {} lags {} cmds (> {MAX_REPLAY_COMMANDS}), parking corrections until ack {:?}",
            snapshot.last_processed_sequence,
            replay_commands.len(),
            floor,
        );
        correction.resync_floor = floor;
        correction.mode = CorrectionMode::Dropped;
        correction.last_server_tick = snapshot.server_tick;
        correction.last_ack_sequence = snapshot.last_processed_sequence;
        correction.last_error = history_error
            .map(|(total_error, _, _, _, _)| total_error)
            .unwrap_or(0.0);
        correction.replayed_commands = 0;
        return;
    }

    let local_state = ack_frame
        .as_ref()
        .map(|ack_frame| (&ack_frame.controller_state, &ack_frame.accumulated_input));

    stepper.restore(predicted_entity, snapshot, local_state);
    let restored_position = stepper
        .position(predicted_entity)
        .unwrap_or(snapshot.position);

    // A starving server repeats the same ack every snapshot, and retain_after
    // deliberately keeps the frame AT the ack for that re-compare. When that
    // frame is gone (history eviction, a capture gap), nothing below
    // recreates it — replay only captures frames AFTER the ack — so every
    // repeat forced this whole degraded rewind again. Bank server truth at
    // the ack so the next repeat can take the ignore path.
    if ack_frame.is_none() {
        let baseline = AhoyUserCmd {
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
            .unwrap_or(snapshot.position);
        if let Err(err) = stepper.player_move(
            predicted_entity,
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
            .unwrap_or(snapshot.position);
        let velocity = stepper
            .velocity(predicted_entity)
            .unwrap_or(snapshot.velocity);
        trace!(
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
        .unwrap_or(snapshot.position);
    let new_velocity = stepper
        .velocity(predicted_entity)
        .unwrap_or(snapshot.velocity);
    let correction_distance = current_position.distance(new_position);
    // Teleport-class is a perceptual call, not an absolute one: dragging the
    // visual across 3 m mid-bhop at 30 m/s is ~100 ms of ground it was
    // covering anyway and hides fine; the same 3 m standing still is a
    // teleport. Gate on travel time at the current speed AND on the ground
    // the replay window covered (see SNAP_WINDOW_FRACTION), floored by the
    // walking-speed constant — otherwise high-speed corrections take the
    // no-smoothing pop exactly where corrections are biggest.
    let window_travel = snapshot.position.distance(new_position);
    let snap_distance = SNAP_ERROR_DISTANCE
        .max(current_velocity.length().max(new_velocity.length()) * SNAP_ERROR_TRAVEL_SECS)
        .max(window_travel * SNAP_WINDOW_FRACTION);

    correction.mode = if ack_frame.is_none() {
        CorrectionMode::MissingHistory
    } else if correction_distance >= snap_distance {
        CorrectionMode::Snapped
    } else {
        CorrectionMode::Replayed
    };
    // Only the rewind path logs. The ignore path above is the common case
    // (every snapshot, ~20 Hz) and would drown this out.
    let (xz_error, y_error, state_mismatch) = history_error
        .map(|(_, state_mismatch, xz_error, y_error, _)| (xz_error, y_error, state_mismatch))
        .unwrap_or_default();
    debug!(
        "{:?}: ack {}, replayed {} cmds, xz {:.3}m, y {:.3}m, state_mismatch {}, pose pre {:.6?} restore {:.6?} post {:.6?}, moved {:.3}m, gate {:.3}m",
        correction.mode,
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
    correction.last_rewind_mode = correction.mode;
    correction.last_correction_distance = correction_distance;
    correction.last_snap_gate = snap_distance;
    correction.last_server_tick = snapshot.server_tick;
    correction.last_ack_sequence = snapshot.last_processed_sequence;
    correction.last_error = history_error
        .map(|(total_error, _, _, _, _)| total_error)
        .unwrap_or(correction_distance);
    correction.replayed_commands = replayed;
    // Always report the deltas — even over-gate. The sim is already
    // corrected either way; pop-vs-hide is the game's perceptual policy.
    correction.pending.merge(PendingCorrection::Fold {
        position: current_position - new_position,
        velocity: current_velocity - new_velocity,
        snap_gate: snap_distance,
    });
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
    fn fixed_clock_advances_exactly_one_tick() {
        let mut clock = clock_at(100);
        let start = clock.target_time().unwrap().as_ticks_f64();
        clock.observe_server_tick(101);
        clock.advance_fixed();
        let end = clock.target_time().unwrap().as_ticks_f64();
        assert_eq!(end - start, 1.0);
    }

    #[test]
    fn clock_extrapolation_is_capped() {
        let mut clock = clock_at(100);
        for _ in 0..20 {
            clock.advance_fixed();
        }
        let max = 100.0 + f64::from(REMOTE_EXTRAPOLATION_SECONDS) * FIXED_TIMESTEP_HZ;
        let end = clock.target_time().unwrap().as_ticks_f64();
        assert!(end <= max + 1e-6, "clock overran the cap: {end} > {max}");
        assert!(clock.stats.capped_frames > 0);
    }

    #[test]
    fn clock_snaps_after_long_outage() {
        let mut clock = clock_at(100);
        for _ in 0..20 {
            clock.advance_fixed();
        }
        clock.observe_server_tick(140);
        clock.advance_fixed();
        assert_eq!(clock.stats.snaps, 1);
        let expected = 140.0 - REMOTE_INTERPOLATION_DELAY_TICKS as f64;
        let end = clock.target_time().unwrap().as_ticks_f64();
        assert!(
            (end - expected).abs() < 0.5,
            "clock should snap to {expected}, got {end}"
        );
    }

    #[test]
    fn fixed_clock_holds_its_two_tick_delay() {
        let mut clock = clock_at(100);
        for server_tick in 101..=1_000 {
            clock.observe_server_tick(server_tick);
            clock.advance_fixed();
            assert_eq!(
                clock.target_time().unwrap().as_ticks_f64(),
                server_tick as f64 - REMOTE_INTERPOLATION_DELAY_TICKS as f64
            );
        }
    }
}
