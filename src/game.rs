//! What a game plugs in: [`PlayerGame`] on its per-player POD, and the [`Step`] each command
//! hands it.

use std::fmt::Debug;

use bevy::{
    ecs::{
        entity::MapEntities,
        system::{ReadOnlySystemParam, SystemParamItem},
    },
    prelude::*,
};
use bevy_ahoy::{
    CharacterController, CharacterControllerDerivedProps, Mover,
    movers::AmongMovers,
    moves::{MoveState, Pm},
};
use serde::{Serialize, de::DeserializeOwned};

use crate::{
    events::StepEvent,
    lag_comp::PlayerPoses,
    pmove::wish,
    poses::RemoteRenderTime,
    protocol::{Buttons, PlayerId, PlayerShape, TickRate, UserCmd},
    seen::{Seen, StepWorld},
};

/// How hard overlap pushes players apart (1/s²): m/s² of push per metre of overlap, at most
/// [`PLAYER_SEPARATION_MAX_ACCELERATION`].
///
/// The push changes velocity, never position: it resolves through the next move_and_slide, so
/// it can't shove anyone through world geometry, and a misprediction is one tick of wrong
/// velocity, not a teleport.
const PLAYER_SEPARATION_STIFFNESS: f32 = 160.0;
/// The hardest the push accelerates a player out of an overlap (m/s²).
const PLAYER_SEPARATION_MAX_ACCELERATION: f32 = 180.0;

/// The game half of a player, implemented on the game's POD. `PartialEq` is exact: the server
/// finds external edits with it.
///
/// `MapEntities`: the [`OwnerSnapshot`](crate::OwnerSnapshot) carries the game half and the
/// client maps its entities; a game half with none derives it and maps nothing. A game whose
/// steps make nothing happen sets `type Event` to [`NoEvents`](crate::NoEvents).
pub trait PlayerGame:
    Copy
    + Default
    + PartialEq
    + Debug
    + Serialize
    + DeserializeOwned
    + MapEntities
    + Send
    + Sync
    + 'static
{
    /// What a step makes happen, and the message the game reads it as. State follows the last
    /// run, events follow the first: each (id, outermost enum variant) is written once per
    /// timeline, by the first run that emits it, and a step emits at most one event per id and
    /// outermost variant, so each kind of event needs its own top-level variant ([`StepEvent`]).
    type Event: StepEvent;

    /// What a step reads of the world besides its player and the world as seen: a resource, a
    /// query, a tuple of them, `()` for none. Every step fetches it, on every path: the client's
    /// prediction and replay, the server's step and its rollback ([`Step::context`]).
    ///
    /// A step runs again for old commands, long after the tick they stand for, and the client's
    /// runs ahead of the server's. So a context is only for data that is fixed, or that changes
    /// identically on both peers at a known tick. Anything that changes during play is read
    /// through a [`History`](crate::History) at the step's [`seen_time`](Step::seen_time), or
    /// lives in the game half. Read-only: a step changes the world only through its player and
    /// its events. Reading what the stepping systems write (any player's [`PlayerState`]) is a
    /// system-param conflict, which Bevy panics on at startup.
    type Context: ReadOnlySystemParam + 'static;

    /// One command through one player: the move, then the think, in the game's order.
    fn step(player: &mut PlayerState<Self>, step: &mut Step<Self>);

    /// Reconcile equality on the game half. Override with tolerances for floats that go
    /// through libm.
    fn agrees(&self, other: &Self) -> bool {
        self == other
    }
}

/// Everything rollback saves for one player, and what a replay starts from: movement and the
/// game half.
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct PlayerState<G> {
    /// The movement half: where it is, how it moves, what it stands on.
    pub moves: MoveState,
    /// The game's own per-player state.
    pub game: G,
}

impl<G: PlayerGame> PlayerState<G> {
    /// A player standing at `position`, looking along -Z.
    pub fn at(position: Vec3) -> Self {
        Self {
            moves: MoveState {
                position,
                orientation: Quat::IDENTITY,
                ..default()
            },
            game: default(),
        }
    }
}

/// Everything one step reads besides the player.
pub struct Step<'a, 'w, 's, G: PlayerGame> {
    pub(crate) owner: PlayerId,
    pub(crate) command: UserCmd,
    pub(crate) previous_sequence: u32,
    pub(crate) previous_buttons: Buttons,
    pub(crate) poses: &'a PlayerPoses<'a>,
    pub(crate) tick_rate: TickRate,
    pub(crate) shape: PlayerShape,
    pub(crate) seen: &'a Seen<'a, 'a, 'a>,
    /// What the step emitted, for its timeline's [`Outbox`](crate::events::Outbox).
    pub(crate) outbox: &'a mut Vec<G::Event>,
    pub(crate) cfg: &'a CharacterController,
    pub(crate) hulls: &'a CharacterControllerDerivedProps,
    pub(crate) context: &'a SystemParamItem<'w, 's, G::Context>,
}

impl<'a, 'w, 's, G: PlayerGame> Step<'a, 'w, 's, G> {
    /// The id of the player being stepped.
    pub fn owner(&self) -> PlayerId {
        self.owner
    }

    /// The command, as stepped (sanitized on the server).
    pub fn command(&self) -> &UserCmd {
        &self.command
    }

    /// The last command stepped before this one: sweeps cover the gap between them. The client
    /// picks the sequences, so the gap is as wide as it makes it (from 0 on a timeline's first
    /// step): cover it in closed form, never a pass per sequence.
    pub fn previous_sequence(&self) -> u32 {
        self.previous_sequence
    }

    /// That command's buttons: a press is a bit set on the command's buttons and not here.
    pub fn previous_buttons(&self) -> Buttons {
        self.previous_buttons
    }

    /// Every player's pose history, for lag compensation and the push.
    pub fn poses(&self) -> &'a PlayerPoses<'a> {
        self.poses
    }

    /// The rate the step runs at: one command is one tick of it.
    pub fn tick_rate(&self) -> TickRate {
        self.tick_rate
    }

    /// Every player's shape: what the push keeps apart and lag-compensated casts hit.
    pub fn shape(&self) -> PlayerShape {
        self.shape
    }

    /// The game's [`PlayerGame::Context`].
    pub fn context(&self) -> &'a SystemParamItem<'w, 's, G::Context> {
        self.context
    }

    /// The stock move: [`wish`] from the command, [`pmove`](crate::pmove::pmove) among the
    /// movers near the player.
    pub fn pmove(&self, s: &mut MoveState) {
        let wish = wish(&self.command, self.previous_buttons);
        self.with_pm(s.position, |movers, pm| {
            crate::pmove::pmove(s, wish, movers, pm)
        });
    }

    /// The seen level among the movers near `at`, built when asked, for a game's own move.
    pub fn with_pm<R>(&self, at: Vec3, f: impl FnOnce(&[Mover], &Pm) -> R) -> R {
        let movers = self.seen.movers(at);
        let level = AmongMovers::new(self.seen.level(), &movers);
        let pm = Pm {
            cfg: self.cfg,
            hulls: self.hulls,
            world: &level,
            dt: self.tick_rate.seconds() as f32,
        };
        f(&movers, &pm)
    }

    /// The world at the command's seen time, for rays.
    pub fn world(&self) -> StepWorld<'_, '_, '_, '_> {
        StepWorld::seen(self.seen)
    }

    /// The remote time the player saw when they built the command: sample a game's
    /// [`History`](crate::History) here.
    pub fn seen_time(&self) -> RemoteRenderTime {
        RemoteRenderTime::new(self.command.seen_server_tick, self.command.seen_alpha)
    }

    /// Make `event` happen. Every run of the command emits it again; the first run that emits
    /// it writes it, as a `G::Event` message, and no later one on this timeline does. At most
    /// one event per variant per [`StepEvent::id`] a step: a second is dropped, with a warning.
    pub fn emit(&mut self, event: G::Event) {
        self.outbox.push(event);
    }

    /// The player push: clips `s.velocity` against the other players where the player saw them
    /// and drains any overlap. Call it after the move: it reads the moved position.
    pub fn separate(&self, s: &mut MoveState) {
        let seen_time = self.seen_time();
        let position = s.position;
        let velocity = &mut s.velocity;
        // Each player pushes only themselves, so no step writes another player. Poses at the
        // seen time give both peers the same push; ids keep the f32 sum in one order.
        for other in self.poses.ids() {
            if other == self.owner {
                continue;
            }
            let Some(pose) = self.poses.pose_at_time(other, seen_time) else {
                continue;
            };
            let delta = position - pose.position;
            if delta.y.abs() >= self.shape.half_height * 2.0 {
                continue;
            }
            let flat = Vec2::new(delta.x, delta.z);
            let distance = flat.length();
            let touch = self.shape.radius * 2.0;
            if distance >= touch {
                continue;
            }
            // Dead-centre overlap (spawn stacks) needs a way out both peers agree on.
            let direction = if distance > 0.001 {
                flat / distance
            } else {
                Vec2::X
            };
            let away = Vec3::new(direction.x, 0.0, direction.y);
            // Stop at their surface and keep the slide, as move_and_slide does at a wall.
            let approach = -Vec3::new(velocity.x, 0.0, velocity.z).dot(away);
            if approach > 0.0 {
                *velocity += away * approach;
            }
            // overlap * STIFFNESS * dt: an acceleration, the same per second at any tick rate.
            let hz = self.tick_rate.hz() as f32;
            let share = PLAYER_SEPARATION_STIFFNESS / hz / hz;
            let bias = (touch - distance) * hz * share;
            *velocity += away * bias.min(PLAYER_SEPARATION_MAX_ACCELERATION / hz);
        }
    }
}

/// The smallest game, for the library's own tests: the stock move, the push, and a
/// [`TestEvent::Fired`] on every click.
#[cfg(test)]
pub(crate) mod test_game {
    use bevy::{ecs::entity::MapEntities, prelude::Message};
    use serde::{Deserialize, Serialize};

    use super::{PlayerGame, PlayerState, Step};
    use crate::events::{EventId, StepEvent};

    #[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize, MapEntities)]
    pub(crate) struct TestGame;

    #[derive(Message, Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
    pub(crate) enum TestEvent {
        Fired(EventId),
        Hit(EventId),
    }

    impl StepEvent for TestEvent {
        fn id(&self) -> EventId {
            match *self {
                Self::Fired(id) | Self::Hit(id) => id,
            }
        }
    }

    impl PlayerGame for TestGame {
        type Event = TestEvent;
        type Context = ();

        fn step(player: &mut PlayerState<Self>, step: &mut Step<Self>) {
            step.pmove(&mut player.moves);
            step.separate(&mut player.moves);
            if step.command().fire.is_some() {
                step.emit(TestEvent::Fired(EventId {
                    owner: step.owner(),
                    sequence: step.command().sequence,
                }));
            }
        }
    }

    /// A rocket and nothing else: a click fires one, which blasts at its fuse, or on its first
    /// command of flight if that command saw the target in its path (a direct hit). Either
    /// blast is a [`TestEvent::Hit`] under the fire's id, as the example's rockets.
    #[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize, MapEntities)]
    pub(crate) struct Fuse {
        /// The command that fired the rocket in flight.
        pub(crate) rocket: Option<u32>,
    }

    impl Fuse {
        /// The fuse (s): the example's longest flight, 81 commands at 60 Hz.
        pub(crate) const SECONDS: f64 = 1.35;
        /// The one seen tick at which the target is in a rocket's path.
        pub(crate) const TARGET_TICK: u64 = 3;
    }

    impl PlayerGame for Fuse {
        type Event = TestEvent;
        type Context = ();

        fn step(player: &mut PlayerState<Self>, step: &mut Step<Self>) {
            let sequence = step.command().sequence;
            if let Some(fired) = player.game.rocket {
                let flight = sequence.wrapping_sub(fired);
                let direct = flight == 1 && step.seen_time().tick == Self::TARGET_TICK;
                if direct || u64::from(flight) >= step.tick_rate().ticks(Self::SECONDS) {
                    step.emit(TestEvent::Hit(EventId {
                        owner: step.owner(),
                        sequence: fired,
                    }));
                    player.game.rocket = None;
                }
            }
            if step.command().fire.is_some() && player.game.rocket.is_none() {
                step.emit(TestEvent::Fired(EventId {
                    owner: step.owner(),
                    sequence,
                }));
                player.game.rocket = Some(sequence);
            }
        }
    }

    /// A game that only walks: no entities, no events.
    #[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize, MapEntities)]
    pub(crate) struct Walk;

    impl PlayerGame for Walk {
        type Event = crate::events::NoEvents;
        type Context = ();

        fn step(player: &mut PlayerState<Self>, step: &mut Step<Self>) {
            step.pmove(&mut player.moves);
        }
    }
}

#[cfg(test)]
mod tests {
    use bevy::{prelude::*, state::app::StatesPlugin};
    use bevy_replicon::prelude::*;

    use super::test_game::Walk;
    use crate::{NetAhoyClientPlugin, NetAhoyProtocolPlugin, NetAhoyServerPlugin};

    #[test]
    fn a_game_with_no_events_builds_every_plugin() {
        for server in [true, false] {
            let mut app = App::new();
            app.add_plugins((
                MinimalPlugins,
                StatesPlugin,
                RepliconPlugins,
                NetAhoyProtocolPlugin::<Walk>::default(),
            ));
            if server {
                app.add_plugins(NetAhoyServerPlugin::<Walk>::default());
            } else {
                app.add_plugins(NetAhoyClientPlugin::<Walk>::default());
            }
            app.finish();
        }
    }
}
