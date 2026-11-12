//! Debug tools: read-only readouts of the netcode for a HUD, slow motion and a continuity probe.
//! For bad network conditions, see `bevy_link_conditioner`.
//!
//! - [`PredictionStatus`], on the [`PredictedPlayer`](crate::PredictedPlayer): what the last
//!   reconcile did, and how far the acks trail the input.
//! - [`CommandClock`], a client resource: the input clock's pace, the server's last
//!   [`InputLead`] report and its resyncs.
//! - [`RemoteClock`], a client resource: the remote render clock's snaps and extrapolation.
//! - [`RemotePlaybackStatus`], on each [`RemoteVisual`](crate::RemoteVisual): whether its pose
//!   is dead-reckoned.

use bevy::prelude::*;

pub use crate::client::{
    CommandClock, CommandClockStats, CorrectionMode, PredictionStatus, RemoteClock,
    RemoteClockStats, RemotePlaybackStatus,
};
pub use crate::protocol::InputLead;

/// The slowest [`DebugTimeScale`].
pub const MIN_DEBUG_TIME_SCALE: f32 = 0.01;
/// The fastest [`DebugTimeScale`].
pub const MAX_DEBUG_TIME_SCALE: f32 = 4.0;

/// How fast `Time<Virtual>` runs, for watching the netcode in slow motion.
#[derive(Resource, Clone, Copy, Debug, PartialEq)]
pub struct DebugTimeScale {
    /// Virtual seconds per real second.
    pub factor: f32,
}

impl Default for DebugTimeScale {
    fn default() -> Self {
        Self { factor: 1.0 }
    }
}

impl DebugTimeScale {
    /// `factor`, clamped to [`MIN_DEBUG_TIME_SCALE`]..=[`MAX_DEBUG_TIME_SCALE`].
    pub fn new(factor: f32) -> Self {
        Self {
            factor: factor.clamp(MIN_DEBUG_TIME_SCALE, MAX_DEBUG_TIME_SCALE),
        }
    }

    /// Whether it runs at anything but real time.
    pub fn is_scaled(self) -> bool {
        (self.factor - 1.0).abs() > f32::EPSILON
    }
}

/// Sets `Time<Virtual>` to the [`DebugTimeScale`].
pub fn apply_debug_time_scale(
    config: Res<DebugTimeScale>,
    mut virtual_time: ResMut<Time<Virtual>>,
) {
    virtual_time.set_relative_speed(config.factor);
    if config.is_scaled() {
        info!("debug time scale set to {:.3}x", config.factor);
    }
}

/// Logs velocity jumps in how remote players are drawn: a render-rate C1-continuity probe.
///
/// Samples each remote visual's rendered translation in `Last` and estimates per-frame velocity
/// and acceleration. Smooth motion through the tick-rate Hermite stays under
/// `|dv_tick| / tick_dt`; snaps and extrapolation reversals are steps that read as thousands of
/// m/s^2. Each spike logs at once with clock context; a summary prints every 5 s, so a quiet
/// run still shows the probe ran.
pub struct ContinuityProbePlugin;

/// m/s^2, above what smooth motion reaches: a hard redirect renders as up to ~6*|dv|/tick_dt
/// (2-4k for 20-30 m/s at 20 Hz). Only presentation discontinuities exceed it.
const CONTINUITY_SPIKE_ACCEL: f32 = 5000.0;
/// m/s: a spike must also be a real velocity step, not sub-frame noise.
const CONTINUITY_SPIKE_DV: f32 = 0.5;
const CONTINUITY_SUMMARY_SECONDS: f32 = 5.0;

#[derive(Resource, Default)]
struct ContinuityProbeState {
    tracks: bevy::platform::collections::HashMap<Entity, (Vec3, Vec3)>,
    window_seconds: f32,
    window_frames: u32,
    window_spikes: u32,
    window_max_accel: f32,
}

impl Plugin for ContinuityProbePlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<ContinuityProbeState>()
            .add_systems(Last, probe_remote_continuity);
    }
}

fn probe_remote_continuity(
    time: Res<Time>,
    fixed_time: Res<Time<Fixed>>,
    mut clock: ResMut<RemoteClock>,
    tick_rate: Res<crate::TickRate>,
    remotes: Query<(Entity, &Transform, &RemotePlaybackStatus), With<crate::RemoteVisual>>,
    mut state: ResMut<ContinuityProbeState>,
) {
    let dt = time.delta_secs();
    if dt <= f32::EPSILON {
        return;
    }

    let mut live = bevy::ecs::entity::EntityHashSet::default();
    for (entity, transform, status) in &remotes {
        live.insert(entity);
        let pos = transform.translation;
        let Some((prev_pos, prev_vel)) = state.tracks.get(&entity).copied() else {
            state.tracks.insert(entity, (pos, Vec3::ZERO));
            continue;
        };
        let vel = (pos - prev_pos) / dt;
        let dv = vel - prev_vel;
        let accel = dv.length() / dt;
        state.window_frames += 1;
        state.window_max_accel = state.window_max_accel.max(accel);
        if accel > CONTINUITY_SPIKE_ACCEL && dv.length() > CONTINUITY_SPIKE_DV {
            state.window_spikes += 1;
            info!(
                "continuity spike {entity}: |dv| {:.2} m/s accel {:.0} m/s^2 dt {:.1}ms alpha {:.3} pos {:.3?} extrap {} frozen {} clock lag {:.2}",
                dv.length(),
                accel,
                dt * 1000.0,
                fixed_time.overstep_fraction(),
                pos,
                status.extrapolating,
                status.frozen,
                clock
                    .target_time()
                    .map(|t| {
                        (clock.latest_server_tick as f64
                            - clock.interpolation_delay_seconds * f64::from(tick_rate.hz()))
                            - t.as_ticks_f64()
                    })
                    .unwrap_or(0.0),
            );
        }
        state.tracks.insert(entity, (pos, vel));
    }
    state.tracks.retain(|entity, _| live.contains(entity));

    state.window_seconds += dt;
    if state.window_seconds >= CONTINUITY_SUMMARY_SECONDS && state.window_frames > 0 {
        info!(
            "continuity summary: {} remote-frames, {} spikes, max accel {:.0} m/s^2 over {:.1}s | snaps {} extrap-frames {} head-jump-max {}",
            state.window_frames,
            state.window_spikes,
            state.window_max_accel,
            state.window_seconds,
            clock.stats.snaps,
            clock.stats.extrapolated_frames,
            clock.stats.head_jump_max,
        );
        clock.stats.head_jump_max = 0;
        state.window_seconds = 0.0;
        state.window_frames = 0;
        state.window_spikes = 0;
        state.window_max_accel = 0.0;
    }
}
