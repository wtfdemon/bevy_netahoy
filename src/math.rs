//! Interpolation timing and capsule-cast math shared by client interpolation
//! and server lag compensation.

use std::collections::VecDeque;

use avian3d::prelude::{Collider, Position, Rotation};
use bevy::prelude::*;
use serde::{Deserialize, Serialize};

use crate::protocol::{AhoySnapshot, NetAhoyMoveState, FIXED_TIMESTEP_HZ};

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
    pub fn from_snapshot(snapshot: &AhoySnapshot) -> Self {
        Self {
            server_tick: snapshot.server_tick,
            position: snapshot.position,
            velocity: snapshot.velocity,
            look: snapshot.look,
            state: snapshot.state,
        }
    }

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

pub fn ray_capsule_distance(
    origin: Vec3,
    direction: Vec3,
    max_distance: f32,
    capsule_center: Vec3,
    radius: f32,
    half_height: f32,
) -> Option<f32> {
    let direction = direction.try_normalize()?;
    ray_segment_capsule_distance(
        origin,
        direction,
        max_distance,
        capsule_center - Vec3::Y * half_height,
        capsule_center + Vec3::Y * half_height,
        radius,
    )
}

pub(crate) fn ray_segment_capsule_distance(
    origin: Vec3,
    direction: Vec3,
    max_distance: f32,
    segment_a: Vec3,
    segment_b: Vec3,
    radius: f32,
) -> Option<f32> {
    let direction = direction.try_normalize()?;
    let capsule_center = segment_a.midpoint(segment_b);
    let capsule = Collider::capsule_endpoints(
        radius,
        segment_a - capsule_center,
        segment_b - capsule_center,
    );
    let (distance, _) = capsule.cast_ray(
        Position::new(capsule_center),
        Rotation::IDENTITY,
        origin,
        direction,
        max_distance,
        false,
    )?;
    Some(distance)
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
