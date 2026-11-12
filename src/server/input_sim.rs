//! Recovery simulation for the input clock: the real server timeline (`run_player_tick`) and
//! the real client [`CommandClock`] over a scripted TCP-like link, with a toy integrator for the
//! pmove. Each scenario breaks something (lost ticks, outages, a hidden tab, a server hitch,
//! clock drift) and asserts the stream is healthy again within a bounded time: the server steps
//! real input on its tick, not a guess, and acks stay fresh.

use std::{collections::VecDeque, time::Duration};

use bevy::prelude::*;

use crate::{
    client::{
        COMMAND_CLOCK_MAX_SLOWDOWN, COMMAND_CLOCK_MAX_SPEEDUP, ClientInputState, CommandClock,
        CommandClockAction,
    },
    events::Outbox,
    game::{
        PlayerState,
        test_game::{TestEvent, TestGame},
    },
    protocol::*,
    server::{
        QueuedUserCmds, RollbackStats, ServerCommandBuffer, TimelineStepper, run_player_tick,
    },
    stepper::PredictionFrame,
};

const RATE: TickRate = TickRate::new(20);
const TICK_MS: f64 = 1000.0 / RATE.hz() as f64;
/// Bevy's default `Time<Virtual>::max_delta`.
const MAX_DELTA_MS: f64 = 250.0;
const CLIENT_FRAME_MS: f64 = 1000.0 / 60.0;

/// The pmove, reduced to "every step moves you along x by your input".
#[derive(Default)]
struct ToyStepper {
    position: Vec3,
    velocity: Vec3,
    rate: TickRate,
}

impl TimelineStepper<TestGame> for ToyStepper {
    fn capture_frame(&mut self, _: Entity, command: UserCmd) -> Option<PredictionFrame<TestGame>> {
        let mut state = PlayerState::default();
        state.moves.position = self.position;
        state.moves.velocity = self.velocity;
        Some(PredictionFrame { command, state })
    }

    fn restore_frame(&mut self, _: Entity, frame: &PredictionFrame<TestGame>) {
        self.position = frame.position();
        self.velocity = frame.velocity();
    }

    fn player_move(
        &mut self,
        _: Entity,
        _: &mut Outbox<TestEvent>,
        command: UserCmd,
        _: u32,
        _: Buttons,
    ) -> Result<()> {
        self.velocity = Vec3::new(command.movement.x, 0.0, command.movement.y);
        self.position += self.velocity / self.rate.hz() as f32;
        Ok(())
    }
}

/// In-order, delayed delivery. Packets sent during a blackout queue up and
/// arrive after it ends, like TCP holding a stalled stream.
struct Link<T> {
    base_ms: f64,
    jitter_ms: f64,
    /// Per packet: chance it is lost and retransmitted, stalling the stream
    /// behind it for `stall_ms` (TCP head-of-line blocking).
    stall_chance: f64,
    stall_ms: f64,
    blackouts: Vec<(f64, f64)>,
    in_flight: VecDeque<(f64, T)>,
    last_arrival: f64,
}

impl<T> Link<T> {
    fn new(base_ms: f64, jitter_ms: f64) -> Self {
        Self {
            base_ms,
            jitter_ms,
            stall_chance: 0.0,
            stall_ms: 0.0,
            blackouts: Vec::new(),
            in_flight: VecDeque::new(),
            last_arrival: 0.0,
        }
    }

    fn send(&mut self, now: f64, rng: &mut Rng, payload: T) {
        let mut departure = now;
        for &(start, end) in &self.blackouts {
            if (start..end).contains(&now) {
                departure = end;
            }
        }
        let mut arrival = departure + self.base_ms + rng.next_f64() * self.jitter_ms;
        if rng.next_f64() < self.stall_chance {
            arrival += self.stall_ms;
        }
        let arrival = arrival.max(self.last_arrival);
        self.last_arrival = arrival;
        self.in_flight.push_back((arrival, payload));
    }

    fn receive(&mut self, now: f64) -> Option<T> {
        if self.in_flight.front()?.0 <= now {
            self.in_flight.pop_front().map(|(_, payload)| payload)
        } else {
            None
        }
    }
}

struct Rng(u64);

impl Rng {
    fn next_f64(&mut self) -> f64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        (self.0.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 11) as f64 / (1u64 << 53) as f64
    }
}

#[derive(Clone)]
struct Scenario {
    /// The rate both peers step at.
    rate: TickRate,
    seconds: f64,
    one_way_ms: f64,
    jitter_ms: f64,
    /// Client real time runs this much faster than the server's.
    client_drift: f64,
    /// (start, length) in ms: the client renders no frame, then one long one.
    client_hitches: Vec<(f64, f64)>,
    /// (start, length) in ms: both directions stall.
    blackouts: Vec<(f64, f64)>,
    /// (start, length) in ms: the server's frame stalls; Bevy clamps it to
    /// one `max_delta` and the rest of its ticks never run.
    server_hitches: Vec<(f64, f64)>,
    /// TCP retransmit stalls, both directions: (chance per packet, ms).
    stalls: (f64, f64),
    /// Off: the client ignores every lead report and ack.
    steering: bool,
    /// Server re-simulates late commands (`NetAhoyServerPlugin::input_rollback`).
    rollback: bool,
}

impl Default for Scenario {
    fn default() -> Self {
        Self {
            rate: RATE,
            seconds: 60.0,
            one_way_ms: 60.0,
            jitter_ms: 20.0,
            client_drift: 0.0,
            client_hitches: Vec::new(),
            blackouts: Vec::new(),
            server_hitches: Vec::new(),
            stalls: (0.0, 0.0),
            steering: true,
            rollback: true,
        }
    }
}

#[derive(Clone, Copy)]
struct TickSample {
    at_ms: f64,
    /// The slot stepped this tick ran the client's real command.
    real: bool,
    /// Commands the client has produced but the server hasn't acked.
    ack_lag: u32,
    rolled_back: bool,
    /// The slot stepped this tick ran a guessed command.
    guessed: bool,
}

struct Outcome {
    ticks: Vec<TickSample>,
    client_resyncs: u32,
    server_reanchors: u32,
    max_rollback_depth: usize,
    final_lead: Option<InputLead>,
    min_rate: f64,
    max_rate: f64,
    /// Server re-simulations, and the slots they stepped again.
    rollbacks: u32,
    restepped_ticks: u32,
    /// Real commands that arrived for a slot already stepped on a guess.
    late_arrivals: u32,
    /// Sequences the server's re-anchors skipped.
    reanchor_skipped: u32,
}

#[derive(Debug)]
struct Health {
    real_ratio: f64,
    max_ack_lag: u32,
    rollbacks: usize,
}

impl Outcome {
    /// Slots stepped on a guessed command.
    fn guessed(&self) -> usize {
        self.ticks.iter().filter(|tick| tick.guessed).count()
    }

    fn health(&self, from_s: f64, to_s: f64) -> Health {
        let window: Vec<_> = self
            .ticks
            .iter()
            .filter(|tick| (from_s * 1000.0..to_s * 1000.0).contains(&tick.at_ms))
            .collect();
        assert!(!window.is_empty(), "no ticks in {from_s}..{to_s}s");
        Health {
            real_ratio: window.iter().filter(|tick| tick.real).count() as f64 / window.len() as f64,
            max_ack_lag: window.iter().map(|tick| tick.ack_lag).max().unwrap(),
            rollbacks: window.iter().filter(|tick| tick.rolled_back).count(),
        }
    }

    /// Real input on ≥ 97% of ticks and acks within a round trip and change.
    fn assert_healthy(&self, from_s: f64, to_s: f64, label: &str) {
        let health = self.health(from_s, to_s);
        println!(
            "{label:>24} {from_s:>5}..{to_s:<5}s real {:>5.1}% ack lag <= {:>2} rollback ticks {:>4} | resyncs {} re-anchors {} max depth {} pace {:.3}..{:.3} lead {:?}",
            health.real_ratio * 100.0,
            health.max_ack_lag,
            health.rollbacks,
            self.client_resyncs,
            self.server_reanchors,
            self.max_rollback_depth,
            self.min_rate,
            self.max_rate,
            self.final_lead,
        );
        assert!(
            health.real_ratio >= 0.97 && health.max_ack_lag <= 16,
            "{label}: not healthy in {from_s}..{to_s}s: {health:?} (resyncs {}, re-anchors {}, lead {:?})",
            self.client_resyncs,
            self.server_reanchors,
            self.final_lead,
        );
    }
}

fn in_any(spans: &[(f64, f64)], now: f64) -> Option<(f64, f64)> {
    spans
        .iter()
        .copied()
        .find(|&(start, length)| (start..start + length).contains(&now))
}

fn run(scenario: Scenario) -> Outcome {
    let rate = scenario.rate;
    let tick_ms = 1000.0 / f64::from(rate.hz());
    let mut rng = Rng(0x5eed_1234_abcd_9876);
    let player = Entity::PLACEHOLDER;

    let mut uplink: Link<UserCmdPacket> = Link::new(scenario.one_way_ms, scenario.jitter_ms);
    let mut downlink: Link<OwnerSnapshot<TestGame>> =
        Link::new(scenario.one_way_ms, scenario.jitter_ms);
    for &(start, length) in &scenario.blackouts {
        uplink.blackouts.push((start, start + length));
        downlink.blackouts.push((start, start + length));
    }
    (uplink.stall_chance, uplink.stall_ms) = scenario.stalls;
    (downlink.stall_chance, downlink.stall_ms) = scenario.stalls;

    let mut buffer = ServerCommandBuffer::<TestGame>::default();
    let mut queued = QueuedUserCmds::default();
    let mut stats = RollbackStats::default();
    let mut server_stepper = ToyStepper { rate, ..default() };
    let mut server_tick: u64 = 1_000;
    let mut next_server_tick_ms = tick_ms;

    let mut clock = CommandClock::new(rate);
    let mut input = ClientInputState::default();
    let mut fixed = Time::<Fixed>::from_hz(f64::from(rate.hz()));
    let timestep = fixed.timestep();
    let mut last_snapshot_tick = 0;
    let mut next_client_frame_ms = CLIENT_FRAME_MS;
    let mut last_client_frame_ms = 0.0;

    let mut outcome = Outcome {
        ticks: Vec::new(),
        client_resyncs: 0,
        server_reanchors: 0,
        max_rollback_depth: 0,
        final_lead: None,
        min_rate: 1.0,
        max_rate: 1.0,
        rollbacks: 0,
        restepped_ticks: 0,
        late_arrivals: 0,
        reanchor_skipped: 0,
    };

    let end_ms = scenario.seconds * 1000.0;
    let mut now = 0.0;
    while now < end_ms {
        now += 1.0;

        if now >= next_client_frame_ms && in_any(&scenario.client_hitches, now).is_none() {
            let real_delta_ms = (now - last_client_frame_ms) * (1.0 + scenario.client_drift);
            last_client_frame_ms = now;
            next_client_frame_ms = now + CLIENT_FRAME_MS;

            // A replicated component: the client only ever sees the newest.
            let mut newest = None;
            while let Some(snapshot) = downlink.receive(now) {
                newest = Some(snapshot);
            }
            if let Some(snapshot) = newest
                && snapshot.server_tick > last_snapshot_tick
            {
                last_snapshot_tick = snapshot.server_tick;
                if scenario.steering {
                    let action = clock.observe(
                        snapshot.server_tick,
                        snapshot.last_processed_sequence,
                        snapshot.input_lead,
                        input.next_sequence,
                    );
                    if action != CommandClockAction::None {
                        outcome.client_resyncs += 1;
                    }
                    clock.apply(action, &mut input);
                }
            }

            let virtual_delta_ms = real_delta_ms.min(MAX_DELTA_MS);
            let lost = Duration::from_secs_f64((real_delta_ms - virtual_delta_ms) / 1000.0);
            let virtual_delta = Duration::from_secs_f64(virtual_delta_ms / 1000.0);
            if scenario.steering {
                clock.slew(real_delta_ms / 1000.0);
                clock.pace(&mut fixed, virtual_delta, lost);
            }
            if now >= 10_000.0 {
                outcome.min_rate = outcome.min_rate.min(clock.rate);
                outcome.max_rate = outcome.max_rate.max(clock.rate);
            }
            fixed.accumulate_overstep(virtual_delta);
            while fixed.overstep() >= timestep {
                fixed.discard_overstep(timestep);
                clock.remote_tick_scale();
                let command = UserCmd {
                    sequence: input.next_sequence.wrapping_add(1),
                    movement: Vec2::X,
                    ..default()
                };
                input.next_sequence = command.sequence;
                input.previous_sequence = command.sequence;
                uplink.send(
                    now,
                    &mut rng,
                    UserCmdPacket {
                        commands: vec![command],
                    },
                );
            }
        }

        if now >= next_server_tick_ms {
            if let Some((start, length)) = in_any(&scenario.server_hitches, now) {
                // On wake Bevy runs one max_delta of catch-up ticks; the rest never run.
                next_server_tick_ms = next_server_tick_ms.max(start + length - MAX_DELTA_MS);
                continue;
            }
            next_server_tick_ms += tick_ms;
            server_tick += 1;

            while let Some(packet) = uplink.receive(now) {
                queued.push_packet(&packet, buffer.last_processed_sequence, rate);
            }
            outcome.late_arrivals += queued
                .commands
                .iter()
                .filter(|command| {
                    buffer
                        .timeline
                        .iter()
                        .any(|frame| frame.command.sequence == command.sequence && !frame.received)
                })
                .count() as u32;
            let rollbacks_before = stats.rollbacks;
            run_player_tick(
                &mut server_stepper,
                player,
                server_tick,
                &mut buffer,
                &mut queued,
                &mut stats,
                scenario.rollback,
                rate,
            );
            let stepped = buffer
                .timeline
                .back()
                .filter(|frame| frame.server_tick == server_tick);
            outcome.ticks.push(TickSample {
                at_ms: now,
                real: stepped.is_some_and(|frame| frame.received),
                ack_lag: input
                    .next_sequence
                    .wrapping_sub(buffer.last_processed_sequence)
                    .min(10_000),
                rolled_back: stats.rollbacks != rollbacks_before,
                guessed: stepped.is_some_and(|frame| !frame.received),
            });

            let snapshot = OwnerSnapshot {
                server_tick,
                last_processed_sequence: buffer.last_processed_sequence,
                input_lead: buffer
                    .lead
                    .report(server_tick, rate.ticks(INPUT_LEAD_WINDOW_SECONDS)),
                ..default()
            };
            downlink.send(now, &mut rng, snapshot);
        }
    }

    outcome.rollbacks = stats.rollbacks;
    outcome.restepped_ticks = stats.restepped_ticks;
    outcome.server_reanchors = stats.reanchors;
    outcome.reanchor_skipped = stats.reanchor_skipped;
    outcome.max_rollback_depth = stats.max_depth;
    outcome.final_lead = buffer
        .lead
        .report(server_tick, rate.ticks(INPUT_LEAD_WINDOW_SECONDS));
    outcome
}

#[test]
fn steady_link_holds_its_lead_without_resyncs() {
    let outcome = run(Scenario::default());
    outcome.assert_healthy(10.0, 60.0, "steady");
    assert_eq!(outcome.client_resyncs, 0);
    assert_eq!(outcome.server_reanchors, 0);
    let lead = outcome.final_lead.unwrap();
    assert!(
        (-1..=4).contains(&lead.min),
        "lead drifted out of band: {lead:?}"
    );
}

#[test]
fn clock_drift_is_absorbed_by_the_nudge() {
    for drift in [0.01, -0.01] {
        let outcome = run(Scenario {
            seconds: 300.0,
            client_drift: drift,
            ..default()
        });
        // 1% is 3 s of drift over five minutes: without the nudge the client
        // would sit 60 ticks off. Pace alone must hold it, no hard resyncs.
        outcome.assert_healthy(20.0, 300.0, &format!("drift {drift}"));
        assert_eq!(outcome.client_resyncs, 0, "drift {drift}");
        assert_eq!(outcome.server_reanchors, 0, "drift {drift}");
        assert!(
            outcome.min_rate >= 1.0 - COMMAND_CLOCK_MAX_SLOWDOWN - 1e-9
                && outcome.max_rate <= 1.0 + COMMAND_CLOCK_MAX_SPEEDUP + 1e-9
        );
    }
}

#[test]
fn the_slowest_rates_absorb_drift_and_still_shed_a_hitch() {
    // At 10 and 11 Hz the re-anchor's seconds round to a lead the nudge holds a fast clock at;
    // its floor keeps it past that. A clock up to 3% fast or 4% slow for five minutes is only
    // nudged, and a 10-tick server hitch is still shed by one re-anchor.
    for hz in [10, 12, 15] {
        let rate = TickRate::new(hz);
        for drift in [0.02, 0.03, -0.04] {
            let outcome = run(Scenario {
                rate,
                seconds: 300.0,
                client_drift: drift,
                ..default()
            });
            assert_eq!(outcome.client_resyncs, 0, "{hz} Hz, drift {drift}");
            assert_eq!(outcome.server_reanchors, 0, "{hz} Hz, drift {drift}");
            // A clock 4% slow is held a tick late, its every command re-stepped by a rollback,
            // as at 20 Hz.
            if drift > 0.0 {
                outcome.assert_healthy(20.0, 300.0, &format!("{hz} Hz, drift {drift}"));
            }
        }

        // Bevy runs the stall's last max_delta: the server loses 10 ticks, the client doesn't.
        let tick_ms = 1000.0 / f64::from(hz);
        let outcome = run(Scenario {
            rate,
            seconds: 90.0,
            server_hitches: vec![(20_000.0, MAX_DELTA_MS + 10.0 * tick_ms)],
            ..default()
        });
        outcome.assert_healthy(45.0, 90.0, &format!("{hz} Hz after server hitch"));
        assert_eq!(outcome.server_reanchors, 1, "{hz} Hz");
        assert_eq!(
            outcome.reanchor_skipped, 10,
            "{hz} Hz: the ticks the server lost"
        );
        assert_eq!(outcome.client_resyncs, 0, "{hz} Hz");
    }
}

#[test]
fn hitch_inside_the_catchup_budget_recovers_in_the_same_frame() {
    // 0.8 s frame: Bevy keeps 250 ms, the clock hands back the other 550.
    let outcome = run(Scenario {
        client_hitches: vec![(20_000.0, 800.0)],
        ..default()
    });
    outcome.assert_healthy(10.0, 20.0, "before hitch");
    outcome.assert_healthy(22.0, 60.0, "after hitch");
    assert_eq!(outcome.client_resyncs, 0, "no round trip needed");
}

#[test]
fn hitch_past_the_catchup_budget_resyncs() {
    // 3 s frame: 1 s recovered locally, 1.75 s of ticks lost for good. The
    // lead report then shows every command landing ~35 ticks late.
    let outcome = run(Scenario {
        client_hitches: vec![(20_000.0, 3_000.0)],
        ..default()
    });
    outcome.assert_healthy(28.0, 60.0, "after long hitch");
    assert!(outcome.client_resyncs >= 1);
}

#[test]
fn hidden_tab_crosses_the_rollback_window_and_comes_back() {
    // 60 s without a client frame: every guess has left the 64-tick window and been acked.
    // Without steering the client numbers on from where it stopped, every command it sends is a
    // stale duplicate, and the server runs on guesses for good.
    let hidden = Scenario {
        seconds: 120.0,
        client_hitches: vec![(20_000.0, 60_000.0)],
        ..default()
    };
    let outcome = run(hidden.clone());
    outcome.assert_healthy(10.0, 20.0, "before hiding");
    outcome.assert_healthy(85.0, 120.0, "after returning");

    let old = run(Scenario {
        steering: false,
        ..hidden
    });
    let stuck = old.health(85.0, 120.0);
    assert!(
        stuck.real_ratio < 0.1,
        "the old client should still be broken: {stuck:?}"
    );
}

#[test]
fn blackout_recovers_after_the_stall() {
    let outcome = run(Scenario {
        seconds: 90.0,
        blackouts: vec![(20_000.0, 10_000.0)],
        ..default()
    });
    outcome.assert_healthy(10.0, 20.0, "before blackout");
    outcome.assert_healthy(36.0, 90.0, "after blackout");
}

#[test]
fn server_hitch_sheds_the_backlog() {
    // The server loses ~1.75 s of ticks; the client keeps producing, so its
    // commands pile up ~35 ticks ahead of the server's timeline: 1.75 s
    // of input latency until something sheds it.
    let outcome = run(Scenario {
        server_hitches: vec![(20_000.0, 2_000.0)],
        ..default()
    });
    outcome.assert_healthy(25.0, 60.0, "after server hitch");
    assert!(outcome.server_reanchors >= 1);
    let lead = outcome.final_lead.unwrap();
    assert!(lead.max <= 6, "backlog not shed: {lead:?}");
}

#[test]
fn bad_link_stays_bounded() {
    // Transatlantic-and-then-some: heavy jitter plus a retransmit stall of
    // two RTTs on 2% of packets. Rollbacks are expected; resync storms and
    // a stuck stream are not — a stall's catch-up burst carries a fresh
    // head, so it must never read as "behind".
    let outcome = run(Scenario {
        seconds: 180.0,
        one_way_ms: 120.0,
        jitter_ms: 120.0,
        stalls: (0.02, 480.0),
        ..default()
    });
    let health = outcome.health(20.0, 180.0);
    println!(
        "bad link: {health:?} resyncs {} re-anchors {} max depth {}",
        outcome.client_resyncs, outcome.server_reanchors, outcome.max_rollback_depth
    );
    // Ticks behind a stall run on guesses and roll back when the burst lands: that share is the
    // link's, not the clock's. The clock must never mistake a stall for being behind, and must
    // keep every rollback inside one stall.
    assert_eq!(outcome.client_resyncs, 0, "a stall read as lateness");
    assert_eq!(
        outcome.server_reanchors, 0,
        "a stall's burst read as a backlog"
    );
    let stall_ticks = (480.0 / TICK_MS) as usize;
    assert!(
        outcome.max_rollback_depth <= stall_ticks + 4,
        "rollback deeper than a stall: {}",
        outcome.max_rollback_depth
    );
    assert!(
        health.real_ratio >= 0.8 && health.max_ack_lag <= 24,
        "bad link: {health:?} (lead {:?})",
        outcome.final_lead
    );
}

#[test]
fn rollback_off_never_resimulates_and_acks_through_stalls() {
    // The bad link again, rollback on vs off. Off: the same stalls make the
    // server guess, but late commands are dropped (never re-stepped) and the
    // ack still crosses every guessed slot within a stall.
    let bad = Scenario {
        seconds: 120.0,
        one_way_ms: 120.0,
        jitter_ms: 120.0,
        stalls: (0.02, 480.0),
        ..default()
    };
    let on = run(bad.clone());
    assert!(on.rollbacks > 0 && on.restepped_ticks >= on.rollbacks);
    assert!(on.late_arrivals > 0);
    assert!(on.guessed() > 0);

    let off = run(Scenario {
        rollback: false,
        ..bad
    });
    let health = off.health(20.0, 120.0);
    println!(
        "rollback off: {health:?} rollbacks {} restepped {} guessed {} late {} (on: rollbacks {} restepped {} guessed {} late {})",
        off.rollbacks,
        off.restepped_ticks,
        off.guessed(),
        off.late_arrivals,
        on.rollbacks,
        on.restepped_ticks,
        on.guessed(),
        on.late_arrivals,
    );
    assert_eq!(off.rollbacks, 0);
    assert_eq!(off.restepped_ticks, 0);
    assert_eq!(health.rollbacks, 0);
    // Late commands arrived and none was re-stepped: each was dropped.
    assert!(off.guessed() > 0 && off.late_arrivals > 0);
    let stall_ticks = (480.0 / TICK_MS) as u32;
    assert!(
        health.max_ack_lag <= 24 + stall_ticks,
        "ack stuck behind dropped commands: {health:?}"
    );
}
