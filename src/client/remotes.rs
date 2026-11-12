//! Remote players, drawn from their pose histories.

use avian3d::prelude::LinearVelocity;
use bevy::prelude::*;

use crate::{
    demo::DemoPlayback,
    history::History,
    poses::{
        REMOTE_EXTRAPOLATION_SECONDS, REMOTE_INTERPOLATION_TELEPORT_DISTANCE, RemotePose,
        RemoteRenderTime, pose_at,
    },
    protocol::*,
};

use super::{RemotePlayer, RemoteVisual, input::PresentedFrame, remote_clock::RemoteClock};

/// Viewer-local state of a remote player's playback cursor. Unlike
/// [`RemoteFlags`](crate::RemoteFlags), this depends on this client's packet arrival history.
#[derive(Component, Debug, Default, Clone, Copy)]
pub struct RemotePlaybackStatus {
    pub(crate) extrapolating: bool,
    pub(crate) frozen: bool,
    last_render_ticks: Option<f64>,
}

impl RemotePlaybackStatus {
    /// The pose on screen is dead-reckoned past the newest snapshot.
    pub fn extrapolating(&self) -> bool {
        self.extrapolating
    }

    /// Dead reckoning hit its cap: the player holds still until a snapshot arrives.
    pub fn frozen(&self) -> bool {
        self.frozen
    }
}

/// The pose a [`RemoteVisual`] is drawn at this frame, with the velocity
/// its history gives it there (none rides the wire). Presentation tied to the pose (animation,
/// effects) reads this, not the newest [`PlayerSnapshot`], which is an interpolation delay ahead
/// of the screen; link warnings read the newest. Inserted on the first sample.
#[derive(Component, Debug, Default, Clone, Copy)]
pub struct RemoteSampledFrame(pub RemotePose);

pub(super) fn interpolate_remote_players(
    mut commands: Commands,
    mut clock: ResMut<RemoteClock>,
    tick_rate: Res<TickRate>,
    remotes: Query<&History<PlayerSnapshot>, With<RemotePlayer>>,
    mut visuals: Query<(
        Entity,
        &RemoteVisual,
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
        let Ok(history) = remotes.get(visual.server_entity) else {
            continue;
        };
        let Some(sample) = pose_at(history, render_time, *tick_rate) else {
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
        // Past the history's newest entry is data that has not arrived.
        let newest_tick = history.newest().map_or(0, |(tick, _)| tick);
        // Judged at the start of this tick's render window ([cursor - advance, cursor]): the
        // flag means the pose on screen is dead-reckoned. At the cursor it would flip a tick
        // early, while the screen still interpolates.
        let ahead_ticks =
            (render_time.as_ticks_f64() - clock.last_advance_ticks - newest_tick as f64).max(0.0);
        status.extrapolating = ahead_ticks > 0.0;
        status.frozen =
            ahead_ticks / clock.tick_hz >= f64::from(REMOTE_EXTRAPOLATION_SECONDS) - 1e-6;
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

/// Peak correction acceleration the error bridge aims for (m/s²): sizes the spring per fold.
pub(super) const REMOTE_BRIDGE_MAX_ACCEL: f32 = 40.0;
/// Settle-window clamp for the bridge (95% converged ≈ 4.7/ω).
pub(super) const REMOTE_BRIDGE_SETTLE_MIN_SECS: f32 = 0.08;
pub(super) const REMOTE_BRIDGE_SETTLE_MAX_SECS: f32 = 0.35;
/// A render-time jump past this in one frame (s) is a cursor resync, not
/// frame pacing; the bridge folds the traversal instead of fast-forwarding.
const REMOTE_BRIDGE_TIME_JUMP_SECONDS: f64 = 0.15;

/// Critically damped error bridge for one remote visual: `rendered = true_sample + offset`, the
/// offset decaying along x(t) = (A + Bt)·e^(−ωt) (exact step, framerate-independent). A fold
/// keeps both the offset and its velocity, so the drawn path stays C1 through every correction.
#[derive(Default, Clone, Copy)]
pub(super) struct RemoteBridge {
    offset: Vec3,
    velocity: Vec3,
    omega: f32,
    /// Previous frame's (render ticks, sampled position, sampled velocity).
    prev: Option<(f64, Vec3, Vec3)>,
}

impl RemoteBridge {
    fn advance(&mut self, dt: f32) {
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
    fn fold(&mut self, de: Vec3, dv: Vec3) {
        self.offset += de;
        self.velocity += dv;
        let e0 = self.offset.length().max(1e-5);
        let v0 = self.velocity.length().max(1e-4);
        let omega_pos = (REMOTE_BRIDGE_MAX_ACCEL / e0).sqrt();
        let omega_vel = REMOTE_BRIDGE_MAX_ACCEL / (2.0 * v0);
        self.omega = omega_pos.min(omega_vel).clamp(
            4.7 / REMOTE_BRIDGE_SETTLE_MAX_SECS,
            4.7 / REMOTE_BRIDGE_SETTLE_MIN_SECS,
        );
    }
}

/// Draws each remote at the sub-tick render time every frame, sampled straight from its history.
/// Not transform easing: easing between per-tick poses carries state that any outside
/// `Transform` write resets as a teleport, freezing translation for the rest of the tick. The
/// history's own Hermite is the curve, so there is no state to wipe. `interpolate_remote_players`
/// (`FixedLast`) stays the per-tick pose for fixed-schedule readers.
///
/// A [`RemoteBridge`] rides on the sample: the current history resampled at the previous frame's
/// render time isolates what changed under the viewer (a late snapshot landing where playback
/// extrapolated). Unchanged data reproduces last frame's pose bit for bit, so real motion never
/// enters the spring. Teleports (respawns) still snap.
pub(super) fn ease_remote_players_from_history(
    time: Res<Time>,
    fixed_time: Res<Time<Fixed>>,
    tick_rate: Res<TickRate>,
    mut clock: ResMut<RemoteClock>,
    mut presented: ResMut<PresentedFrame>,
    remotes: Query<&History<PlayerSnapshot>, With<RemotePlayer>>,
    mut visuals: Query<(
        Entity,
        &RemoteVisual,
        &mut Transform,
        Option<&mut bevy_ahoy::CharacterLook>,
        Option<&mut RemoteSampledFrame>,
    )>,
    mut bridges: Local<bevy::ecs::entity::EntityHashMap<RemoteBridge>>,
    playback: Option<Res<DemoPlayback>>,
) {
    // A demo seek is a cut, not a correction: nothing to spring across.
    if playback.is_some_and(|playback| playback.bursting()) {
        bridges.clear();
    }
    presented.remote = clock.render_time(fixed_time.overstep_fraction());
    let Some(render_time) = presented.remote else {
        return;
    };
    let render_ticks = render_time.as_ticks_f64();

    let mut live = bevy::ecs::entity::EntityHashSet::default();
    for (entity, visual, mut transform, look, frame) in &mut visuals {
        live.insert(entity);
        let Ok(history) = remotes.get(visual.server_entity) else {
            continue;
        };
        let Some(sample) = pose_at(history, render_time, *tick_rate) else {
            continue;
        };
        // The look too: the sampler runs yaw (wrap-aware) and pitch on the position's curve, so
        // facing turns smoothly instead of stepping at the tick rate.
        if let Some(mut look) = look {
            look.yaw = sample.look.x;
            look.pitch = sample.look.y;
        }
        // The frame the game reads is this render time's too, not the fixed tick's: it moves
        // with the body every frame, a pure function of the render time (in a demo, of the demo
        // clock), not of where the fixed ticks fall among the frames.
        if let Some(mut frame) = frame {
            frame.0 = sample;
        }

        let bridge = bridges.entry(entity).or_default();
        if let Some((prev_ticks, prev_pos, prev_vel)) = bridge.prev {
            // The previous frame's pose on this frame's data.
            let resampled = pose_at(
                history,
                RemoteRenderTime::from_ticks_f64(prev_ticks),
                *tick_rate,
            )
            .unwrap_or(sample);
            let mut de = prev_pos - resampled.position;
            let mut dv = prev_vel - resampled.velocity;
            // A cursor resync skipped the render clock: spring across the skipped path rather
            // than fast-forward it in one frame.
            if (render_ticks - prev_ticks).abs() > REMOTE_BRIDGE_TIME_JUMP_SECONDS * clock.tick_hz {
                de = prev_pos - sample.position;
                dv = prev_vel - sample.velocity;
            }
            if de.length() > REMOTE_INTERPOLATION_TELEPORT_DISTANCE {
                *bridge = RemoteBridge::default();
            } else if de.length_squared() > 1e-10 {
                bridge.fold(de, dv);
                clock.stats.bridge_folds += 1;
                clock.stats.bridge_folded += de.length();
            }
        }
        bridge.advance(time.delta_secs());
        bridge.prev = Some((render_ticks, sample.position, sample.velocity));
        transform.translation = sample.position + bridge.offset;
    }
    bridges.retain(|entity, _| live.contains(entity));
}
