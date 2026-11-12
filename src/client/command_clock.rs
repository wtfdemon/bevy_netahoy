//! The input clock: paces the local ticks so each command reaches the server just before its
//! tick.

use std::time::Duration;

use bevy::prelude::*;

use crate::{game::PlayerGame, protocol::*};

use super::{OwnPlayerId, PredictedPlayer, predict::ClientInputState};

/// The band (ticks) the worst reported arrival lead ([`InputLead::min`]) is steered into. Below
/// it one jitter spike makes a command miss its tick (a guess, then a rollback); above it is
/// input latency nobody needs. The tick of slack at the bottom (0 = just in time) absorbs jitter;
/// the width keeps the clock from hunting.
pub(crate) const INPUT_LEAD_BAND: (i32, i32) = (1, 3);
/// Pace change per tick of lead above the band's bottom edge, inside the band. The band alone
/// parks the lead wherever it enters, so a stream that starts early (boot hitches) would keep
/// latency it never needs all session. A quarter of the outside gain walks it down, too slowly to
/// hunt: the window's min sees the newest arrival within one RTT.
pub(super) const COMMAND_CLOCK_DRAIN_GAIN: f64 = 0.005;
/// Pace change per tick of lead error outside [`INPUT_LEAD_BAND`]. Proportional only: the lead
/// is already the integral of pace error, and an integral term on top would hunt between the pace
/// limits. Under clock drift the lead settles at the band's bottom edge, which its slack covers.
pub(super) const COMMAND_CLOCK_GAIN: f64 = 0.02;
pub(crate) const COMMAND_CLOCK_MAX_SPEEDUP: f64 = 0.06;
pub(crate) const COMMAND_CLOCK_MAX_SLOWDOWN: f64 = 0.04;
/// Max change of the applied pace per real second. The pace scales how fast the rendered player
/// travels: a ramp keeps that a gentle acceleration, not a velocity step.
pub(super) const COMMAND_CLOCK_RATE_SLEW: f64 = 0.05;
/// How late even the freshest command in a full lead window may land before the client resyncs
/// (s). Past it the client is behind the server's timeline (lost ticks), not jittery: catch up
/// or skip instead of nudging for seconds through constant rollbacks. Never under
/// [`INPUT_LEAD_RESYNC_BEHIND_MIN_TICKS`], however few ticks it rounds to.
pub(super) const INPUT_LEAD_RESYNC_BEHIND_SECONDS: f64 = 0.3;
/// The fewest ticks late the lead resync waits for, as many as its seconds are at 10 Hz: one
/// past lead -2, where the whole [`COMMAND_CLOCK_MAX_SPEEDUP`] (at [`COMMAND_CLOCK_GAIN`] a
/// tick, 3 under [`INPUT_LEAD_BAND`]) holds a clock that lags the server's by all of it.
const INPUT_LEAD_RESYNC_BEHIND_MIN_TICKS: i32 = 3;
/// The most ticks one catch-up simulates, for lead resyncs and for frames Bevy clamped
/// (`Time<Virtual>::max_delta`). Past it the stream skips sequence numbers instead.
pub(super) const COMMAND_CLOCK_CATCHUP_MAX_TICKS: u32 = 20;

pub(super) fn reset_command_clock(mut clock: ResMut<CommandClock>) {
    clock.reset();
}

/// The input clock: runs the client's fixed clock a few percent fast or slow so each command
/// lands just ahead of its server tick, steered by the [`InputLead`] in every owner snapshot. A
/// command that lands after its tick runs on the server as a guess, then rolls back.
///
/// Pace only changes how often a tick fires in real time, through the fixed clock's overstep:
/// every tick still steps one `Time<Fixed>` timestep, so prediction stays bit-identical to the
/// server's step.
///
/// Past what nudging can fix it resyncs instead:
/// - frames Bevy clamped to `max_delta` (hitches) are simulated right away,
/// - consistently late commands (lost ticks) catch up by simulating the deficit, or skip
///   sequence numbers when that's too much,
/// - an ack past everything sent (a long outage) is adopted,
/// - and the server skips a backlog that runs too far ahead.
#[derive(Resource, Debug)]
pub struct CommandClock {
    /// Applied pace: fixed ticks per real timestep.
    pub(crate) rate: f64,
    /// The pace the reported lead asks for: `rate` slews toward it.
    pub(crate) target_rate: f64,
    /// The newest lead the server reported.
    pub(crate) last_lead: Option<InputLead>,
    /// What the clock has done to the command stream so far.
    pub(crate) stats: CommandClockStats,
    /// Slow-down time still to take out of the fixed clock. Bevy's overstep can't go negative,
    /// and right after a tick it can be smaller than one frame's slice: what doesn't fit before
    /// the loop comes out after it (`settle_fixed_clock`), so the drawn fraction never stutters.
    debt: Duration,
    /// Lead-resync catch-up ticks to run next frame.
    pending_catchup: u32,
    /// Catch-up ticks queued into the fixed clock but not yet run: they
    /// stand for no real time, so the remote clock skips them.
    burst_ticks: u32,
    /// After a resync every lead sample the server holds predates it: hold off until the ack
    /// passes `.0` (a window past the resync) or the server reaches tick `.1` without it.
    cooldown: Option<(u32, u64)>,
    last_server_tick: u64,
    /// [`INPUT_LEAD_WINDOW_SECONDS`] in ticks.
    pub(super) lead_window: u64,
    /// [`INPUT_LEAD_RESYNC_BEHIND_SECONDS`] in ticks, at least
    /// [`INPUT_LEAD_RESYNC_BEHIND_MIN_TICKS`].
    pub(super) resync_behind: i32,
}

/// Running totals of the [`CommandClock`]'s resyncs, for logs and HUDs.
#[derive(Debug, Default, Clone, Copy)]
pub struct CommandClockStats {
    /// Resyncs of every kind: catch-ups, skips and adopted acks.
    pub resyncs: u32,
    /// Extra ticks simulated to catch up.
    pub catchup_ticks: u32,
    /// Sequence numbers jumped over, never stepped.
    pub skipped_sequences: u32,
    /// Ticks handed back to the fixed clock after frames Bevy clamped.
    pub hitch_recovered_ticks: f64,
}

/// What one owner snapshot asks of the command stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CommandClockAction {
    /// Nothing to do: the stream is on time.
    None,
    /// The server acked this sequence though it was never sent (an outage retired the unsent
    /// guesses): number on after it, from the state the snapshot just restored.
    AdoptAck(u32),
    /// Behind by more than a catch-up can simulate: skip this many
    /// sequence numbers. The server already stepped those slots on guesses.
    Skip(u32),
    /// Behind: simulate this many extra ticks next frame. Their commands
    /// land late, replace the server's guesses with the same held input,
    /// and the stream is back on time.
    CatchUp(u32),
}

impl CommandClock {
    /// The pace the fixed clock runs at: fixed ticks per real timestep, a few percent either
    /// side of 1 while it nudges the lead.
    pub fn rate(&self) -> f64 {
        self.rate
    }

    /// The newest arrival lead the server reported, if any.
    pub fn last_lead(&self) -> Option<InputLead> {
        self.last_lead
    }

    /// What the clock has done to the command stream so far.
    pub fn stats(&self) -> CommandClockStats {
        self.stats
    }

    /// A clock at its natural pace, for a server stepping at `rate`.
    pub(crate) fn new(rate: TickRate) -> Self {
        Self::with_windows(
            rate.ticks(INPUT_LEAD_WINDOW_SECONDS),
            i32::try_from(rate.ticks(INPUT_LEAD_RESYNC_BEHIND_SECONDS))
                .unwrap_or(i32::MAX)
                .max(INPUT_LEAD_RESYNC_BEHIND_MIN_TICKS),
        )
    }

    /// Back to `new` at the same rate.
    pub(crate) fn reset(&mut self) {
        *self = Self::with_windows(self.lead_window, self.resync_behind);
    }

    fn with_windows(lead_window: u64, resync_behind: i32) -> Self {
        Self {
            rate: 1.0,
            target_rate: 1.0,
            last_lead: None,
            stats: CommandClockStats::default(),
            debt: Duration::ZERO,
            pending_catchup: 0,
            burst_ticks: 0,
            cooldown: None,
            last_server_tick: 0,
            lead_window,
            resync_behind,
        }
    }

    /// Judge one owner snapshot, once per new server tick: its `server_tick`, its `ack`
    /// ([`OwnerSnapshot::last_processed_sequence`]) and its `input_lead`, against the
    /// `next_sequence` the client numbers from. Retargets the nudge, or returns the hard resync
    /// the stream needs; the caller applies it (`apply`).
    pub(crate) fn observe(
        &mut self,
        server_tick: u64,
        ack: u32,
        input_lead: Option<InputLead>,
        next_sequence: u32,
    ) -> CommandClockAction {
        if ack != 0 && sequence_is_newer(ack, next_sequence) {
            self.target_rate = 1.0;
            self.begin_cooldown(ack, server_tick);
            return CommandClockAction::AdoptAck(ack);
        }
        if let Some((until_ack, until_tick)) = self.cooldown {
            let acked_past = ack == until_ack || sequence_is_newer(ack, until_ack);
            if !acked_past && server_tick < until_tick {
                self.target_rate = 1.0;
                return CommandClockAction::None;
            }
            self.cooldown = None;
        }
        let Some(lead) = input_lead else {
            self.target_rate = 1.0;
            return CommandClockAction::None;
        };
        self.last_lead = Some(lead);

        let max = i32::from(lead.max);
        if max <= -self.resync_behind
            && u64::from(lead.samples) >= self.lead_window / 2
            && let Some(deficit) = u32::try_from(INPUT_LEAD_TARGET - max)
                .ok()
                .filter(|&deficit| deficit > 0)
        {
            self.target_rate = 1.0;
            self.begin_cooldown(next_sequence.wrapping_add(deficit), server_tick);
            return if deficit <= COMMAND_CLOCK_CATCHUP_MAX_TICKS {
                CommandClockAction::CatchUp(deficit)
            } else {
                CommandClockAction::Skip(deficit)
            };
        }

        let (low, high) = INPUT_LEAD_BAND;
        let min = i32::from(lead.min);
        let nudge = if min < low {
            COMMAND_CLOCK_GAIN * f64::from(low - min)
        } else if min > high {
            COMMAND_CLOCK_GAIN * f64::from(high - min)
                - COMMAND_CLOCK_DRAIN_GAIN * f64::from(high - low)
        } else {
            -COMMAND_CLOCK_DRAIN_GAIN * f64::from(min - low)
        };
        self.target_rate =
            1.0 + nudge.clamp(-COMMAND_CLOCK_MAX_SLOWDOWN, COMMAND_CLOCK_MAX_SPEEDUP);
        CommandClockAction::None
    }

    /// Apply a resync to the command stream.
    pub(crate) fn apply(&mut self, action: CommandClockAction, input: &mut ClientInputState) {
        match action {
            CommandClockAction::None => return,
            CommandClockAction::AdoptAck(ack) => {
                self.stats.skipped_sequences += ack.wrapping_sub(input.next_sequence);
                input.next_sequence = ack;
                input.previous_sequence = ack;
            }
            CommandClockAction::Skip(count) => {
                self.stats.skipped_sequences += count;
                input.next_sequence = input.next_sequence.wrapping_add(count);
            }
            CommandClockAction::CatchUp(count) => {
                self.stats.catchup_ticks += count;
                self.pending_catchup += count;
            }
        }
        self.stats.resyncs += 1;
        info!(
            "command clock resync: {action:?} (lead {:?}), next sequence {}",
            self.last_lead, input.next_sequence
        );
    }

    fn begin_cooldown(&mut self, from_sequence: u32, server_tick: u64) {
        self.cooldown = Some((
            from_sequence.wrapping_add(self.lead_window as u32),
            server_tick + 4 * self.lead_window,
        ));
    }

    /// Move the applied pace toward the target, at most `COMMAND_CLOCK_RATE_SLEW` per second of
    /// `real_delta`.
    pub(crate) fn slew(&mut self, real_delta: f64) {
        let step = COMMAND_CLOCK_RATE_SLEW * real_delta.max(0.0);
        self.rate += (self.target_rate - self.rate).clamp(-step, step);
    }

    /// Feed one frame into the fixed clock: the pace, frames Bevy clamped,
    /// and queued catch-up. `lost` is real time `Time<Virtual>` dropped this
    /// frame (scaled by its relative speed).
    pub(crate) fn pace(
        &mut self,
        fixed: &mut Time<Fixed>,
        virtual_delta: Duration,
        lost: Duration,
    ) {
        let timestep = fixed.timestep();
        let cap = timestep * COMMAND_CLOCK_CATCHUP_MAX_TICKS;
        let recovered = lost.min(cap);
        self.stats.hitch_recovered_ticks += recovered.as_secs_f64() / timestep.as_secs_f64();
        let extra = virtual_delta.as_secs_f64() * (self.rate - 1.0) + recovered.as_secs_f64()
            - self.debt.as_secs_f64();
        self.debt = Duration::ZERO;
        if extra >= 0.0 {
            fixed.accumulate_overstep(Duration::from_secs_f64(extra));
        } else {
            self.debt = Duration::from_secs_f64(-extra);
            self.settle(fixed);
        }
        if self.pending_catchup > 0 {
            fixed.accumulate_overstep(timestep * self.pending_catchup);
            self.burst_ticks += self.pending_catchup;
            self.pending_catchup = 0;
        }
    }

    /// Take outstanding slow-down out of whatever overstep there is.
    pub(crate) fn settle(&mut self, fixed: &mut Time<Fixed>) {
        let take = self.debt.min(fixed.overstep());
        fixed.discard_overstep(take);
        self.debt -= take;
    }

    /// How much real time the fixed tick now running stands for, in ticks.
    pub(crate) fn remote_tick_scale(&mut self) -> f64 {
        if self.burst_ticks > 0 {
            self.burst_ticks -= 1;
            0.0
        } else {
            1.0 / self.rate
        }
    }
}

/// Before the fixed loop: slew the pace and feed this frame into
/// `Time<Fixed>` (see `CommandClock::pace`).
pub(crate) fn pace_fixed_clock(
    real: Res<Time<Real>>,
    virtual_time: Res<Time<Virtual>>,
    mut fixed: ResMut<Time<Fixed>>,
    mut clock: ResMut<CommandClock>,
) {
    clock.slew(real.delta_secs_f64());
    // A frame longer than max_delta is clamped and the rest of it dropped: ticks the server
    // steps without this client. Hand them back to the fixed clock.
    let lost = if virtual_time.is_paused() {
        Duration::ZERO
    } else {
        real.delta()
            .saturating_sub(virtual_time.max_delta())
            .mul_f64(virtual_time.relative_speed_f64())
    };
    clock.pace(&mut fixed, virtual_time.delta(), lost);
}

/// After the fixed loop: settle slow-down the overstep couldn't cover
/// before it (see `CommandClock::settle`), before anything renders.
pub(crate) fn settle_fixed_clock(mut fixed: ResMut<Time<Fixed>>, mut clock: ResMut<CommandClock>) {
    clock.settle(&mut fixed);
}

pub(super) fn steer_command_clock<G: PlayerGame>(
    local: Res<OwnPlayerId>,
    server_players: Query<(&PlayerId, &OwnerSnapshot<G>), With<ServerPlayer>>,
    predictions: Query<&PredictedPlayer>,
    mut input_state: ResMut<ClientInputState>,
    mut clock: ResMut<CommandClock>,
) {
    let Some(local_id) = local.0 else {
        return;
    };
    let Ok(prediction) = predictions.single() else {
        return;
    };
    let Ok((player_id, snapshot)) = server_players.get(prediction.server_entity) else {
        return;
    };
    if *player_id != local_id || snapshot.server_tick <= clock.last_server_tick {
        return;
    }
    clock.last_server_tick = snapshot.server_tick;
    let action = clock.observe(
        snapshot.server_tick,
        snapshot.last_processed_sequence,
        snapshot.input_lead,
        input_state.next_sequence,
    );
    clock.apply(action, &mut input_state);
}

#[cfg(test)]
mod render;

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: TickRate = TickRate::new(20);

    /// A full window's lead, between `min` and `max`.
    fn lead(min: i16, max: i16) -> Option<InputLead> {
        Some(InputLead {
            min,
            max,
            samples: RATE.ticks(INPUT_LEAD_WINDOW_SECONDS) as u16,
        })
    }

    #[test]
    fn command_clock_drains_inside_the_band_and_nudges_outside() {
        let mut clock = CommandClock::new(RATE);
        let (low, high) = INPUT_LEAD_BAND;
        let mut at = |min: i32| {
            let action = clock.observe(1, 90, lead(min as i16, min as i16 + 1), 100);
            assert_eq!(action, CommandClockAction::None);
            clock.target_rate
        };
        assert_eq!(at(low), 1.0);
        let drain = 1.0 - at(high);
        assert!(
            drain > 0.0,
            "inside the band: drift down to its bottom edge"
        );
        assert!(
            drain < COMMAND_CLOCK_GAIN,
            "inside the band: far gentler than outside"
        );
        assert!(
            1.0 - at(high + 1) > drain,
            "the drain carries on past the band"
        );
        assert!(at(low - 1) > 1.0, "late: speed up");
        assert!(at(high + 1) < 1.0, "early: slow down");
        assert!(at(-5) <= 1.0 + COMMAND_CLOCK_MAX_SPEEDUP);
        assert!(at(100) >= 1.0 - COMMAND_CLOCK_MAX_SLOWDOWN);
    }

    #[test]
    fn command_clock_resyncs_then_waits_for_fresh_evidence() {
        let mut clock = CommandClock::new(RATE);
        let mut input = ClientInputState {
            next_sequence: 100,
            previous_sequence: 100,
            ..default()
        };

        // Behind by a catch-up's worth: simulate the deficit.
        let action = clock.observe(1, 90, lead(-12, -10), 100);
        assert_eq!(
            action,
            CommandClockAction::CatchUp((INPUT_LEAD_TARGET + 10) as u32)
        );
        clock.apply(action, &mut input);

        // The window still holds pre-resync samples: no second resync until
        // the ack passes the resync point plus a window.
        let action = clock.observe(2, 95, lead(-12, -10), 100);
        assert_eq!(action, CommandClockAction::None);
        let past = 100 + 11 + RATE.ticks(INPUT_LEAD_WINDOW_SECONDS) as u32;
        let action = clock.observe(3, past, lead(-50, -40), past + 2);
        assert_eq!(
            action,
            CommandClockAction::Skip((INPUT_LEAD_TARGET + 40) as u32)
        );
        clock.apply(action, &mut input);
        assert_eq!(input.next_sequence, 141);
        assert_eq!(
            input.previous_sequence, 100,
            "the sweep covers the skipped gap"
        );

        // The server acked past everything sent: number on from the ack, cooldown or not.
        let action = clock.observe(4, 5_000, None, input.next_sequence);
        assert_eq!(action, CommandClockAction::AdoptAck(5_000));
        clock.apply(action, &mut input);
        assert_eq!(
            (input.next_sequence, input.previous_sequence),
            (5_000, 5_000)
        );
    }

    /// A client 200 ticks behind, past what a byte of lead could say: one skip of the whole
    /// deficit, and no second one on the same window. Far past the wire's range the lead and the
    /// skip saturate, never wrap.
    #[test]
    fn a_deficit_past_127_ticks_resyncs_in_one_skip() {
        let mut clock = CommandClock::new(RATE);
        let mut input = ClientInputState {
            next_sequence: 100,
            previous_sequence: 100,
            ..default()
        };
        let action = clock.observe(1, 90, lead(-210, -200), 100);
        assert_eq!(
            action,
            CommandClockAction::Skip((INPUT_LEAD_TARGET + 200) as u32)
        );
        clock.apply(action, &mut input);
        assert_eq!(input.next_sequence, 301);
        assert_eq!(
            clock.observe(2, 95, lead(-210, -200), 301),
            CommandClockAction::None
        );

        let mut clock = CommandClock::new(RATE);
        assert_eq!(
            clock.observe(1, 90, lead(i16::MIN, i16::MIN), 100),
            CommandClockAction::Skip((INPUT_LEAD_TARGET - i32::from(i16::MIN)) as u32)
        );
    }

    /// A full window whose freshest arrival was early, just in time or late short of the
    /// threshold, however late its worst, only nudges, at 10, 12, 15 or 20 Hz; one at the
    /// threshold catches up the deficit.
    #[test]
    fn an_on_time_client_never_resyncs_at_any_rate() {
        assert_eq!(CommandClock::new(RATE).resync_behind, 6);
        for hz in [10, 12, 15, 20] {
            let rate = TickRate::new(hz);
            let full = |min: i16, max: i16| {
                Some(InputLead {
                    min,
                    max,
                    samples: rate.ticks(INPUT_LEAD_WINDOW_SECONDS) as u16,
                })
            };
            let behind = CommandClock::new(rate).resync_behind;
            assert!(
                behind >= INPUT_LEAD_RESYNC_BEHIND_MIN_TICKS,
                "{hz} Hz: threshold {behind}"
            );
            for max in 1 - behind as i16..=INPUT_LEAD_TARGET as i16 {
                let mut clock = CommandClock::new(rate);
                assert_eq!(
                    clock.observe(1, 90, full(max - 3, max), 100),
                    CommandClockAction::None,
                    "{hz} Hz, lead {max}"
                );
                assert!(clock.target_rate > 1.0, "{hz} Hz: late jitter speeds up");
            }
            let max = -behind as i16;
            assert_eq!(
                CommandClock::new(rate).observe(1, 90, full(max - 3, max), 100),
                CommandClockAction::CatchUp((INPUT_LEAD_TARGET + behind) as u32),
                "{hz} Hz"
            );
        }
    }

    /// Both resyncs wait past the lead the whole nudge holds a drifting clock at: by the
    /// server's re-anchor floor the client slows down all it can, and a tick before the client's
    /// floor it already speeds up all it can.
    #[test]
    fn the_resync_floors_sit_past_the_whole_nudge() {
        let mut clock = CommandClock::new(RATE);
        let mut pace_at = |lead: i32| {
            let action = clock.observe(1, 90, self::lead(lead as i16, lead as i16), 100);
            assert_eq!(action, CommandClockAction::None, "lead {lead}");
            clock.target_rate
        };
        assert_eq!(
            pace_at(crate::server::INPUT_LEAD_RESYNC_AHEAD_MIN_TICKS),
            1.0 - COMMAND_CLOCK_MAX_SLOWDOWN
        );
        assert_eq!(
            pace_at(1 - INPUT_LEAD_RESYNC_BEHIND_MIN_TICKS),
            1.0 + COMMAND_CLOCK_MAX_SPEEDUP
        );
    }

    #[test]
    fn slow_down_never_stalls_the_overstep() {
        let mut clock = CommandClock {
            rate: 0.96,
            target_rate: 0.96,
            ..CommandClock::new(RATE)
        };
        let mut fixed = Time::<Fixed>::from_hz(f64::from(RATE.hz()));
        let frame = Duration::from_micros(6_944);
        // Leftover smaller than one frame's slow-down slice: take what's
        // there now, the rest after the loop, none lost.
        fixed.accumulate_overstep(Duration::from_micros(100));
        clock.pace(&mut fixed, frame, Duration::ZERO);
        assert_eq!(fixed.overstep(), Duration::ZERO);
        fixed.accumulate_overstep(frame);
        clock.settle(&mut fixed);
        let expected = Duration::from_micros(100) + frame.mul_f64(0.96);
        let error = fixed.overstep().abs_diff(expected);
        assert!(error < Duration::from_nanos(10), "{error:?}");
    }
}
