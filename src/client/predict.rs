//! The local player's prediction: each command stepped, kept and sent.

use std::collections::VecDeque;

use bevy::{ecs::system::SystemParam, prelude::*};
use bevy_replicon::prelude::*;

use crate::{
    events::Outbox,
    game::PlayerGame,
    protocol::*,
    stepper::{PredictionFrame, Stepper},
};

use super::{
    PredictedPlayer,
    input::{ClientInput, PresentedFrame, command_buttons},
    reconcile::PredictionStatus,
    remote_clock::RemoteClock,
};

/// How many commands each packet carries again, by link: set from the [`LinkLossModel`] the
/// game puts on its connecting session entity. A stream redelivers on its own, and
/// extra copies only cost a throttled uplink; on a datagram link the tail is the recovery: a lost
/// packet's commands ride the next one.
#[derive(Resource, Clone, Copy, Debug)]
pub struct LinkProfile {
    /// The most commands a packet carries: every unacked one up to this, so the tail follows the
    /// link's real RTT. 1 = the newest only.
    pub usercmd_backup_cap: usize,
    /// The cap a [`LinkLossModel::Datagram`] link gets.
    pub datagram_backup_cap: usize,
}

impl LinkProfile {
    /// The stream profile, and [`DATAGRAM_USERCMD_BACKUP_SECONDS`] at `rate`, at most
    /// [`DATAGRAM_USERCMD_BACKUP_MAX_COMMANDS`], for a datagram link.
    pub fn new(rate: TickRate) -> Self {
        Self {
            usercmd_backup_cap: 1,
            datagram_backup_cap: rate.capped_ticks(
                DATAGRAM_USERCMD_BACKUP_SECONDS,
                DATAGRAM_USERCMD_BACKUP_MAX_COMMANDS,
            ) as usize,
        }
    }
}

/// How much input a datagram carries again (s): with a tail of n commands, every input gets
/// through n - 1 packets lost in a row. Deeper tails grow the packet with no rescue left to buy.
pub(crate) const DATAGRAM_USERCMD_BACKUP_SECONDS: f64 = 0.5;

/// The most commands one datagram carries, at any rate ([`TickRate::capped_ticks`]): a big
/// datagram is what a collapsed congestion window holds back first.
pub(crate) const DATAGRAM_USERCMD_BACKUP_MAX_COMMANDS: u64 = 10;

/// Derive the link profile from the game's [`LinkLossModel`] tag on the
/// connecting session entity, and again whenever the tag changes.
pub(super) fn apply_link_profile(
    models: Query<&LinkLossModel, Changed<LinkLossModel>>,
    mut profile: ResMut<LinkProfile>,
) {
    for model in &models {
        profile.usercmd_backup_cap = match model {
            LinkLossModel::Stream => 1,
            LinkLossModel::Datagram => profile.datagram_backup_cap.max(1),
        };
    }
}

/// How much of its own past the client keeps (s), as predicted frames and as commands: well past
/// the deepest replay ([`MAX_REPLAY_SECONDS`](super::reconcile::MAX_REPLAY_SECONDS)).
pub(super) const PREDICTION_HISTORY_SECONDS: f64 = 12.8;

/// Where the local player's command stream stands: the sequences handed out and stepped.
#[derive(Resource, Default)]
pub(crate) struct ClientInputState {
    /// The newest sequence handed out; the next command is this + 1.
    pub(crate) next_sequence: u32,
    /// The command the predicted state last stepped through (or adopted from the server). Equal
    /// to `next_sequence` except right after a [`CommandClock`](super::CommandClock) skip: the
    /// next step sees the gap.
    pub(crate) previous_sequence: u32,
    pub(crate) pending_record_command: Option<UserCmd>,
    pub(crate) previous_buttons: Buttons,
}

#[derive(Resource)]
pub(crate) struct PredictionHistory<G> {
    pub frames: VecDeque<PredictionFrame<G>>,
    pub(super) capacity: usize,
}

impl<G> PredictionHistory<G> {
    /// No frames yet, room for [`PREDICTION_HISTORY_SECONDS`] at `rate`.
    pub(crate) fn new(rate: TickRate) -> Self {
        let capacity = rate.ticks(PREDICTION_HISTORY_SECONDS) as usize;
        Self {
            frames: VecDeque::with_capacity(capacity),
            capacity,
        }
    }
}

impl<G: PlayerGame> PredictionHistory<G> {
    pub(crate) fn push(&mut self, frame: PredictionFrame<G>) {
        if let Some(existing) = self
            .frames
            .iter_mut()
            .find(|existing| existing.command.sequence == frame.command.sequence)
        {
            *existing = frame;
            return;
        }

        if self.frames.len() == self.capacity {
            self.frames.pop_front();
        }
        self.frames.push_back(frame);
    }

    pub(crate) fn get(&self, sequence: u32) -> Option<&PredictionFrame<G>> {
        self.frames
            .iter()
            .rev()
            .find(|frame| frame.command.sequence == sequence)
    }

    /// Forgets every frame before `sequence`; the frame at it stays.
    pub(crate) fn retain_after(&mut self, sequence: u32) {
        self.frames.retain(|frame| {
            frame.command.sequence == sequence
                || sequence_is_newer(frame.command.sequence, sequence)
        });
    }

    pub(crate) fn clear(&mut self) {
        self.frames.clear();
    }
}

/// Forgets the old session's predicted frames and the events it passed on.
pub(super) fn reset_prediction<G: PlayerGame>(world: &mut World) {
    if let Some(mut history) = world.get_resource_mut::<PredictionHistory<G>>() {
        history.clear();
    }
    if let Some(mut events) = world.get_resource_mut::<Outbox<G::Event>>() {
        *events = Outbox::default();
    }
}

#[derive(Resource)]
pub(crate) struct LocalCommandHistory {
    pub commands: VecDeque<UserCmd>,
    pub(super) capacity: usize,
}

impl LocalCommandHistory {
    /// No commands yet, room for [`PREDICTION_HISTORY_SECONDS`] at `rate`.
    pub(crate) fn new(rate: TickRate) -> Self {
        let capacity = rate.ticks(PREDICTION_HISTORY_SECONDS) as usize;
        Self {
            commands: VecDeque::with_capacity(capacity),
            capacity,
        }
    }

    /// Forgets every command.
    pub(crate) fn clear(&mut self) {
        self.commands.clear();
    }

    pub(crate) fn push(&mut self, command: UserCmd) {
        if self.commands.len() == self.capacity {
            self.commands.pop_front();
        }
        self.commands.push_back(command);
    }

    pub(crate) fn after_sequence(&self, sequence: u32) -> Vec<UserCmd> {
        self.commands
            .iter()
            .copied()
            .filter(|command| sequence_is_newer(command.sequence, sequence))
            .collect()
    }
}

/// The local player's run of commands: where its stream stands, the commands it stepped and the
/// events it passed on. A new session starts all three over.
#[derive(SystemParam)]
pub(super) struct LocalTimeline<'w, G: PlayerGame> {
    pub(super) input: ResMut<'w, ClientInputState>,
    pub(super) sent: ResMut<'w, LocalCommandHistory>,
    pub(super) events: ResMut<'w, Outbox<<G as PlayerGame>::Event>>,
}

pub(super) fn drive_prediction_and_send_input<G: PlayerGame>(
    mut commands: Commands,
    mut input: ResMut<ClientInput>,
    clock: Res<RemoteClock>,
    mut timeline: LocalTimeline<G>,
    profile: Res<LinkProfile>,
    presented: Res<PresentedFrame>,
    mut predictions: Query<(Entity, &mut PredictionStatus), With<PredictedPlayer>>,
    mut stepper: Stepper<G>,
) {
    let Ok((predicted_entity, mut status)) = predictions.single_mut() else {
        return;
    };

    // The remote time the player reacted to rides the command so the shared
    // step samples the same poses on both peers: what the frame the input
    // was sampled from drew.
    let cursor = clock.target_time().unwrap_or_default();
    let raw_fire = input.fire.take();
    let fire = raw_fire.and_then(SubtickFire::sanitized);
    let fire_frame = input.fire_frame.take().filter(|_| raw_fire.is_some());
    let sampled = fire_frame.unwrap_or(*presented);
    let seen = sampled.remote.unwrap_or(cursor);
    // Drained every tick, used only when nothing is held by the tick frame.
    let tap_movement = std::mem::take(&mut input.tap_movement);
    let movement = if input.movement != Vec2::ZERO {
        input.movement
    } else {
        tap_movement
    };
    let (buttons, wait) = command_buttons(
        input.buttons,
        std::mem::take(&mut input.pressed),
        timeline.input.previous_buttons,
    );
    input.pressed |= wait;
    let command = UserCmd {
        sequence: timeline.input.next_sequence.wrapping_add(1),
        movement: movement.clamp_length_max(1.0),
        look: input.look,
        buttons,
        fire,
        seen_server_tick: seen.tick,
        seen_alpha: seen.alpha,
    };

    if let Err(err) = stepper.player_move(
        predicted_entity,
        &mut timeline.events,
        command,
        timeline.input.previous_sequence,
        timeline.input.previous_buttons,
    ) {
        warn!(
            "failed to step predicted KCC for command {}: {err}",
            command.sequence
        );
    }

    let input_state = &mut timeline.input;
    input_state.next_sequence = command.sequence;
    input_state.previous_sequence = command.sequence;
    input_state.previous_buttons = command.buttons;
    input_state.pending_record_command = Some(command);
    timeline.sent.push(command);
    status.newest_sequence = command.sequence;

    let mut unacked = timeline.sent.after_sequence(status.last_ack_sequence);
    let skip = unacked.len().saturating_sub(profile.usercmd_backup_cap);
    commands.client_trigger(UserCmdPacket {
        commands: unacked.split_off(skip),
    });
}

pub(super) fn record_prediction_state<G: PlayerGame>(
    mut input_state: ResMut<ClientInputState>,
    mut history: ResMut<PredictionHistory<G>>,
    predictions: Query<Entity, With<PredictedPlayer>>,
    stepper: Stepper<G>,
) {
    let Some(command) = input_state.pending_record_command.take() else {
        return;
    };
    let Ok(predicted_entity) = predictions.single() else {
        return;
    };

    if let Some(frame) = stepper.capture_frame(predicted_entity, command) {
        history.push(frame);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        client::{ClientInput, NetAhoyClientPlugin, PresentedFrame},
        game::{PlayerState, test_game::TestGame},
    };

    /// The session's end forgets every predicted frame: a disconnect leaves
    /// `ClientState::Connected`, and a demo seek runs the session reset itself.
    #[test]
    fn a_session_end_clears_the_prediction_history() {
        let mut app = App::new();
        app.add_plugins((
            MinimalPlugins,
            bevy::state::app::StatesPlugin,
            RepliconPlugins,
            NetAhoyProtocolPlugin::<TestGame>::default(),
            NetAhoyClientPlugin::<TestGame>::default(),
        ));
        app.finish();
        let frame = PredictionFrame {
            command: UserCmd::default(),
            state: PlayerState::<TestGame>::default(),
        };
        let world = app.world_mut();
        world
            .resource_mut::<PredictionHistory<TestGame>>()
            .push(frame);
        world.run_schedule(OnExit(ClientState::Connected));
        assert!(
            world
                .resource::<PredictionHistory<TestGame>>()
                .frames
                .is_empty()
        );

        world
            .resource_mut::<PredictionHistory<TestGame>>()
            .push(frame);
        world
            .run_system_cached(crate::reconnect::reset_client_session)
            .unwrap();
        assert!(
            world
                .resource::<PredictionHistory<TestGame>>()
                .frames
                .is_empty()
        );
    }

    /// Every packet a datagram link sends carries the unacked commands, newest last, as many as
    /// at 20 Hz at most: the half-second window's 10 there, and at 60 Hz the cap
    /// ([`DATAGRAM_USERCMD_BACKUP_MAX_COMMANDS`]) where the window would be 30.
    #[test]
    fn a_datagram_carries_no_more_commands_at_60_hz_than_at_20() {
        use bevy::{ecs::system::RunSystemOnce, state::app::StatesPlugin};
        use bevy_ahoy::CharacterController;

        use crate::{client::stepping_app, game::test_game::TestGame};

        #[derive(Resource, Default)]
        struct Sent(Vec<UserCmdPacket>);

        let cap = TickRate::new(20).ticks(DATAGRAM_USERCMD_BACKUP_SECONDS) as u32;
        let sent_at = |hz| {
            let rate = TickRate::new(hz);
            let mut app = stepping_app::<TestGame>(rate);
            app.add_plugins((
                StatesPlugin,
                RepliconPlugins,
                NetAhoyProtocolPlugin::<TestGame>::new(rate),
            ))
            .insert_resource(RemoteClock::new(rate))
            .insert_resource(LinkProfile::new(rate))
            .init_resource::<ClientInput>()
            .init_resource::<PresentedFrame>()
            .init_resource::<Sent>()
            // Disconnected, replicon hands the client's packets to this app as the server's.
            .add_observer(
                |packet: On<FromClient<UserCmdPacket>>, mut sent: ResMut<Sent>| {
                    sent.0.push(packet.message.clone());
                },
            );
            app.finish();
            let world = app.world_mut();
            world.spawn(LinkLossModel::Datagram);
            world.spawn((
                PlayerId(1),
                PredictedPlayer {
                    server_entity: Entity::PLACEHOLDER,
                },
                PlayerState::<TestGame>::at(Vec3::Y),
                CharacterController::default(),
            ));
            world.run_system_once(apply_link_profile).unwrap();
            // Nothing is ever acked: the tail only grows.
            for _ in 0..rate.ticks(DATAGRAM_USERCMD_BACKUP_SECONDS) {
                app.world_mut()
                    .run_system_once(drive_prediction_and_send_input::<TestGame>)
                    .unwrap();
                app.update();
            }
            app.update();
            std::mem::take(&mut app.world_mut().resource_mut::<Sent>().0)
        };

        for (hz, packets) in [(20, 10), (60, 30)] {
            let sent = sent_at(hz);
            assert_eq!(sent.len(), packets, "{hz} Hz");
            for (index, packet) in sent.iter().enumerate() {
                let newest = index as u32 + 1;
                let oldest = newest.saturating_sub(cap - 1).max(1);
                let sequences: Vec<u32> = packet
                    .commands
                    .iter()
                    .map(|command| command.sequence)
                    .collect();
                assert_eq!(
                    sequences,
                    (oldest..=newest).collect::<Vec<_>>(),
                    "{hz} Hz, packet {newest}"
                );
            }
        }
    }
}
