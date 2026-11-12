//! The remote render clock: the server time remote players are drawn at.

use bevy::prelude::*;

use crate::{
    demo::DemoPlayback,
    game::PlayerGame,
    poses::{REMOTE_EXTRAPOLATION_SECONDS, RemoteRenderTime},
    protocol::*,
};

use super::{OwnPlayerId, command_clock::CommandClock};

/// How far behind the newest snapshot remotes are drawn (s). Over a stream a loss is a
/// retransmit stall and then an in-order burst; the delay is what covers the stall.
pub(super) const REMOTE_INTERPOLATION_DELAY_SECONDS: f64 = 0.15;
/// If playback falls this far from the delayed head, it resyncs in one jump (s).
const REMOTE_CLOCK_RESET_SECONDS: f64 = 0.5;
/// Distance (ticks) from the delayed head past which playback runs a few percent fast or slow to
/// rejoin it: that reads as smooth motion, where the reset's snap reads as a teleport.
const REMOTE_CLOCK_SLEW_TICKS: f64 = 1.0;
const REMOTE_CLOCK_SLEW_RATE: f64 = 1.05;
/// Lag past which the slew runs at [`REMOTE_CLOCK_FAST_SLEW_RATE`] (s).
const REMOTE_CLOCK_FAST_SLEW_SECONDS: f64 = 0.2;
const REMOTE_CLOCK_FAST_SLEW_RATE: f64 = 1.10;

/// What the remote render clock did over a report window, to tell loss, snaps and render
/// hitches apart.
#[derive(Debug, Default, Clone, Copy)]
pub struct RemoteClockStats {
    /// Times the clock fell too far from the delayed head and jumped to it.
    pub snaps: u32,
    /// Ticks drawn past the newest snapshot (dead-reckoned): a late or lost head.
    pub extrapolated_frames: u32,
    /// Extrapolated frames that hit the hard cap and held position.
    pub capped_frames: u32,
    /// Frames in which the owner snapshot's server tick moved on: the
    /// heartbeat the clock reads.
    pub heartbeat_frames: u32,
    /// Most ticks the heartbeat moved on in one frame (1 = every tick in a
    /// frame of its own; more = a burst after a stall).
    pub max_heartbeat_jump: u32,
    /// Render-time intervals between remote fixed-tick poses, counted. Transform easing
    /// expects each to be one server tick.
    pub(crate) endpoint_intervals: u32,
    /// Of those, intervals of no time at all.
    pub(crate) endpoint_duplicate_times: u32,
    /// The shortest of those intervals, in server ticks.
    pub(crate) endpoint_min_delta_ticks: f32,
    /// The longest of those intervals, in server ticks.
    pub(crate) endpoint_max_delta_ticks: f32,
    /// Largest jump of the newest observed server tick in one message batch:
    /// the snapshot arrival cadence in ticks (1 = every tick, 4 = batchy).
    pub head_jump_max: u32,
    /// Error-bridge folds: a remote's curve changed under the viewer.
    pub bridge_folds: u32,
    /// The metres those folds took up.
    pub bridge_folded: f32,
}

impl RemoteClockStats {
    /// Whether anything was counted.
    pub(crate) fn any(self) -> bool {
        self.snaps > 0
            || self.extrapolated_frames > 0
            || self.capped_frames > 0
            || self.heartbeat_frames > 0
            || self.endpoint_intervals > 0
    }
}

/// The clock remote players are drawn by: a render time a fixed delay behind the newest server
/// tick, slewed to stay there.
#[derive(Resource, Debug)]
pub struct RemoteClock {
    /// The newest server tick seen.
    pub(crate) latest_server_tick: u64,
    pub(super) tick_hz: f64,
    /// How far behind the newest server tick remotes are drawn (s).
    pub(crate) interpolation_delay_seconds: f64,
    /// What the clock did since the last 30 s report.
    pub(crate) stats: RemoteClockStats,
    render_server_ticks: f64,
    /// Ticks the cursor moved this fixed step (the slew rate, or the snap
    /// distance). Sub-tick render sampling scales the overstep fraction by
    /// this so render time is continuous through slewed ticks.
    pub(super) last_advance_ticks: f64,
    /// Consecutive ticks the cursor has been ahead of the delayed head;
    /// gates the down-slew so bursty snapshot arrival can't trigger it.
    ahead_ticks_streak: u32,
    initialized: bool,
    /// Demo playback ([`Self::align_to_demo`]): the cursor runs one tick per tick, no slew, no
    /// snap, so the render time stays a pure function of the demo clock however the recording's
    /// arrivals jittered.
    demo_locked: bool,
}

impl RemoteClock {
    /// The newest server tick seen.
    pub fn latest_server_tick(&self) -> u64 {
        self.latest_server_tick
    }

    /// What the clock did since its last 30 s report.
    pub fn stats(&self) -> RemoteClockStats {
        self.stats
    }

    /// A clock that has seen nothing yet, for a server stepping at `rate`.
    pub(crate) fn new(rate: TickRate) -> Self {
        Self::at_hz(f64::from(rate.hz()))
    }

    /// Back to `new` at the same rate: the next server tick seats the cursor.
    pub(crate) fn reset(&mut self) {
        *self = Self::at_hz(self.tick_hz);
    }

    fn at_hz(tick_hz: f64) -> Self {
        Self {
            latest_server_tick: 0,
            tick_hz,
            interpolation_delay_seconds: REMOTE_INTERPOLATION_DELAY_SECONDS,
            stats: RemoteClockStats::default(),
            render_server_ticks: 0.0,
            last_advance_ticks: 1.0,
            ahead_ticks_streak: 0,
            initialized: false,
            demo_locked: false,
        }
    }

    /// Moves the head on to `server_tick` if it is newer; the first one seats the cursor.
    pub(crate) fn observe_server_tick(&mut self, server_tick: u64) {
        // The owner snapshot holds its tick on frames that brought nothing
        // new; only a newer tick moves the head.
        if server_tick == 0 || server_tick <= self.latest_server_tick {
            return;
        }

        if self.initialized {
            let jump = server_tick.saturating_sub(self.latest_server_tick) as u32;
            self.stats.head_jump_max = self.stats.head_jump_max.max(jump);
            self.stats.max_heartbeat_jump = self.stats.max_heartbeat_jump.max(jump);
        }
        self.stats.heartbeat_frames += 1;
        self.latest_server_tick = server_tick;
        if !self.initialized {
            self.render_server_ticks = self.latest_renderable_server_ticks();
            self.initialized = true;
        }
    }

    /// Steps the cursor one fixed tick toward the delayed head.
    #[cfg(test)]
    pub(crate) fn advance_fixed(&mut self) {
        self.advance_fixed_scaled(1.0);
    }

    /// `advance_fixed` for a local tick that stands for `scale` of a real tick: `1 / pace` while
    /// [`CommandClock`] runs the fixed clock fast or slow (remotes stay in real time), 0 for a
    /// catch-up tick.
    ///
    /// A catch-up tick leaves the clock untouched, so [`Self::render_time`] keeps interpolating
    /// across its frame: it runs first in the frame, usually with no real tick after it, and a
    /// zero step would collapse the sub-tick span (a jump, then a hold). A snap waits for the
    /// next real tick too.
    pub(crate) fn advance_fixed_scaled(&mut self, scale: f64) {
        if !self.initialized || scale == 0.0 {
            return;
        }

        let delayed_head = self.latest_renderable_server_ticks();
        let lag = delayed_head - self.render_server_ticks;
        if self.demo_locked {
            // Not even the extrapolation cap: a held cursor would never make
            // up the time (no slew), and the samplers cap their own
            // extrapolation past the newest snapshot.
            self.render_server_ticks += scale;
            self.last_advance_ticks = scale;
        } else if lag.abs() > REMOTE_CLOCK_RESET_SECONDS * self.tick_hz {
            self.render_server_ticks = delayed_head;
            self.last_advance_ticks = 1.0;
            self.stats.snaps += 1;
        } else {
            // Slowing down needs persistence: with bursty arrival a negative lag is routine
            // between batches, and slowing on every dip plays remotes in slow motion. A cursor
            // really ahead (a hitch's catch-up steps, no new snapshots) is ahead every tick, so a
            // second of it is required. Without the down slew nothing pulls the cursor back from
            // the extrapolation cap.
            if lag < -REMOTE_CLOCK_SLEW_TICKS {
                self.ahead_ticks_streak += 1;
            } else {
                self.ahead_ticks_streak = 0;
            }
            let ahead_persistent = f64::from(self.ahead_ticks_streak) >= self.tick_hz;
            let fast = REMOTE_CLOCK_FAST_SLEW_SECONDS * self.tick_hz;
            let rate = if lag > fast {
                REMOTE_CLOCK_FAST_SLEW_RATE
            } else if lag > REMOTE_CLOCK_SLEW_TICKS {
                REMOTE_CLOCK_SLEW_RATE
            } else if ahead_persistent && lag < -fast {
                2.0 - REMOTE_CLOCK_FAST_SLEW_RATE
            } else if ahead_persistent && lag < -REMOTE_CLOCK_SLEW_TICKS {
                2.0 - REMOTE_CLOCK_SLEW_RATE
            } else {
                1.0
            };
            let extrapolation_ticks = f64::from(REMOTE_EXTRAPOLATION_SECONDS) * self.tick_hz;
            let next = (self.render_server_ticks + rate * scale)
                .min(self.latest_server_tick as f64 + extrapolation_ticks);
            self.last_advance_ticks = next - self.render_server_ticks;
            self.render_server_ticks = next;
        }

        let newest = self.latest_server_tick as f64;
        let max_render = newest + f64::from(REMOTE_EXTRAPOLATION_SECONDS) * self.tick_hz;
        // As in RemotePlaybackStatus: a tick is extrapolated when the pose drawn is past the head,
        // not when the cursor is about to be.
        let render_window_start = self.render_server_ticks - self.last_advance_ticks;
        if render_window_start >= max_render {
            self.stats.capped_frames += 1;
        }
        if render_window_start > newest {
            self.stats.extrapolated_frames += 1;
        }
    }

    /// Demo playback: seats the cursor so this frame draws remotes at `render_ticks`, a pure
    /// function of the demo clock rather than a whole tick, and locks it (`demo_locked`): straight
    /// playback and a seek to the same time then draw the same. Call it right after
    /// initialisation, before this frame's fixed steps: `overstep` is last frame's leftover and
    /// `delta` the time those steps will consume, so this frame's time
    /// (`cursor + steps + overstep_after - 1`, see [`Self::render_time`]) is `render_ticks`.
    pub(crate) fn align_to_demo(&mut self, render_ticks: f64, overstep: f32, delta: f64) {
        if !self.initialized {
            return;
        }
        self.render_server_ticks = render_ticks - delta * self.tick_hz + 1.0 - f64::from(overstep);
        self.demo_locked = true;
    }

    /// The render time this fixed tick freezes; `None` before the first server tick.
    pub(crate) fn target_time(&self) -> Option<RemoteRenderTime> {
        self.initialized
            .then(|| RemoteRenderTime::from_ticks_f64(self.render_server_ticks))
    }

    /// The render time of a frame at `overstep_fraction`: one cursor step behind this tick's
    /// [`target_time`](Self::target_time) at 0, on it at 1, like transform easing between the
    /// last two tick poses. Sampling ahead of the target would run into extrapolation and draw
    /// remotes ahead of the seen time lag compensation uses.
    pub(crate) fn render_time(&self, overstep_fraction: f32) -> Option<RemoteRenderTime> {
        self.initialized.then(|| {
            RemoteRenderTime::from_ticks_f64(
                self.render_server_ticks
                    + (f64::from(overstep_fraction) - 1.0) * self.last_advance_ticks,
            )
        })
    }

    fn latest_renderable_server_ticks(&self) -> f64 {
        (self.latest_server_tick as f64 - self.interpolation_delay_seconds * self.tick_hz).max(0.0)
    }
}

pub(super) fn advance_server_clock_fixed(
    mut clock: ResMut<RemoteClock>,
    mut command_clock: ResMut<CommandClock>,
) {
    let scale = command_clock.remote_tick_scale();
    clock.advance_fixed_scaled(scale);
}

pub(super) fn reset_server_clock(mut clock: ResMut<RemoteClock>) {
    clock.reset();
}

/// Moves the remote clock's head on the local player's [`OwnerSnapshot`]: the server rewrites
/// it every tick, so it beats even in a still world, where replication alone (only what changed)
/// would starve the clock. The input clock steers on the same signal, and the histories run to
/// its tick ([`hold_histories`]).
///
/// A live client with no player of its own gets no owner snapshot and would need a heartbeat of
/// its own. Demo playback reads the recorded player's.
pub(super) fn update_server_clock<G: PlayerGame>(
    time: Res<Time>,
    mut clock: ResMut<RemoteClock>,
    local: Res<OwnPlayerId>,
    mut playback: Option<ResMut<DemoPlayback>>,
    players: Query<(&PlayerId, &OwnerSnapshot<G>), With<ServerPlayer>>,
    replicated: Res<crate::history::ReplicatedTicks>,
    fixed: Res<Time<Fixed>>,
    mut window: Local<f32>,
) {
    let spectating = playback.is_some();
    let was_initialized = clock.initialized;
    let own = local.0.or(playback
        .as_ref()
        .and_then(|playback| playback.recorded_player));
    if let Some(snapshot) = own.and_then(|own| {
        players
            .iter()
            .find(|(id, _)| **id == own)
            .map(|(_, snapshot)| snapshot)
    }) {
        // Only the newest tick matters: a burst of several moves the head once.
        clock.observe_server_tick(snapshot.server_tick);
    } else if spectating && own.is_none() {
        // A spectator's demo (no JoinRequest, so no owner snapshot): the
        // tick every replication message carries. It only moves when
        // something changed, which a world with anyone in it always has.
        clock.observe_server_tick(replicated.newest());
    }
    if !was_initialized
        && clock.initialized
        && let Some(playback) = playback.as_mut()
        && let Some(arrived) = playback.newest_record_time()
    {
        // Playback draws remotes at a fixed offset from the demo clock: the one its first start
        // found, kept for the session. A seek re-seats the cursor on it rather than on the newest
        // record's arrival, which jitters from record to record.
        let hz = clock.tick_hz;
        let demo_ticks = playback.clock * hz;
        let start = clock.render_server_ticks + (playback.clock - arrived) * hz;
        let offset = *playback
            .remote_clock_offset
            .get_or_insert(start - demo_ticks);
        clock.align_to_demo(
            demo_ticks + offset,
            fixed.overstep_fraction(),
            time.delta_secs_f64(),
        );
    }
    *window += time.delta_secs();
    if *window >= 30.0 {
        let stats = clock.stats;
        if stats.any() {
            info!(
                "remote clock, last {:.0}s: {} heartbeat frames (max burst {}); {} extrapolated frames ({} capped), {} resets; fixed endpoint dt {:.3}..{:.3} ticks ({} duplicate, {} intervals; Avian expects 1.000)",
                *window,
                stats.heartbeat_frames,
                stats.max_heartbeat_jump,
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

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: TickRate = TickRate::new(20);

    fn clock_at(latest_tick: u64) -> RemoteClock {
        let mut clock = RemoteClock::new(RATE);
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
        let max = 100.0 + f64::from(REMOTE_EXTRAPOLATION_SECONDS) * f64::from(RATE.hz());
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
        let expected = 140.0 - REMOTE_INTERPOLATION_DELAY_SECONDS * f64::from(RATE.hz());
        let end = clock.target_time().unwrap().as_ticks_f64();
        assert!(
            (end - expected).abs() < 0.5,
            "clock should snap to {expected}, got {end}"
        );
    }

    /// Steady ticks, a frame of only catch-up ticks, steady again: the
    /// render time at evenly spaced overstep fractions moves on evenly, with
    /// no hold and no jump.
    #[test]
    fn catch_up_ticks_neither_freeze_nor_jump_remotes() {
        let mut clock = clock_at(100);
        let mut samples = Vec::new();
        let mut sample = |clock: &RemoteClock, overstep: f32| {
            samples.push(clock.render_time(overstep).unwrap().as_ticks_f64());
        };
        for server_tick in 101..=110 {
            clock.observe_server_tick(server_tick);
            clock.advance_fixed_scaled(1.0);
            sample(&clock, 0.0);
            sample(&clock, 0.25);
            if server_tick == 105 {
                // A resync's catch-up burst lands mid-tick: three ticks that
                // stand for no time, then the frame renders.
                for _ in 0..3 {
                    clock.advance_fixed_scaled(0.0);
                }
            }
            sample(&clock, 0.5);
            sample(&clock, 0.75);
        }
        for pair in samples.windows(2) {
            let step = pair[1] - pair[0];
            assert!(
                (step - 0.25).abs() < 1e-9,
                "render time stepped {step}: {samples:?}"
            );
        }
    }

    #[test]
    fn fixed_clock_holds_its_0_15_s_delay() {
        let mut clock = clock_at(100);
        for server_tick in 101..=1_000 {
            clock.observe_server_tick(server_tick);
            clock.advance_fixed();
            assert_eq!(
                clock.target_time().unwrap().as_ticks_f64(),
                server_tick as f64 - REMOTE_INTERPOLATION_DELAY_SECONDS * f64::from(RATE.hz())
            );
        }
    }
}
