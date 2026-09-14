//! Interpolation timing and capsule-cast math shared by client interpolation
//! and server lag compensation.

use std::collections::{HashMap, VecDeque};

use avian3d::prelude::{Collider, Position, Rotation};
use bevy::prelude::*;
use serde::{Deserialize, Serialize};

use crate::protocol::{FIXED_TIMESTEP_HZ, NetAhoyMoveState, PlayerId};

pub const LAG_COMPENSATION_HISTORY_CAPACITY: usize = 64;

pub const REMOTE_INTERPOLATION_DISCONTINUITY_TICKS: u64 = 20;
pub const REMOTE_INTERPOLATION_TELEPORT_DISTANCE: f32 = 8.0;
/// Dead-reckon at most this far past the newest sample (packet loss / stalls),
/// then hold — never sail off at the last velocity forever. With the 150 ms
/// interp delay this covers a ~450 ms websocket retransmit stall before a
/// remote freezes; longer would trade bigger corrections when data returns.
pub const REMOTE_EXTRAPOLATION_SECONDS: f32 = 0.30;
/// Across a longer gap (lost packets), a cubic can invent a broad curve far
/// from the real path, so fall back to lerp.
const HERMITE_MAX_SPAN_TICKS: u64 = 4;

/// A point on the server timeline: whole tick plus a fraction into the next.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct RemoteRenderTime {
    pub tick: u64,
    pub alpha: f32,
}

impl RemoteRenderTime {
    pub fn new(tick: u64, alpha: f32) -> Self {
        Self::from_ticks_f64((tick as f64 + alpha as f64).max(0.0))
    }

    pub fn from_seconds(seconds: f64, tick_hz: f64) -> Self {
        Self::from_ticks_f64((seconds.max(0.0) * tick_hz).max(0.0))
    }

    pub fn from_ticks_f64(ticks: f64) -> Self {
        let ticks = ticks.max(0.0);
        let mut tick = ticks.floor() as u64;
        let mut alpha = (ticks - tick as f64) as f32;
        // A fraction like .9999999998 rounds up to exactly 1.0 in f32; carry
        // it so the same instant never has two representations on the wire.
        if alpha >= 1.0 {
            tick += 1;
            alpha = 0.0;
        }
        Self { tick, alpha }
    }

    pub fn as_ticks_f64(self) -> f64 {
        self.tick as f64 + self.alpha as f64
    }

    pub fn clamp_ticks(self, min_tick: u64, max_tick: u64) -> Self {
        let min_ticks = min_tick as f64;
        let max_ticks = max_tick as f64;
        Self::from_ticks_f64(self.as_ticks_f64().clamp(min_ticks, max_ticks))
    }
}

bitflags::bitflags! {
    /// Discrete per-snapshot presentation flags — Q3's eFlags, not pm_flags:
    /// they ride the wire for viewers and never round-trip into
    /// `CharacterControllerState` or reconciliation.
    #[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub struct RemoteFlags: u8 {
        /// Owner was holding a movement key — presentation can't derive
        /// "sliding with no input" (skid) from velocity alone.
        const MOVE_INPUT = 1 << 0;
        /// No usercmds processed for a while (hidden tab, dying connection).
        /// Q3's EF_CONNECTION. The published velocity is zeroed while set so
        /// Hermite tangents and dead-reckoning stay quiet.
        const CONNECTION_INTERRUPTED = 1 << 1;
        /// The server had no real usercmd for this tick and repeated the
        /// player's previous input. A late command may revise this away.
        const FAKED_INPUT = 1 << 2;
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq)]
pub struct RemoteSnapshotSample {
    pub server_tick: u64,
    pub position: Vec3,
    pub velocity: Vec3,
    pub look: Vec2,
    pub state: NetAhoyMoveState,
    pub flags: RemoteFlags,
}

impl RemoteSnapshotSample {
    pub(crate) fn starts_new_motion_segment_after(self, previous: Self) -> bool {
        let tick_gap = self.server_tick.saturating_sub(previous.server_tick);
        tick_gap > REMOTE_INTERPOLATION_DISCONTINUITY_TICKS
            || self.position.distance(previous.position) > REMOTE_INTERPOLATION_TELEPORT_DISTANCE
    }

    pub fn interpolate_at(self, other: Self, render_time: RemoteRenderTime) -> Self {
        let span = other.server_tick.saturating_sub(self.server_tick).max(1) as f64;
        let alpha =
            ((render_time.as_ticks_f64() - self.server_tick as f64) / span).clamp(0.0, 1.0) as f32;
        self.interpolate_fraction(other, alpha, render_time)
    }

    pub fn interpolate_fraction(
        self,
        other: Self,
        alpha: f32,
        render_time: RemoteRenderTime,
    ) -> Self {
        let alpha = alpha.clamp(0.0, 1.0);
        Self {
            server_tick: render_time.tick,
            // Two positions cannot provide Source's preceding tangent, so the
            // public two-sample operation is the honest linear fallback.
            position: self.position.lerp(other.position, alpha),
            velocity: self.velocity.lerp(other.velocity, alpha),
            look: Vec2::new(
                lerp_radians(self.look.x, other.look.x, alpha),
                self.look.y.lerp(other.look.y, alpha),
            ),
            state: if alpha < 0.5 { self.state } else { other.state },
            // Union, not nearest-knot: velocity blends continuously across
            // the segment, so picking one knot's flags manufactures "moving
            // with no input" (a phantom skid) for the first half of every
            // start-running segment. Erring toward the flag being set only
            // delays a real skid/bounce boundary by at most one knot (50ms).
            flags: self.flags | other.flags,
        }
    }
}

/// Source's three-position Hermite: derive both tangents from observed
/// positions, never from simulation velocity. The older point is resampled
/// to an equal-duration interval exactly like TimeFixup2_Hermite.
fn source_hermite_sample(
    previous: RemoteSnapshotSample,
    start: RemoteSnapshotSample,
    end: RemoteSnapshotSample,
    render_time: RemoteRenderTime,
) -> RemoteSnapshotSample {
    let span_ticks = end.server_tick.saturating_sub(start.server_tick);
    let previous_span_ticks = start.server_tick.saturating_sub(previous.server_tick);
    if span_ticks == 0
        || span_ticks > HERMITE_MAX_SPAN_TICKS
        || previous_span_ticks == 0
        || start.starts_new_motion_segment_after(previous)
        || end.starts_new_motion_segment_after(start)
    {
        return start.interpolate_at(end, render_time);
    }

    let alpha = ((render_time.as_ticks_f64() - start.server_tick as f64)
        / span_ticks as f64)
        .clamp(0.0, 1.0) as f32;
    let interval_ratio = span_ticks as f32 / previous_span_ticks as f32;
    let fixed_previous = previous.position.lerp(start.position, 1.0 - interval_ratio);
    let start_tangent = start.position - fixed_previous;
    let end_tangent = end.position - start.position;
    let span_seconds = span_ticks as f32 / FIXED_TIMESTEP_HZ as f32;

    RemoteSnapshotSample {
        server_tick: render_time.tick,
        position: hermite_position(
            start.position,
            start_tangent / span_seconds,
            end.position,
            end_tangent / span_seconds,
            span_seconds,
            alpha,
        ),
        velocity: hermite_velocity(
            start.position,
            start_tangent / span_seconds,
            end.position,
            end_tangent / span_seconds,
            span_seconds,
            alpha,
        ),
        look: Vec2::new(
            lerp_radians(start.look.x, end.look.x, alpha),
            start.look.y.lerp(end.look.y, alpha),
        ),
        state: if alpha < 0.5 { start.state } else { end.state },
        flags: start.flags | end.flags,
    }
}

/// Hermite basis over `t ∈ [0, 1]`; tangents are velocity × span (velocity is
/// per-second, Hermite wants position-per-unit-parameter).
fn hermite_position(p0: Vec3, v0: Vec3, p1: Vec3, v1: Vec3, span_seconds: f32, t: f32) -> Vec3 {
    let t2 = t * t;
    let t3 = t2 * t;
    let h00 = 2.0 * t3 - 3.0 * t2 + 1.0;
    let h10 = t3 - 2.0 * t2 + t;
    let h01 = -2.0 * t3 + 3.0 * t2;
    let h11 = t3 - t2;
    p0 * h00 + v0 * (h10 * span_seconds) + p1 * h01 + v1 * (h11 * span_seconds)
}

fn hermite_velocity(p0: Vec3, v0: Vec3, p1: Vec3, v1: Vec3, span_seconds: f32, t: f32) -> Vec3 {
    let t2 = t * t;
    let dh00 = 6.0 * t2 - 6.0 * t;
    let dh10 = 3.0 * t2 - 4.0 * t + 1.0;
    let dh01 = -dh00;
    let dh11 = 3.0 * t2 - 2.0 * t;
    (p0 * dh00 + v0 * (dh10 * span_seconds) + p1 * dh01 + v1 * (dh11 * span_seconds)) / span_seconds
}

pub(crate) fn sample_buffer_at(
    samples: &VecDeque<RemoteSnapshotSample>,
    render_time: RemoteRenderTime,
) -> Option<RemoteSnapshotSample> {
    let first = samples.front().copied()?;
    let last = samples.back().copied()?;
    let target_ticks = render_time.as_ticks_f64();

    if target_ticks <= first.server_tick as f64 {
        return Some(first);
    }
    if target_ticks >= last.server_tick as f64 {
        // Past the newest sample (late/lost packets): dead-reckon on the last
        // velocity, capped, instead of freezing and snapping.
        let dt = (((target_ticks - last.server_tick as f64) / FIXED_TIMESTEP_HZ) as f32)
            .min(REMOTE_EXTRAPOLATION_SECONDS);
        return Some(RemoteSnapshotSample {
            position: last.position + last.velocity * dt,
            ..last
        });
    }

    let mut before_previous = None;
    let mut previous = first;
    for sample in samples.iter().copied().skip(1) {
        if target_ticks <= sample.server_tick as f64 {
            return Some(if let Some(before_previous) = before_previous {
                source_hermite_sample(before_previous, previous, sample, render_time)
            } else {
                previous.interpolate_at(sample, render_time)
            });
        }
        before_previous = Some(previous);
        previous = sample;
    }

    Some(last)
}

fn lerp_radians(from: f32, to: f32, alpha: f32) -> f32 {
    let delta =
        (to - from + std::f32::consts::PI).rem_euclid(std::f32::consts::TAU) - std::f32::consts::PI;
    from + delta * alpha
}

/// Ray vs the player hitbox: a vertical cylinder identical to the movement
/// collider (`Collider::cylinder(radius, half_height * 2)`). Movement, hit
/// registration, and visuals all share this one shape — you hit exactly what
/// you see, and what you see is exactly what blocks you.
pub fn ray_hitbox_distance(
    origin: Vec3,
    direction: Vec3,
    max_distance: f32,
    center: Vec3,
    radius: f32,
    half_height: f32,
) -> Option<f32> {
    let direction = direction.try_normalize()?;
    let cylinder = Collider::cylinder(radius, half_height * 2.0);
    let (distance, _) = cylinder.cast_ray(
        Position::new(center),
        Rotation::IDENTITY,
        origin,
        direction,
        max_distance,
        false,
    )?;
    Some(distance)
}

/// Per-player pose history on the server timeline. The server records the
/// sample it publishes each tick (`publish_authoritative_player_snapshots`);
/// the client mirrors it from received snapshots (`buffer_remote_snapshots`),
/// so both hold the same poses AND the same flags. Both peers sample it
/// at a command's seen time, so rocket-vs-player sweeps in the shared step
/// judge hits against the same poses and direct hits predict.
#[derive(Resource, Debug)]
pub struct LagCompensationHistory {
    pub max_frames: usize,
    pub poses: HashMap<PlayerId, VecDeque<RemoteSnapshotSample>>,
}

impl Default for LagCompensationHistory {
    fn default() -> Self {
        Self {
            max_frames: LAG_COMPENSATION_HISTORY_CAPACITY,
            poses: HashMap::new(),
        }
    }
}

impl LagCompensationHistory {
    pub fn record(&mut self, player_id: PlayerId, sample: RemoteSnapshotSample) {
        let samples = self
            .poses
            .entry(player_id)
            .or_insert_with(|| VecDeque::with_capacity(self.max_frames));

        if let Some(index) = samples
            .iter()
            .position(|existing| existing.server_tick >= sample.server_tick)
        {
            if samples[index].server_tick == sample.server_tick {
                samples[index] = sample;
            } else {
                samples.insert(index, sample);
            }
        } else {
            samples.push_back(sample);
        }

        while samples.len() > self.max_frames {
            samples.pop_front();
        }
    }

    pub fn pose_at_time(
        &self,
        player_id: PlayerId,
        server_time: RemoteRenderTime,
    ) -> Option<RemoteSnapshotSample> {
        sample_buffer_at(self.poses.get(&player_id)?, server_time)
    }

    /// Ray-test every player's hitbox (the movement cylinder) as it stood at
    /// `server_time` — the timestamp the shooter's client sampled its screen at.
    pub fn raycast_hitboxes_at_time(&self, cast: LagCompensatedCast) -> Option<LagCompensatedHit> {
        let direction = cast.direction.try_normalize()?;

        self.poses
            .keys()
            .copied()
            .filter(|player_id| cast.ignored_player != Some(*player_id))
            .filter_map(|player_id| {
                let pose = self.pose_at_time(player_id, cast.server_time)?;
                let distance = ray_hitbox_distance(
                    cast.origin,
                    direction,
                    cast.max_distance,
                    pose.position,
                    cast.radius,
                    cast.half_height,
                )?;
                Some(LagCompensatedHit {
                    player_id,
                    position: cast.origin + direction * distance,
                    distance,
                })
            })
            .min_by(|a, b| a.distance.total_cmp(&b.distance))
    }
}

#[derive(Clone, Copy, Debug)]
pub struct LagCompensatedCast {
    pub server_time: RemoteRenderTime,
    pub origin: Vec3,
    pub direction: Vec3,
    pub max_distance: f32,
    pub radius: f32,
    pub half_height: f32,
    pub ignored_player: Option<PlayerId>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LagCompensatedHit {
    pub player_id: PlayerId,
    pub position: Vec3,
    pub distance: f32,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(server_tick: u64, position: Vec3, velocity: Vec3) -> RemoteSnapshotSample {
        RemoteSnapshotSample {
            server_tick,
            position,
            velocity,
            look: Vec2::ZERO,
            state: NetAhoyMoveState::default(),
            flags: RemoteFlags::empty(),
        }
    }

    fn buffer(samples: &[RemoteSnapshotSample]) -> VecDeque<RemoteSnapshotSample> {
        samples.iter().copied().collect()
    }

    #[test]
    fn interpolation_hits_endpoints() {
        let a = sample(10, Vec3::new(0.0, 0.0, 0.0), Vec3::new(5.0, 3.0, 0.0));
        let b = sample(11, Vec3::new(1.0, 2.0, 3.0), Vec3::new(5.0, -3.0, 0.0));
        let buf = buffer(&[a, b]);

        let at_a = sample_buffer_at(&buf, RemoteRenderTime::new(10, 0.0)).unwrap();
        assert!(at_a.position.distance(a.position) < 1e-4);
        let at_b = sample_buffer_at(&buf, RemoteRenderTime::new(11, 0.0)).unwrap();
        assert!(at_b.position.distance(b.position) < 1e-4);
    }

    #[test]
    fn player_hermite_uses_positions_not_published_velocity() {
        let previous = sample(9, Vec3::new(-1.0, 0.0, 0.0), Vec3::Y * 1_000.0);
        let start = sample(10, Vec3::ZERO, Vec3::Y * -1_000.0);
        let end = sample(11, Vec3::X, Vec3::Y * 1_000.0);
        let buf = buffer(&[previous, start, end]);

        let mid = sample_buffer_at(&buf, RemoteRenderTime::new(10, 0.5)).unwrap();
        assert!(
            mid.position.y.abs() < 1e-5,
            "published velocity bent a position-derived curve: {}",
            mid.position.y
        );
        assert!((mid.position.x - 0.5).abs() < 1e-5);
        assert!((mid.velocity.x - FIXED_TIMESTEP_HZ as f32).abs() < 1e-3);
    }

    #[test]
    fn source_time_fixup_handles_unequal_snapshot_intervals() {
        // The object moved one unit per tick, but the older interval spans two
        // ticks. Source resamples it to one tick before constructing tangents.
        let previous = sample(8, Vec3::X * -2.0, Vec3::ZERO);
        let start = sample(10, Vec3::ZERO, Vec3::ZERO);
        let end = sample(11, Vec3::X, Vec3::ZERO);
        let buf = buffer(&[previous, start, end]);

        let mid = sample_buffer_at(&buf, RemoteRenderTime::new(10, 0.5)).unwrap();
        assert!((mid.position.x - 0.5).abs() < 1e-5);
        assert!((mid.velocity.x - FIXED_TIMESTEP_HZ as f32).abs() < 1e-3);
    }

    #[test]
    fn source_hermite_keeps_velocity_continuous_through_a_turn() {
        let buf = buffer(&[
            sample(9, Vec3::NEG_X, Vec3::ZERO),
            sample(10, Vec3::ZERO, Vec3::ZERO),
            sample(11, Vec3::Y, Vec3::ZERO),
            sample(12, Vec3::Y * 2.0, Vec3::ZERO),
        ]);

        let before = sample_buffer_at(&buf, RemoteRenderTime::new(10, 0.999)).unwrap();
        let after = sample_buffer_at(&buf, RemoteRenderTime::new(11, 0.001)).unwrap();
        assert!(before.velocity.distance(after.velocity) < 0.2);
    }

    #[test]
    fn long_gaps_fall_back_to_lerp() {
        let previous = sample(9, Vec3::NEG_X, Vec3::ZERO);
        let a = sample(10, Vec3::ZERO, Vec3::new(0.0, 100.0, 0.0));
        let b = sample(
            10 + HERMITE_MAX_SPAN_TICKS + 1,
            Vec3::new(10.0, 0.0, 0.0),
            Vec3::ZERO,
        );
        let buf = buffer(&[previous, a, b]);

        let mid = sample_buffer_at(&buf, RemoteRenderTime::new(12, 0.5)).unwrap();
        assert!(
            mid.position.y.abs() < 1e-4,
            "tangents must be ignored across long gaps, got y = {}",
            mid.position.y
        );
    }

    #[test]
    fn hitbox_matches_movement_cylinder() {
        let center = Vec3::new(0.0, 1.0, 0.0);

        // Side-on: surface at exactly the radius.
        let side = ray_hitbox_distance(
            Vec3::new(5.0, 1.0, 0.0),
            Vec3::NEG_X,
            100.0,
            center,
            0.45,
            0.75,
        )
        .unwrap();
        assert!((side - (5.0 - 0.45)).abs() < 1e-3);

        // The old capsule's cap reached y = center + 1.2; the cylinder tops
        // out at +0.75. A side-on ray at +0.9 must now miss.
        let over_head = ray_hitbox_distance(
            Vec3::new(5.0, 1.9, 0.0),
            Vec3::NEG_X,
            100.0,
            center,
            0.45,
            0.75,
        );
        assert!(over_head.is_none());

        // Straight down onto the flat lid at y = center + 0.75.
        let top = ray_hitbox_distance(
            Vec3::new(0.2, 5.0, 0.0),
            Vec3::NEG_Y,
            100.0,
            center,
            0.45,
            0.75,
        )
        .unwrap();
        assert!((top - (5.0 - 1.75)).abs() < 1e-3);
    }

    #[test]
    fn extrapolation_is_capped() {
        let a = sample(100, Vec3::ZERO, Vec3::new(10.0, 0.0, 0.0));
        let buf = buffer(&[a]);

        // One tick past: dead-reckon 50 ms at 10 m/s.
        let near = sample_buffer_at(&buf, RemoteRenderTime::new(101, 0.0)).unwrap();
        assert!((near.position.x - 0.5).abs() < 1e-4);

        // A full second past: clamped to REMOTE_EXTRAPOLATION_SECONDS.
        let far = sample_buffer_at(&buf, RemoteRenderTime::new(120, 0.0)).unwrap();
        assert!((far.position.x - 10.0 * REMOTE_EXTRAPOLATION_SECONDS).abs() < 1e-4);
    }

    #[test]
    fn lag_comp_history_replaces_a_revised_historical_tick() {
        let player = PlayerId(7);
        let mut history = LagCompensationHistory::default();
        history.record(player, sample(10, Vec3::ZERO, Vec3::ZERO));
        history.record(player, sample(11, Vec3::X, Vec3::ZERO));
        history.record(player, sample(12, Vec3::new(2.0, 0.0, 0.0), Vec3::ZERO));
        history.record(player, sample(11, Vec3::new(9.0, 0.0, 0.0), Vec3::ZERO));

        let revised = history
            .pose_at_time(player, RemoteRenderTime::new(11, 0.0))
            .unwrap();
        assert_eq!(revised.position, Vec3::new(9.0, 0.0, 0.0));
        assert_eq!(history.poses[&player].len(), 3);
    }
}
