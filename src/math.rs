//! Interpolation timing and capsule-cast math shared by client interpolation
//! and server lag compensation.

use std::collections::{HashMap, VecDeque};

use avian3d::prelude::{Collider, Position, Rotation};
use bevy::prelude::*;
use serde::{Deserialize, Serialize};

use crate::protocol::{NetAhoyMoveState, PlayerId, FIXED_TIMESTEP_HZ};

pub const LAG_COMPENSATION_HISTORY_CAPACITY: usize = 128;

pub const REMOTE_INTERPOLATION_DISCONTINUITY_TICKS: u64 = 20;
pub const REMOTE_INTERPOLATION_TELEPORT_DISTANCE: f32 = 8.0;
/// Dead-reckon at most this far past the newest sample (packet loss / stalls),
/// then hold — never sail off at the last velocity forever.
pub const REMOTE_EXTRAPOLATION_SECONDS: f32 = 0.25;
/// Hermite needs trustworthy tangents. Across a longer gap (lost packets) the
/// velocity-scaled curve can bulge far off the real path, so fall back to lerp.
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
        let tick = ticks.floor() as u64;
        let alpha = (ticks - tick as f64) as f32;
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

#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq)]
pub struct RemoteSnapshotSample {
    pub server_tick: u64,
    pub position: Vec3,
    pub velocity: Vec3,
    pub look: Vec2,
    pub state: NetAhoyMoveState,
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
        // Cubic Hermite with the sampled velocities as tangents: a 20 Hz jump
        // arc renders as an arc instead of chords with kinks at every knot.
        // Lag compensation samples through here too, so the server
        // reconstructs the same curve viewers saw.
        let span_ticks = other.server_tick.saturating_sub(self.server_tick);
        let position = if span_ticks > 0 && span_ticks <= HERMITE_MAX_SPAN_TICKS {
            let span_seconds = span_ticks as f32 / FIXED_TIMESTEP_HZ as f32;
            hermite_position(
                self.position,
                self.velocity,
                other.position,
                other.velocity,
                span_seconds,
                alpha,
            )
        } else {
            self.position.lerp(other.position, alpha)
        };
        Self {
            server_tick: render_time.tick,
            position,
            velocity: self.velocity.lerp(other.velocity, alpha),
            look: Vec2::new(
                lerp_radians(self.look.x, other.look.x, alpha),
                self.look.y.lerp(other.look.y, alpha),
            ),
            state: if alpha < 0.5 { self.state } else { other.state },
        }
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

    let mut previous = first;
    for sample in samples.iter().copied().skip(1) {
        if target_ticks <= sample.server_tick as f64 {
            return Some(previous.interpolate_at(sample, render_time));
        }
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

/// Per-player pose history on the server timeline. The server records every
/// tick's poses (`record_lag_compensation_history`); the client mirrors it
/// from received snapshots (`buffer_remote_snapshots`). Both peers sample it
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
        if samples
            .back()
            .is_some_and(|last| last.server_tick == sample.server_tick)
        {
            *samples.back_mut().unwrap() = sample;
            return;
        }

        if samples.len() == self.max_frames {
            samples.pop_front();
        }
        samples.push_back(sample);
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
    fn hermite_arcs_along_velocity() {
        // Rising then falling: the midpoint of the arc must bulge above the
        // straight chord (which stays at y = 0).
        let a = sample(10, Vec3::ZERO, Vec3::new(2.0, 10.0, 0.0));
        let b = sample(11, Vec3::new(0.1, 0.0, 0.0), Vec3::new(2.0, -10.0, 0.0));
        let buf = buffer(&[a, b]);

        let mid = sample_buffer_at(&buf, RemoteRenderTime::new(10, 0.5)).unwrap();
        assert!(mid.position.y > 0.05, "expected an arc, got {}", mid.position.y);
    }

    #[test]
    fn long_gaps_fall_back_to_lerp() {
        let a = sample(10, Vec3::ZERO, Vec3::new(0.0, 100.0, 0.0));
        let b = sample(10 + HERMITE_MAX_SPAN_TICKS + 1, Vec3::new(10.0, 0.0, 0.0), Vec3::ZERO);
        let buf = buffer(&[a, b]);

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
        let side =
            ray_hitbox_distance(Vec3::new(5.0, 1.0, 0.0), Vec3::NEG_X, 100.0, center, 0.45, 0.75)
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
        let top =
            ray_hitbox_distance(Vec3::new(0.2, 5.0, 0.0), Vec3::NEG_Y, 100.0, center, 0.45, 0.75)
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
}
