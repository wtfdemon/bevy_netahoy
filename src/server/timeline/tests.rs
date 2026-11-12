use super::*;
use crate::{
    game::{
        PlayerState,
        test_game::{TestEvent, TestGame},
    },
    server::intake::queue_commands,
};

const RATE: TickRate = TickRate::new(20);

/// [`INPUT_LEAD_WINDOW_SECONDS`] in ticks.
fn window() -> u64 {
    RATE.ticks(INPUT_LEAD_WINDOW_SECONDS)
}

fn prediction_frame(command: UserCmd) -> PredictionFrame<TestGame> {
    PredictionFrame {
        command,
        state: PlayerState::default(),
    }
}

fn input_frame(sequence: u32, received: bool) -> ServerInputFrame<TestGame> {
    let command = UserCmd {
        sequence,
        ..default()
    };
    // `before` is the state entering the slot, `after` the state it
    // stepped to: x = sequence - 1 and x = sequence.
    let mut before = prediction_frame(command);
    before.state.moves.position = Vec3::X * (sequence as f32 - 1.0);
    let mut after = prediction_frame(command);
    after.state.moves.position = Vec3::X * sequence as f32;
    ServerInputFrame {
        server_tick: u64::from(sequence),
        command,
        received,
        previous_sequence: sequence.saturating_sub(1),
        previous_buttons: Buttons::empty(),
        before,
        after,
        flags: RemoteFlags::empty(),
    }
}

#[test]
fn stale_guesses_retire_and_acknowledge() {
    let mut buffer = ServerCommandBuffer::default();
    buffer.timeline.extend([
        input_frame(10, true),
        input_frame(11, false),
        input_frame(12, true),
    ]);
    advance_confirmation(&mut buffer);
    assert_eq!(buffer.last_processed_sequence, 10);

    // Nothing newer than the guess has arrived: the client may simply be
    // behind (hitch, slow clock), so the slot waits indefinitely.
    buffer.newest_received_sequence = 10;
    assert!(!retire_stale_guesses(&mut buffer, 100, RATE));
    assert!(!buffer.timeline[1].received);

    // A newer command made it, so 11 is late or lost. Still inside the
    // wait: the slot stays replaceable.
    buffer.newest_received_sequence = 12;
    let wait = RATE.ticks(GUESS_RETIRE_SECONDS);
    assert!(!retire_stale_guesses(&mut buffer, 11 + wait - 1, RATE));
    assert!(!buffer.timeline[1].received);
    assert_eq!(buffer.last_processed_sequence, 10);

    // Waited long enough: the guess is the input that ran, ack crosses it.
    assert!(retire_stale_guesses(&mut buffer, 11 + wait, RATE));
    assert!(buffer.timeline[1].received);
    assert_eq!(buffer.last_processed_sequence, 12);
    assert!(
        !retire_stale_guesses(&mut buffer, 100, RATE),
        "nothing left to retire"
    );
}

#[test]
fn external_edit_floors_the_rollback_window() {
    let mut buffer = ServerCommandBuffer::default();
    buffer.timeline.extend([
        input_frame(10, true),
        input_frame(11, false),
        input_frame(12, true),
    ]);
    advance_confirmation(&mut buffer);
    assert_eq!(buffer.last_processed_sequence, 10);

    // Untouched between steps: not an edit, the guess stays replaceable.
    let untouched = buffer.timeline[2].after;
    assert!(!externally_edited(&buffer.timeline[2].after, &untouched));

    // A teleport lands: everything behind it is final.
    let mut teleported = untouched;
    teleported.state.moves.position = Vec3::new(0.0, 2.2, 8.0);
    assert!(externally_edited(&buffer.timeline[2].after, &teleported));
    assert!(floor_rollback(&mut buffer));
    assert!(buffer.timeline[1].received);
    assert_eq!(buffer.last_processed_sequence, 12);
    assert!(!floor_rollback(&mut buffer), "nothing left to floor");
}

#[test]
fn owner_snapshot_is_the_state_before_the_first_guess() {
    let mut buffer = ServerCommandBuffer::default();
    buffer.timeline.extend([
        input_frame(10, true),
        input_frame(11, false),
        input_frame(12, true),
        input_frame(13, false),
    ]);
    advance_confirmation(&mut buffer);
    assert_eq!(buffer.last_processed_sequence, 10);
    // The state entering slot 11 = the state after the acked command 10.
    let confirmed = confirmed_frame(&buffer).expect("a guessed slot exists");
    assert_eq!(confirmed.position(), Vec3::X * 10.0);

    // Every slot real: the live head is the verdict.
    buffer.timeline[1].received = true;
    buffer.timeline[3].received = true;
    assert!(confirmed_frame(&buffer).is_none());
}

/// At 20 Hz the history and the rollback timeline hold 64 ticks, the queue and the replay 50
/// and the re-anchor waits for 8, and three times as many at 60 Hz, but the rollback stops at
/// its 64 and the queue and the replay at their 50; the queue keeps the newest of a flood. At
/// 10 Hz the queue and the replay hold their 25 and the re-anchor waits for its floor's 5, not
/// the 4 its seconds round to.
#[test]
fn the_server_windows_hold_their_seconds_up_to_their_caps() {
    let slow = TickRate::new(10);
    assert_eq!(queue_commands(slow), 25);
    assert_eq!(crate::client::max_replay_commands(slow), 25);
    assert_eq!(slow.ticks(INPUT_LEAD_RESYNC_AHEAD_SECONDS), 4);
    assert_eq!(resync_ahead_ticks(slow), INPUT_LEAD_RESYNC_AHEAD_MIN_TICKS);
    assert_eq!(INPUT_LEAD_RESYNC_AHEAD_MIN_TICKS, 5);
    for (rate, scale) in [(RATE, 1), (TickRate::new(60), 3)] {
        assert_eq!(rate.ticks(HISTORY_SECONDS), 64 * scale);
        assert_eq!(rate.ticks(SERVER_INPUT_ROLLBACK_SECONDS), 64 * scale);
        assert_eq!(
            rate.capped_ticks(
                SERVER_INPUT_ROLLBACK_SECONDS,
                SERVER_INPUT_ROLLBACK_MAX_TICKS
            ),
            64
        );
        assert_eq!(rate.ticks(INPUT_LEAD_RESYNC_AHEAD_SECONDS), 8 * scale);
        let queue = queue_commands(rate);
        let replay = crate::client::max_replay_commands(rate);
        assert_eq!(queue, 50);
        assert_eq!(replay, queue, "the client replays as deep as it queues");

        let flood = UserCmdPacket {
            commands: (1..=400)
                .map(|sequence| UserCmd {
                    sequence,
                    ..default()
                })
                .collect(),
        };
        let mut queued = QueuedUserCmds::default();
        queued.push_packet(&flood, 0, rate);
        let kept = 50;
        assert_eq!(queued.commands.len() as u32, kept);
        assert_eq!(queued.commands.front().unwrap().sequence, 401 - kept);
    }
}

/// Steps nothing, counts nothing: the timeline's bookkeeping alone.
struct NoStepper;

impl TimelineStepper<TestGame> for NoStepper {
    fn capture_frame(&mut self, _: Entity, command: UserCmd) -> Option<PredictionFrame<TestGame>> {
        Some(prediction_frame(command))
    }

    fn restore_frame(&mut self, _: Entity, _: &PredictionFrame<TestGame>) {}

    fn player_move(
        &mut self,
        _: Entity,
        _: &mut Outbox<TestEvent>,
        _: UserCmd,
        _: u32,
        _: Buttons,
    ) -> Result<()> {
        Ok(())
    }
}

/// Steps nothing, and keeps each command it was asked to step with the one before it.
#[derive(Default)]
struct Recorder(Vec<(u32, u32)>);

impl TimelineStepper<TestGame> for Recorder {
    fn capture_frame(&mut self, _: Entity, command: UserCmd) -> Option<PredictionFrame<TestGame>> {
        Some(prediction_frame(command))
    }

    fn restore_frame(&mut self, _: Entity, _: &PredictionFrame<TestGame>) {}

    fn player_move(
        &mut self,
        _: Entity,
        _: &mut Outbox<TestEvent>,
        command: UserCmd,
        previous_sequence: u32,
        _: Buttons,
    ) -> Result<()> {
        self.0.push((command.sequence, previous_sequence));
        Ok(())
    }
}

/// A new timeline's first tick steps every queued command, oldest first, a slot a sequence; a
/// hole runs on a guess, and its command, landing late, rolls back as anywhere else. The next
/// tick follows the newest: the burst leaves the lead one command a tick would have.
#[test]
fn a_join_burst_steps_a_slot_a_sequence_from_the_oldest() {
    let mut buffer = ServerCommandBuffer::<TestGame>::default();
    let mut queued = QueuedUserCmds::default();
    let mut stats = RollbackStats::default();
    let mut stepper = Recorder::default();
    let packet = |sequences: &[u32]| UserCmdPacket {
        commands: sequences
            .iter()
            .map(|&sequence| UserCmd {
                sequence,
                ..default()
            })
            .collect(),
    };
    let mut tick = |stepper: &mut Recorder, queued: &mut QueuedUserCmds, tick| {
        run_player_tick(
            stepper,
            Entity::PLACEHOLDER,
            tick,
            &mut buffer,
            queued,
            &mut stats,
            true,
            RATE,
        );
        (
            buffer.last_processed_sequence,
            buffer.lead.report(tick, window()),
        )
    };

    // 3 is lost on the way.
    queued.push_packet(&packet(&[1, 2, 4, 5]), 0, RATE);
    let (ack, lead) = tick(&mut stepper, &mut queued, 100);
    assert_eq!(stepper.0, [(1, 0), (2, 1), (3, 2), (4, 3), (5, 4)]);
    assert_eq!(ack, 2, "the guess on 3 holds the ack");
    assert_eq!(lead, None, "the burst is no lead sample");
    assert!(queued.commands.is_empty());

    // 3 lands with 6: it replaces its guess, and 6 is the next slot, just in time.
    stepper.0.clear();
    queued.push_packet(&packet(&[3, 6]), ack, RATE);
    let (ack, lead) = tick(&mut stepper, &mut queued, 101);
    assert_eq!(stepper.0, [(3, 2), (4, 3), (5, 4), (6, 5)]);
    assert_eq!(ack, 6);
    assert_eq!(
        lead,
        Some(InputLead {
            min: 0,
            max: 0,
            samples: 1
        })
    );
    assert_eq!(stats.rollbacks, 1);
}

/// The first tick of a new timeline on `queued` at `rate`: what it stepped, as (sequence, previous
/// sequence), the ack it left, and the commands still queued.
fn first_tick(
    rate: TickRate,
    queued: &mut QueuedUserCmds,
) -> (Vec<(u32, u32)>, u32, VecDeque<UserCmd>) {
    let mut buffer = ServerCommandBuffer::<TestGame>::default();
    let mut stepper = Recorder::default();
    run_player_tick(
        &mut stepper,
        Entity::PLACEHOLDER,
        100,
        &mut buffer,
        queued,
        &mut RollbackStats::default(),
        true,
        rate,
    );
    let left = std::mem::take(&mut queued.commands);
    (stepper.0, buffer.last_processed_sequence, left)
}

fn commands(sequences: impl IntoIterator<Item = u32>) -> Vec<UserCmd> {
    sequences
        .into_iter()
        .map(|sequence| UserCmd {
            sequence,
            ..default()
        })
        .collect()
}

/// Each sequence from `first` to `last` across the wrap, with the one before it: a chain of
/// slots, the first after none.
fn chain(first: u32, last: u32) -> Vec<(u32, u32)> {
    let mut slots = vec![(first, 0)];
    while slots.last().unwrap().0 != last {
        let previous = slots.last().unwrap().0;
        slots.push((previous.wrapping_add(1), previous));
    }
    slots
}

/// The sequences in a first packet are the client's to pick and the wire allows any gap, so a
/// first tick steps at most [`join_burst_span`] of them, ending on the newest, and drops the older
/// commands: `[1, 1_000_000]` steps 1_000_000 alone, at once. A dense join (11 commands, or the
/// queue's whole 50) still steps every one.
#[test]
fn a_join_burst_steps_at_most_a_queue_of_sequences() {
    for rate in [RATE, TickRate::new(60)] {
        let span = join_burst_span(rate);
        assert_eq!(span, 50);
        let first_packet = |sequences: &[u32]| {
            let mut queued = QueuedUserCmds::default();
            let packet = UserCmdPacket {
                commands: commands(sequences.iter().copied()),
            };
            queued.push_packet(&packet, 0, rate);
            let start = std::time::Instant::now();
            let stepped = first_tick(rate, &mut queued);
            (stepped, start.elapsed())
        };

        let ((stepped, ack, left), took) = first_packet(&[1, 1_000_000]);
        assert_eq!(stepped, [(1_000_000, 0)]);
        assert_eq!(ack, 1_000_000);
        assert!(left.is_empty(), "1 is dropped");
        assert!(took < std::time::Duration::from_millis(250), "{took:?}");

        // Only the commands within the span of the newest: 999_960 starts the burst.
        let ((stepped, ack, left), _) = first_packet(&[1, 999_950, 999_960, 999_990, 1_000_000]);
        assert_eq!(stepped, chain(999_960, 1_000_000));
        assert_eq!(ack, 999_960, "the guess on 999_961 holds the ack");
        assert!(left.is_empty());

        // The widest a first packet can carry: just under half the ring past the ack.
        let ((stepped, ..), _) = first_packet(&[1, u32::MAX / 2 - 1]);
        assert_eq!(stepped, [(u32::MAX / 2 - 1, 0)]);

        for dense in [11, 50] {
            let ((stepped, ack, _), _) = first_packet(&Vec::from_iter(1..=dense));
            assert_eq!(
                stepped,
                chain(1, dense),
                "{dense} commands, every one stepped"
            );
            assert_eq!(ack, dense);
        }
    }
}

/// A bot's queue takes any sequences: a burst that crosses the wrap steps through it (0 too, a
/// guess), and one far wider than the span steps the span's newest only.
#[test]
fn a_join_burst_across_the_wrap_steps_at_most_a_queue_of_sequences() {
    let mut queued = QueuedUserCmds::default();
    let far = u32::MAX - 1_000_000;
    let near = [u32::MAX - 20, u32::MAX - 9, u32::MAX, 1, 5];
    for sequence in [far].into_iter().chain(near) {
        queued.push(UserCmd {
            sequence,
            ..default()
        });
    }
    let (stepped, ack, left) = first_tick(RATE, &mut queued);
    assert_eq!(stepped, chain(u32::MAX - 20, 5));
    assert_eq!(stepped.len(), 27);
    assert_eq!(ack, u32::MAX - 20, "the guess on the next holds the ack");
    assert!(left.is_empty(), "{far} is dropped");

    // 64 in a row, past the span: the newest 50 step.
    let mut queued = QueuedUserCmds::default();
    for command in commands((0..64).map(|i| (u32::MAX - 31).wrapping_add(i))) {
        queued.push(command);
    }
    let (stepped, ..) = first_tick(RATE, &mut queued);
    assert_eq!(stepped, chain(u32::MAX - 17, 31));
    assert_eq!(stepped.len(), 50);
}

/// A client silent for 15 s, then every command at once: the rollback re-steps the whole
/// timeline, which is its cap of 64 ticks at 60 Hz as at 20.
#[test]
fn a_rollback_resteps_at_most_its_cap_at_any_rate() {
    for rate in [RATE, TickRate::new(60)] {
        let mut buffer = ServerCommandBuffer::<TestGame>::default();
        let mut queued = QueuedUserCmds::default();
        let mut stats = RollbackStats::default();
        let cmd = |sequence| UserCmd {
            sequence,
            ..default()
        };
        let silent = rate.ticks(15.0) as u32;
        let mut tick = |queued: &mut QueuedUserCmds, tick: u32| {
            run_player_tick(
                &mut NoStepper,
                Entity::PLACEHOLDER,
                u64::from(tick),
                &mut buffer,
                queued,
                &mut stats,
                true,
                rate,
            );
        };
        queued.commands.push_back(cmd(1));
        tick(&mut queued, 1);
        for at in 2..=silent {
            tick(&mut queued, at);
        }
        queued.commands.extend((2..=silent + 1).map(cmd));
        tick(&mut queued, silent + 1);
        assert_eq!(stats.rollbacks, 1);
        assert_eq!(stats.max_depth, 64);
    }
}

#[test]
fn retiring_a_guessed_frame_acknowledges_it() {
    let mut buffer = ServerCommandBuffer::default();
    buffer.timeline.extend([
        input_frame(10, true),
        input_frame(11, false),
        input_frame(12, true),
    ]);
    advance_confirmation(&mut buffer);
    assert_eq!(buffer.last_processed_sequence, 10);

    retire_front(&mut buffer);
    assert_eq!(buffer.last_processed_sequence, 10);
    retire_front(&mut buffer);
    assert_eq!(buffer.last_processed_sequence, 12);
    assert_eq!(buffer.timeline.len(), 1);
}

#[test]
fn extrapolated_usercmd_repeats_the_last_input() {
    let command = UserCmd {
        sequence: 7,
        movement: Vec2::X,
        look: Vec2::new(0.5, -0.25),
        buttons: Buttons::JUMP,
        fire: Some(SubtickFire {
            frac: 0.5,
            look: Vec2::Y,
            origin: None,
        }),
        seen_server_tick: 20,
        seen_alpha: 0.25,
    };
    let next = extrapolate_usercmd(command, 8, 0, 100, false);
    assert_eq!(next.sequence, 8);
    assert_eq!(next.movement, command.movement);
    assert_eq!(next.look, command.look);
    assert_eq!(next.buttons, command.buttons);
    assert_eq!(next.fire, None);
    assert_eq!(next.seen_server_tick, 21);
    assert_eq!(next.seen_alpha, 0.25);

    let interrupted = extrapolate_usercmd(next, 9, 0, 100, true);
    assert_eq!(interrupted.movement, Vec2::ZERO);
    assert!(interrupted.buttons.is_empty());
    assert_eq!(interrupted.fire, None);
}

#[test]
fn confirmation_waits_for_and_then_crosses_a_replaced_gap() {
    let mut buffer = ServerCommandBuffer::default();
    buffer.timeline.extend([
        input_frame(10, true),
        input_frame(11, false),
        input_frame(12, true),
    ]);

    advance_confirmation(&mut buffer);
    assert_eq!(buffer.last_processed_sequence, 10);

    buffer.timeline[1].received = true;
    advance_confirmation(&mut buffer);
    assert_eq!(buffer.last_processed_sequence, 12);
}

#[test]
fn published_flags_come_from_the_stepped_frame() {
    let mut buffer = ServerCommandBuffer::default();
    let mut frame = input_frame(10, true);
    frame.flags = RemoteFlags::FAKED_INPUT | RemoteFlags::MOVE_INPUT;
    buffer.timeline.push_back(frame);
    buffer.last_command_tick = 10;

    // The tick that stepped: the frame's full flag set, faked input included.
    assert_eq!(
        published_flags(&buffer, 10, RATE),
        RemoteFlags::FAKED_INPUT | RemoteFlags::MOVE_INPUT
    );
    // A tick nothing stepped: only what the buffer itself knows.
    let fallback = published_flags(&buffer, 11, RATE);
    assert!(!fallback.contains(RemoteFlags::FAKED_INPUT));
    assert!(!fallback.contains(RemoteFlags::CONNECTION_INTERRUPTED));
    assert!(
        published_flags(
            &buffer,
            11 + RATE.ticks(CONNECTION_INTERRUPTED_SECONDS),
            RATE
        )
        .contains(RemoteFlags::CONNECTION_INTERRUPTED)
    );
}

#[test]
fn ack_crosses_a_reanchor_skip() {
    let mut buffer = ServerCommandBuffer::default();
    // 12..=19 were skipped by a re-anchor: they have no slots at all.
    buffer.timeline.extend([
        input_frame(10, true),
        input_frame(11, true),
        input_frame(20, true),
        input_frame(21, false),
    ]);
    advance_confirmation(&mut buffer);
    assert_eq!(buffer.last_processed_sequence, 20);
}

#[test]
fn lead_window_reports_and_expires() {
    let ticks = window();
    let mut window = InputLeadWindow::default();
    for (tick, lead) in [(100, 2), (101, 1), (102, 3), (103, 0), (105, 2)] {
        window.record(tick, lead, ticks);
    }
    assert_eq!(
        window.report(105, ticks),
        Some(InputLead {
            min: 0,
            max: 3,
            samples: 5
        })
    );
    // Ticks 100..=103 age out; only 105 is left.
    let later = 104 + ticks;
    assert_eq!(
        window.report(later, ticks),
        Some(InputLead {
            min: 2,
            max: 2,
            samples: 1
        })
    );
    assert_eq!(window.report(later + 1, ticks), None);
    // Far-off leads clamp to the wire type instead of wrapping.
    window.record(later + 1, -100_000, ticks);
    assert_eq!(window.report(later + 1, ticks).unwrap().min, i16::MIN);
}

#[test]
fn far_ahead_backlog_is_skipped() {
    let mut buffer = ServerCommandBuffer::<TestGame> {
        last_simulated_sequence: 50,
        ..default()
    };
    let mut queued = QueuedUserCmds::default();
    let packet = |sequence| UserCmdPacket {
        commands: vec![UserCmd {
            sequence,
            ..default()
        }],
    };

    // Every arrival lands 9 ahead of the slot about to run. Half a
    // window of that is evidence enough.
    let mut tick = 0;
    let skipped = loop {
        let sequence = buffer.expected_sequence() + 9;
        queued.push_packet(&packet(sequence), 0, RATE);
        let expected = buffer.expected_sequence();
        if let Some(skipped) = pace_input(&mut buffer, &mut queued, tick, RATE) {
            assert_eq!(buffer.expected_sequence(), expected + skipped);
            break skipped;
        }
        assert!(tick < window(), "never re-anchored");
        // The slot ran: next tick expects the next sequence.
        buffer.last_simulated_sequence += 1;
        tick += 1;
    };
    assert_eq!(tick + 1, window() / 2);
    assert_eq!(skipped as i32, 9 - INPUT_LEAD_TARGET);
    assert!(
        queued
            .commands
            .iter()
            .all(|command| command.sequence >= buffer.expected_sequence())
    );
    assert_eq!(
        buffer.lead.report(tick, window()),
        None,
        "judge the new anchor fresh"
    );

    // Bots fill the queue directly: no arrivals, never paced.
    let mut bot = ServerCommandBuffer::<TestGame> {
        last_simulated_sequence: 50,
        ..default()
    };
    let mut bot_queue = QueuedUserCmds::default();
    bot_queue.commands.extend((60..90).map(|sequence| UserCmd {
        sequence,
        ..default()
    }));
    for tick in 0..2 * window() {
        assert_eq!(pace_input(&mut bot, &mut bot_queue, tick, RATE), None);
    }
}

/// A client 200 ticks ahead: one re-anchor skips the whole backlog but the target's ticks in
/// hand.
#[test]
fn a_backlog_past_127_ticks_is_skipped_in_one_reanchor() {
    let mut buffer = ServerCommandBuffer::<TestGame> {
        last_simulated_sequence: 50,
        ..default()
    };
    let mut queued = QueuedUserCmds::default();
    let mut tick = 0;
    let skipped = loop {
        let packet = UserCmdPacket {
            commands: vec![UserCmd {
                sequence: buffer.expected_sequence() + 200,
                ..default()
            }],
        };
        queued.push_packet(&packet, 0, RATE);
        if let Some(skipped) = pace_input(&mut buffer, &mut queued, tick, RATE) {
            break skipped;
        }
        assert!(tick < window(), "never re-anchored");
        buffer.last_simulated_sequence += 1;
        tick += 1;
    };
    assert_eq!(skipped as i32, 200 - INPUT_LEAD_TARGET);
    assert_eq!(
        queued.commands.back().map(|command| command.sequence),
        Some(buffer.expected_sequence() + INPUT_LEAD_TARGET as u32),
        "the freshest command is the target's ticks ahead of the next slot"
    );
    assert_eq!(buffer.lead.report(tick, window()), None);
}

/// Every tick for `windows` lead windows at `rate`, an arrival `lead` ticks ahead of the slot
/// about to run, then that slot steps: the first re-anchor's skip, if any.
fn first_reanchor(rate: TickRate, lead: u32, windows: u64) -> Option<u32> {
    let mut buffer = ServerCommandBuffer::<TestGame> {
        last_simulated_sequence: 50,
        ..default()
    };
    let mut queued = QueuedUserCmds::default();
    for tick in 0..windows * rate.ticks(INPUT_LEAD_WINDOW_SECONDS) {
        let packet = UserCmdPacket {
            commands: vec![UserCmd {
                sequence: buffer.expected_sequence() + lead,
                ..default()
            }],
        };
        queued.push_packet(&packet, 0, rate);
        if let Some(skipped) = pace_input(&mut buffer, &mut queued, tick, rate) {
            return Some(skipped);
        }
        buffer.last_simulated_sequence += 1;
    }
    None
}

/// At 10 Hz the re-anchor's seconds round to 4 ticks, a lead a fast clock is held at. A client
/// arriving just in time, in the nudge band or held over it by a fast clock is still never
/// re-anchored, at 10, 12, 15 or 20 Hz; one a whole window at the threshold is, by a skip that
/// leaves the target in hand.
#[test]
fn an_on_time_client_is_never_reanchored_at_any_rate() {
    assert_eq!(resync_ahead_ticks(RATE), 8);
    for hz in [10, 12, 15, 20] {
        let rate = TickRate::new(hz);
        let ahead = resync_ahead_ticks(rate);
        assert!(
            ahead >= INPUT_LEAD_RESYNC_AHEAD_MIN_TICKS,
            "{hz} Hz: threshold {ahead}"
        );
        for lead in 0..ahead {
            assert_eq!(reanchor_skip(lead, rate), None, "{hz} Hz, lead {lead}");
            assert_eq!(
                first_reanchor(rate, lead as u32, 10),
                None,
                "{hz} Hz, lead {lead}"
            );
        }
        assert_eq!(
            first_reanchor(rate, ahead as u32, 10),
            Some((ahead - INPUT_LEAD_TARGET) as u32),
            "{hz} Hz"
        );
        assert_eq!(reanchor_skip(i16::MIN.into(), rate), None, "{hz} Hz");
    }
}
