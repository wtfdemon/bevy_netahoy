//! Poses on the server timeline: the time a viewer draws at, what a player publishes each
//! tick, and the curve through a history that gives a pose at any time ([`pose_at`],
//! [`body_at`]). Client interpolation and server lag compensation sample the same one.

use bevy::prelude::*;
use serde::{Deserialize, Serialize};

use crate::history::{Around, History};
use crate::protocol::{BodySnapshot, PlayerSnapshot, TickRate};

/// Farther than this in one tick (m) is a teleport, which no curve crosses.
pub(crate) const REMOTE_INTERPOLATION_TELEPORT_DISTANCE: f32 = 8.0;
/// Dead-reckon at most this far past the newest sample (s), then hold: longer trades bigger
/// corrections when data returns.
pub(crate) const REMOTE_EXTRAPOLATION_SECONDS: f32 = 0.30;

/// A point on the server timeline: whole tick plus a fraction into the next.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct RemoteRenderTime {
    /// The whole server tick.
    pub tick: u64,
    /// How far into the next tick, `0.0..1.0`.
    pub alpha: f32,
}

impl RemoteRenderTime {
    /// `alpha` of a tick past `tick`; a fraction that reaches a whole tick carries into `tick`.
    pub fn new(tick: u64, alpha: f32) -> Self {
        Self::from_ticks_f64((tick as f64 + alpha as f64).max(0.0))
    }

    /// `seconds` into the server timeline, which steps at `rate`.
    pub fn from_seconds(seconds: f64, rate: TickRate) -> Self {
        Self::from_ticks_f64((seconds.max(0.0) * f64::from(rate.hz())).max(0.0))
    }

    /// A fractional tick count, at least 0.
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

    /// As a fractional tick count.
    pub fn as_ticks_f64(self) -> f64 {
        self.tick as f64 + self.alpha as f64
    }

    /// Clamped to `min_tick..=max_tick`.
    pub fn clamp_ticks(self, min_tick: u64, max_tick: u64) -> Self {
        let min_ticks = min_tick as f64;
        let max_ticks = max_tick as f64;
        Self::from_ticks_f64(self.as_ticks_f64().clamp(min_ticks, max_ticks))
    }
}

bitflags::bitflags! {
    /// What viewers draw a player by besides its pose. Presentation only: never read back into
    /// the step or reconcile.
    #[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub struct RemoteFlags: u8 {
        /// The owner held a movement key: velocity alone can't tell a skid from a run.
        const MOVE_INPUT = 1 << 0;
        /// No usercmds processed for a while (hidden tab, dying connection).
        const CONNECTION_INTERRUPTED = 1 << 1;
        /// The server had no real usercmd for this tick and repeated the
        /// player's previous input. A late command re-steps the server's own
        /// timeline, never this published pose.
        const FAKED_INPUT = 1 << 4;
    }
}

/// A player's pose sampled from their [`History`]
/// ([`PlayerPoses::pose_at_time`](crate::PlayerPoses::pose_at_time)), with its velocity there:
/// the derivative of the curve through the logged positions, since no velocity rides the wire.
#[derive(Clone, Copy, Debug, Default, PartialEq, Deref)]
pub struct RemotePose {
    /// The pose at the time.
    #[deref]
    pub sample: PlayerSnapshot,
    /// Its velocity there (m/s).
    pub velocity: Vec3,
}

impl RemotePose {
    fn still(sample: PlayerSnapshot) -> Self {
        Self {
            sample,
            velocity: Vec3::ZERO,
        }
    }
}

/// A body's pose sampled from its [`History`] ([`body_at`]), with its
/// velocities there: derivatives of the curve through the logged poses, never
/// published ones.
#[derive(Clone, Copy, Debug, Default, PartialEq, Deref)]
pub struct BodyPose {
    /// The pose at the time.
    #[deref]
    pub snapshot: BodySnapshot,
    /// Its velocity there (m/s).
    pub linear_velocity: Vec3,
    /// Its angular velocity there (rad/s, about each axis).
    pub angular_velocity: Vec3,
}

impl BodyPose {
    fn still(snapshot: BodySnapshot) -> Self {
        Self {
            snapshot,
            ..default()
        }
    }
}

impl PlayerSnapshot {
    /// Moved further in one tick than anyone runs: a respawn or a teleport,
    /// which no curve should cross.
    fn teleported_from(self, previous: Self) -> bool {
        self.position.distance(previous.position) > REMOTE_INTERPOLATION_TELEPORT_DISTANCE
    }

    /// `alpha` of the way to `other`, the tick after this one.
    fn blend(
        self,
        other: Self,
        alpha: f32,
        position: Vec3,
        look: Vec2,
        render_time: RemoteRenderTime,
    ) -> Self {
        Self {
            server_tick: render_time.tick,
            position,
            look,
            move_flags: if alpha < 0.5 {
                self.move_flags
            } else {
                other.move_flags
            },
            // Union, not nearest knot: velocity blends across the segment, so one knot's flags
            // would make "moving with no input" (a phantom skid) for half of every start. The
            // union delays a real boundary by at most one knot.
            flags: self.flags | other.flags,
        }
    }
}

/// How far `time` is past `tick`, as a fraction of the tick after it.
fn alpha_after(tick: u64, time: RemoteRenderTime) -> f32 {
    (time.as_ticks_f64() - tick as f64).clamp(0.0, 1.0) as f32
}

/// The velocity through a knot on one axis (per second) from the chords either side, each
/// `tick` seconds long: their average, zero where either side holds still, so nothing leaves a
/// stand backwards or runs past a stop. With one chord, that one.
///
/// Per axis, because motion stops per axis: landing at a run, height holds while the rest
/// carries on, and a whole-vector guard would dip below the floor.
fn knot_speed(before: Option<f32>, after: Option<f32>, tick: f32) -> f32 {
    match (before, after) {
        (Some(before), Some(after)) if before == 0.0 || after == 0.0 => 0.0,
        (Some(before), Some(after)) => (before + after) * 0.5 / tick,
        (Some(chord), None) | (None, Some(chord)) => chord / tick,
        (None, None) => 0.0,
    }
}

fn knot_velocity(before: Option<Vec3>, after: Option<Vec3>, tick: f32) -> Vec3 {
    Vec3::from_array(std::array::from_fn(|axis| {
        knot_speed(
            before.map(|chord| chord[axis]),
            after.map(|chord| chord[axis]),
            tick,
        )
    }))
}

/// Position and velocity at `alpha` along `start → end`, one tick, on a cubic Hermite with
/// tangents from observed positions ([`knot_velocity`]), never from simulation velocity.
///
/// Without `after` (the newest segment, drawn only once the delay has run out) the end keeps
/// the motion into it, which extrapolation then continues; the curve moves when `after` lands.
fn source_hermite(
    previous: Option<Vec3>,
    start: Vec3,
    end: Vec3,
    after: Option<Vec3>,
    alpha: f32,
    tick: f32,
) -> (Vec3, Vec3) {
    let chord = end - start;
    let start_velocity =
        knot_velocity(previous.map(|previous| start - previous), Some(chord), tick);
    let end_velocity = knot_velocity(Some(chord), after.map(|after| after - end), tick);
    (
        hermite_position(start, start_velocity, end, end_velocity, tick, alpha),
        hermite_velocity(start, start_velocity, end, end_velocity, tick, alpha),
    )
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

/// Rotation and angular velocity at `alpha` along `start → end`, one tick: [`source_hermite`]
/// on the turns between neighbouring entries (world-frame scaled axes, the short way round),
/// with the same tangents ([`knot_velocity`]), so the angular velocity at an entry is the same
/// from the segments either side. Without neighbours it is the slerp. The turn is applied as one
/// rotation from `start`, so the angular velocity is the curve's exactly about a fixed axis, and
/// close to it otherwise.
fn rotation_hermite(
    previous: Option<Quat>,
    start: Quat,
    end: Quat,
    after: Option<Quat>,
    alpha: f32,
    tick: f32,
) -> (Quat, Vec3) {
    let turn = |from: Quat, to: Quat| angular_velocity(from, to, 1.0);
    let through = turn(start, end);
    let start_velocity = knot_velocity(
        previous.map(|previous| turn(previous, start)),
        Some(through),
        tick,
    );
    let end_velocity = knot_velocity(Some(through), after.map(|after| turn(end, after)), tick);
    let turned = hermite_position(
        Vec3::ZERO,
        start_velocity,
        through,
        end_velocity,
        tick,
        alpha,
    );
    (
        Quat::from_scaled_axis(turned) * start,
        hermite_velocity(
            Vec3::ZERO,
            start_velocity,
            through,
            end_velocity,
            tick,
            alpha,
        ),
    )
}

/// The constant angular velocity a slerp from `from` to `to` over `seconds`
/// turns at (the short way round).
fn angular_velocity(from: Quat, to: Quat, seconds: f32) -> Vec3 {
    let mut delta = to * from.inverse();
    if delta.w < 0.0 {
        delta = -delta;
    }
    delta.to_scaled_axis() / seconds
}

impl BodySnapshot {
    fn teleported_from(self, previous: Self) -> bool {
        self.position.distance(previous.position) > REMOTE_INTERPOLATION_TELEPORT_DISTANCE
    }
}

/// Seconds to dead-reckon `time` past `last_tick`, capped at
/// [`REMOTE_EXTRAPOLATION_SECONDS`], and whether the cap holds it there.
fn extrapolation_seconds(last_tick: u64, time: RemoteRenderTime, rate: TickRate) -> (f32, bool) {
    let seconds = ((time.as_ticks_f64() - last_tick as f64) / f64::from(rate.hz())) as f32;
    (
        seconds.min(REMOTE_EXTRAPOLATION_SECONDS),
        seconds >= REMOTE_EXTRAPOLATION_SECONDS,
    )
}

/// A body's pose at `time` from its [`History`]: what the step's movers and body rendering
/// both sample, on both peers.
///
/// - Before the oldest entry: the oldest.
/// - Past the newest: dead-reckoned on the last tick's motion, rotation too, for at most 0.3 s.
///   A body at rest held its pose over that tick, so it stands.
/// - Otherwise the players' Hermite through the entries around it, and rotation on the same
///   curve through the turns between them; linear (a slerp) from the oldest entry or across a
///   teleport.
///
/// The velocities are the curve's derivatives at `time`.
pub fn body_at(
    history: &History<BodySnapshot>,
    time: RemoteRenderTime,
    rate: TickRate,
) -> Option<BodyPose> {
    let tick = rate.seconds() as f32;
    match history.around(time.as_ticks_f64()) {
        Around::Empty => None,
        Around::Before(first) => Some(BodyPose::still(*first)),
        Around::After { before, last } => {
            let (linear, angular) = before
                .filter(|before| !last.teleported_from(**before))
                .map_or((Vec3::ZERO, Vec3::ZERO), |before| {
                    (
                        (last.position - before.position) / tick,
                        angular_velocity(before.rotation, last.rotation, tick),
                    )
                });
            let (dt, capped) = extrapolation_seconds(last.tick, time, rate);
            let moving = if capped { 0.0 } else { 1.0 };
            Some(BodyPose {
                snapshot: BodySnapshot {
                    tick: time.tick,
                    position: last.position + linear * dt,
                    rotation: Quat::from_scaled_axis(angular * dt) * last.rotation,
                    ..*last
                },
                linear_velocity: linear * moving,
                angular_velocity: angular * moving,
            })
        }
        Around::Between {
            before,
            start,
            end,
            after,
        } => {
            // At an entry's own tick, exactly the entry: whole-tick movers
            // are recorded poses, not a curve's rounding of them.
            let alpha = alpha_after(start.tick, time);
            let previous = before
                .filter(|before| !start.teleported_from(**before) && !end.teleported_from(*start));
            let after = after.filter(|after| !after.teleported_from(*end));
            let (position, linear_velocity) = source_hermite(
                previous.map(|previous| previous.position),
                start.position,
                end.position,
                after.map(|after| after.position),
                alpha,
                tick,
            );
            let (rotation, angular_velocity) = rotation_hermite(
                previous.map(|previous| previous.rotation),
                start.rotation,
                end.rotation,
                after.map(|after| after.rotation),
                alpha,
                tick,
            );
            Some(BodyPose {
                snapshot: BodySnapshot {
                    tick: time.tick,
                    position: if alpha == 1.0 { end.position } else { position },
                    rotation: if alpha == 1.0 { end.rotation } else { rotation },
                    ..*end
                },
                linear_velocity,
                angular_velocity,
            })
        }
    }
}

/// A player's pose at `time` from their [`History`]: what remote rendering, lag-compensated
/// hits and the player push all sample, on both peers.
///
/// - Before the oldest entry: the oldest.
/// - Past the newest (late packets): dead-reckoned on the last tick's motion for at most 0.3 s,
///   so motion runs on instead of freezing. A player who stopped held still over that tick, so
///   they stand.
/// - Across a teleport: the pose before it until its tick, then the one after, no slide.
/// - Otherwise a Hermite through the entries around it, position and look (linear from the
///   oldest entry). A stop is the same position held: the curve eases into it, never past it.
///
/// The velocity is the curve's derivative at `time`.
pub(crate) fn pose_at(
    history: &History<PlayerSnapshot>,
    time: RemoteRenderTime,
    rate: TickRate,
) -> Option<RemotePose> {
    let tick = rate.seconds() as f32;
    match history.around(time.as_ticks_f64()) {
        Around::Empty => None,
        Around::Before(first) => Some(RemotePose::still(*first)),
        Around::After { before, last } => {
            let last = *last;
            let velocity = before
                .filter(|before| !last.teleported_from(**before))
                .map_or(Vec3::ZERO, |before| {
                    (last.position - before.position) / tick
                });
            let (dt, capped) = extrapolation_seconds(last.server_tick, time, rate);
            Some(RemotePose {
                sample: PlayerSnapshot {
                    position: last.position + velocity * dt,
                    ..last
                },
                velocity: if capped { Vec3::ZERO } else { velocity },
            })
        }
        Around::Between {
            before,
            start,
            end,
            after,
        } => {
            let (start, end) = (*start, *end);
            let alpha = alpha_after(start.server_tick, time);
            if end.teleported_from(start) {
                return Some(RemotePose::still(if alpha == 1.0 { end } else { start }));
            }
            let before = before.filter(|before| !start.teleported_from(**before));
            let after = after.filter(|after| !after.teleported_from(end));
            let (position, velocity) = source_hermite(
                before.map(|before| before.position),
                start.position,
                end.position,
                after.map(|after| after.position),
                alpha,
                tick,
            );
            let look = look_hermite(
                before.map(|before| before.look),
                start.look,
                end.look,
                after.map(|after| after.look),
                alpha,
                tick,
            );
            // At an entry's own tick, exactly the entry, at the curve's velocity.
            let sample = if alpha == 1.0 {
                end
            } else {
                start.blend(end, alpha, position, look, time)
            };
            Some(RemotePose { sample, velocity })
        }
    }
}

/// The short way round from `from` to `to`.
pub(crate) fn radians_between(from: f32, to: f32) -> f32 {
    (to - from + std::f32::consts::PI).rem_euclid(std::f32::consts::TAU) - std::f32::consts::PI
}

/// Most a remote look can pitch; a tangent past a turn could overshoot it.
const REMOTE_PITCH_LIMIT: f32 = 1.5;

/// The look at `alpha` along `start → end`: yaw the short way round, pitch
/// straight, on the same curve and tangents as position ([`knot_velocity`]).
fn look_hermite(
    previous: Option<Vec2>,
    start: Vec2,
    end: Vec2,
    after: Option<Vec2>,
    alpha: f32,
    tick: f32,
) -> Vec2 {
    let chord = |from: Vec2, to: Vec2| Vec3::new(radians_between(from.x, to.x), to.y - from.y, 0.0);
    let through = chord(start, end);
    let start_velocity = knot_velocity(
        previous.map(|previous| chord(previous, start)),
        Some(through),
        tick,
    );
    let end_velocity = knot_velocity(Some(through), after.map(|after| chord(end, after)), tick);
    let from = start.extend(0.0);
    let look = hermite_position(
        from,
        start_velocity,
        from + through,
        end_velocity,
        tick,
        alpha,
    );
    Vec2::new(
        look.x,
        look.y.clamp(-REMOTE_PITCH_LIMIT, REMOTE_PITCH_LIMIT),
    )
}

#[cfg(test)]
mod tests;
