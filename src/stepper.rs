//! The movement context: one user command in, one step out. Prediction, replay and the
//! server all go through [`Stepper::player_move`]: copy the player's [`PlayerState`]
//! out, build the world as they saw it, step the game ([`PlayerGame::step`]), copy it back.

use avian3d::prelude::*;
use bevy::{
    ecs::{
        message::Messages,
        schedule::ScheduleLabel,
        system::{StaticSystemParam, SystemParam},
    },
    prelude::*,
};
use bevy_ahoy::{
    CharacterController, CharacterControllerDerivedProps, CharacterLook, moves::MoveState,
};

use crate::{
    events::Outbox,
    game::{PlayerGame, PlayerState, Step},
    history::History,
    lag_comp::PlayerPoses,
    pmove::{look, orientation},
    poses::RemoteRenderTime,
    protocol::{Buttons, OwnerSnapshot, PlayerId, PlayerShape, PlayerSnapshot, TickRate, UserCmd},
    seen::SeenWorld,
};

/// Where bevy_ahoy's own per-tick systems go: a schedule nothing runs, since netahoy steps
/// players itself, one command at a time. Pass it to `AhoyPlugins::new`; the client and server
/// plugins panic at `finish` if the controller runs anywhere else.
#[derive(ScheduleLabel, Clone, Debug, PartialEq, Eq, Hash)]
pub struct KccSchedule;

/// Panics if bevy_ahoy's controller runs on a schedule of its own, where it would move every
/// player a second time, beside the stepper.
pub(crate) fn assert_kcc_unscheduled(app: &App, plugin: &str) {
    for kcc in app.get_added_plugins::<bevy_ahoy::AhoyKccPlugin>() {
        assert!(
            kcc.schedule == KccSchedule.intern(),
            "{plugin}: bevy_ahoy's controller runs on {:?} and would move every player beside \
             the stepper: add AhoyPlugins::new(KccSchedule)",
            kcc.schedule
        );
    }
}

/// One command and the player after it: what a rewind restores and a replay starts from, on the
/// client (prediction) and on the server (input rollback).
#[derive(Clone, Copy, Debug)]
pub struct PredictionFrame<G> {
    /// The command, as stepped.
    pub command: UserCmd,
    /// The player as it stood after `command`.
    pub state: PlayerState<G>,
}

impl<G: PlayerGame> PredictionFrame<G> {
    /// Where the player stood after the command.
    pub fn position(&self) -> Vec3 {
        self.state.moves.position
    }

    /// How it moved after the command.
    pub fn velocity(&self) -> Vec3 {
        self.state.moves.velocity
    }

    /// Its look after the command: yaw and pitch.
    pub fn look(&self) -> Vec2 {
        look(self.state.moves.orientation)
    }
}

/// Everything one step reads and writes: the system parameter prediction, replay and the
/// server's ticks all step players through. Outside the library it only reads them.
#[derive(SystemParam)]
pub struct Stepper<'w, 's, G: PlayerGame> {
    players: Query<
        'w,
        's,
        (
            &'static PlayerId,
            &'static mut PlayerState<G>,
            &'static CharacterController,
            &'static CharacterControllerDerivedProps,
        ),
    >,
    /// The level and the bodies' histories: what [`Seen`](crate::seen::Seen) is made of.
    world: SeenWorld<'w, 's>,
    tick_rate: Res<'w, TickRate>,
    shape: Res<'w, PlayerShape>,
    /// Where each step's events go, the first time their timeline passes them on.
    events: ResMut<'w, Messages<<G as PlayerGame>::Event>>,
    // The same history on both peers: the server records what it publishes, the client what it
    // receives.
    poses: Query<'w, 's, (&'static PlayerId, &'static History<PlayerSnapshot>)>,
    /// What the game's steps read besides: [`PlayerGame::Context`].
    context: StaticSystemParam<'w, 's, <G as PlayerGame>::Context>,
}

impl<G: PlayerGame> Stepper<'_, '_, G> {
    /// Run one command through one player: build the world as the player saw it at the
    /// command's seen time, then [`PlayerGame::step`] moves and thinks the player through it.
    /// `previous_sequence`/`previous_buttons` are the last command actually stepped: sweeps cover
    /// the gap, and a button set on `command` and not before is a press. The step's events go
    /// through `outbox`, its timeline's, and only the ones it never passed on are written.
    pub(crate) fn player_move(
        &mut self,
        entity: Entity,
        outbox: &mut Outbox<G::Event>,
        command: UserCmd,
        previous_sequence: u32,
        previous_buttons: Buttons,
    ) -> Result<()> {
        let Self {
            players,
            world,
            tick_rate,
            shape,
            events,
            poses,
            context,
        } = self;
        let (id, mut state, cfg, hulls) = players.get_mut(entity)?;
        let mut ps = *state;

        // One sample for both the move (bodies at whole ticks) and the game's rays (sub-tick).
        let seen_time = RemoteRenderTime::new(command.seen_server_tick, command.seen_alpha);
        let seen = world.at(seen_time);

        ps.moves.orientation = orientation(&command);
        let poses = PlayerPoses::new(poses.iter(), **tick_rate);
        let mut step = Step {
            owner: *id,
            command,
            previous_sequence,
            previous_buttons,
            poses: &poses,
            tick_rate: **tick_rate,
            shape: **shape,
            seen: &seen,
            outbox: &mut outbox.pending,
            cfg,
            hulls,
            context: &**context,
        };
        G::step(&mut ps, &mut step);

        *state = ps;
        outbox.send(events);
        Ok(())
    }

    /// The player as it stands after `command`, to rewind to later.
    pub fn capture_frame(&self, entity: Entity, command: UserCmd) -> Option<PredictionFrame<G>> {
        let (_, state, ..) = self.players.get(entity).ok()?;
        Some(PredictionFrame {
            command,
            state: *state,
        })
    }

    /// Rewind `entity` to an authoritative snapshot: the server's whole player at the ack.
    pub(crate) fn restore(&mut self, entity: Entity, snapshot: &OwnerSnapshot<G>) {
        let Ok((_, mut state, ..)) = self.players.get_mut(entity) else {
            return;
        };
        *state = PlayerState {
            moves: snapshot.moves,
            game: snapshot.game,
        };
    }

    /// Restore an exact locally captured pmove frame. The server uses this to
    /// replace a guessed input tick when its real command arrives late.
    pub(crate) fn restore_frame(&mut self, entity: Entity, frame: &PredictionFrame<G>) {
        if let Ok((_, mut state, ..)) = self.players.get_mut(entity) {
            *state = frame.state;
        }
    }

    /// The rate it steps at.
    pub fn tick_rate(&self) -> TickRate {
        *self.tick_rate
    }

    /// Where `entity` stands, if it is a player this steps.
    pub fn position(&self, entity: Entity) -> Option<Vec3> {
        let (_, state, ..) = self.players.get(entity).ok()?;
        Some(state.moves.position)
    }

    /// How `entity` moves, if it is a player this steps.
    pub fn velocity(&self, entity: Entity) -> Option<Vec3> {
        let (_, state, ..) = self.players.get(entity).ok()?;
        Some(state.moves.velocity)
    }
}

/// The components the rest of the world reads a player by.
pub(crate) type PlayerComponents = (
    &'static mut Transform,
    &'static mut Position,
    &'static mut LinearVelocity,
    &'static mut CharacterLook,
);

/// The components the rest of the world reads a player by, written from its [`PlayerState`]
/// after stepping. One way: nothing reads them back into the step.
pub(crate) fn write_player_components<G: PlayerGame>(
    mut players: Query<(&PlayerState<G>, PlayerComponents), Changed<PlayerState<G>>>,
) {
    for (state, components) in &mut players {
        show(&state.moves, components);
    }
}

/// Writes `moves` into a player's components.
pub(crate) fn show(
    moves: &MoveState,
    (mut transform, mut position, mut velocity, mut character_look): (
        Mut<Transform>,
        Mut<Position>,
        Mut<LinearVelocity>,
        Mut<CharacterLook>,
    ),
) {
    transform.translation = moves.position;
    position.0 = moves.position;
    velocity.0 = moves.velocity;
    let look = look(moves.orientation);
    character_look.yaw = look.x;
    character_look.pitch = look.y;
}
