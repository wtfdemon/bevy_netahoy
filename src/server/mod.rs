//! The server: steps the commands players send, tells everyone what happened, and keeps the
//! same pose history every client keeps.

#[cfg(test)]
mod input_sim;
mod intake;
#[cfg(test)]
mod link_sim;
mod publish;
mod runner;
mod timeline;
mod visibility;

use std::marker::PhantomData;

use avian3d::prelude::{Collider, CollisionLayers, LinearVelocity, Position};
use bevy::prelude::*;
use bevy_ahoy::{CharacterController, CharacterLook};
use bevy_replicon::prelude::*;

use crate::{
    events::{Outbox, RemoteEvent, StepEvent},
    game::{PlayerGame, PlayerState},
    history::stamp_replication_tick,
    protocol::*,
    stepper::{PlayerComponents, PredictionFrame, Stepper, show},
};

use intake::queue_player_commands;
use publish::{publish_authoritative_player_snapshots, publish_body_snapshots};
use runner::{TicksPerFrame, audit_ticks_per_frame};

pub use intake::QueuedUserCmds;
pub(crate) use intake::{SERVER_CMD_QUEUE_MAX_COMMANDS, SERVER_CMD_QUEUE_MIN_COMMANDS};
#[cfg(not(target_family = "wasm"))]
pub use runner::one_tick_per_frame;
#[cfg(test)]
pub(crate) use timeline::INPUT_LEAD_RESYNC_AHEAD_MIN_TICKS;
pub(crate) use timeline::{RollbackStats, ServerCommandBuffer, TimelineStepper, run_player_tick};
pub use visibility::{OwnerSnapshotPlugin, PlayerOwner};

/// Server input rollback, on or off: [`NetAhoyServerPlugin::input_rollback`].
#[derive(Resource, Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct InputRollback {
    pub(crate) enabled: bool,
}

impl Default for InputRollback {
    fn default() -> Self {
        Self { enabled: true }
    }
}

/// The server plugin's system sets, for a game to order its own systems against.
#[derive(SystemSet, Debug, Clone, Copy, Hash, PartialEq, Eq)]
pub enum NetAhoyServerSystems {
    /// Steps every player's queued commands (`FixedPreUpdate`).
    ApplyCommands,
    /// React to what the steps made happen and send it to the clients, before the players'
    /// components are written (`FixedPreUpdate`).
    Events,
    /// Publish snapshots and record lag-compensation poses (`FixedLast`).
    Publish,
}

/// The server half for the game `G`: steps every player's commands, sends their events,
/// publishes their snapshots.
pub struct NetAhoyServerPlugin<G> {
    /// Whether a late command re-steps the ticks that ran on a guess for it: on by default.
    /// Off, the guess stands as the input that ran and the owner reconciles against it.
    pub input_rollback: bool,
    game: PhantomData<G>,
}

impl<G> Default for NetAhoyServerPlugin<G> {
    fn default() -> Self {
        Self {
            input_rollback: true,
            game: PhantomData,
        }
    }
}

impl<G> NetAhoyServerPlugin<G> {
    /// The same plugin with input rollback on or off.
    pub fn with_input_rollback(self, on: bool) -> Self {
        Self {
            input_rollback: on,
            ..self
        }
    }
}

impl<G: PlayerGame> Plugin for NetAhoyServerPlugin<G> {
    fn build(&self, app: &mut App) {
        app.add_plugins(crate::reconnect::NetAhoyServerSessionPlugin)
            .add_plugins(OwnerSnapshotPlugin::<G>::default())
            .register_required_components::<PlayerOwner, ServerCommandBuffer<G>>()
            .register_required_components::<PlayerOwner, QueuedUserCmds>()
            .register_required_components::<PlayerOwner, OwnerSnapshot<G>>()
            .register_required_components::<PlayerOwner, PlayerState<G>>()
            .register_required_components::<PlayerOwner, Transform>()
            .register_required_components::<PlayerOwner, Position>()
            .register_required_components::<PlayerOwner, LinearVelocity>()
            .register_required_components::<PlayerOwner, CharacterLook>()
            .init_resource::<ServerTick>()
            .add_message::<G::Event>()
            .init_resource::<RollbackStats>()
            .init_resource::<TicksPerFrame>()
            .add_observer(queue_player_commands::<G>)
            .add_observer(admit_player::<G>)
            .add_observer(players_leave_with_their_client)
            .add_systems(FixedFirst, advance_server_tick)
            .configure_sets(
                FixedPreUpdate,
                NetAhoyServerSystems::Events.after(NetAhoyServerSystems::ApplyCommands),
            )
            .add_systems(
                FixedPreUpdate,
                (
                    apply_player_commands::<G>.in_set(NetAhoyServerSystems::ApplyCommands),
                    broadcast_events::<G::Event>.in_set(NetAhoyServerSystems::Events),
                    // Everyone else reads a player by its components: write them once the
                    // tick's steps and the game's reactions are done.
                    crate::stepper::write_player_components::<G>
                        .after(NetAhoyServerSystems::Events),
                ),
            )
            .add_systems(
                FixedLast,
                (
                    publish_authoritative_player_snapshots::<G>,
                    publish_body_snapshots,
                    stamp_replication_tick
                        .run_if(resource_exists::<bevy_replicon::server::ReplicationUserdata>),
                )
                    .chain()
                    .in_set(NetAhoyServerSystems::Publish),
            )
            .add_systems(Last, audit_ticks_per_frame);
        app.insert_resource(InputRollback {
            enabled: self.input_rollback,
        });
        if !self.input_rollback {
            info!("server input rollback off");
        }
    }

    fn finish(&self, app: &mut App) {
        crate::stepper::assert_kcc_unscheduled(app, "NetAhoyServerPlugin");
        fixed_tick_rate(app.world(), "NetAhoyServerPlugin");
    }
}

/// A new player: checks it has what stepping it needs, writes its components from its state,
/// and tells the client that owns it which player it is. A player owned by anything but an
/// authorized client (or by nobody, a bot) is despawned: a client refused or gone between its join
/// and its player's spawn must not leave one behind.
fn admit_player<G: PlayerGame>(
    insert: On<Insert, PlayerOwner>,
    mut players: Query<(
        &PlayerOwner,
        Option<&PlayerId>,
        Has<CharacterController>,
        (Has<Collider>, Option<&CollisionLayers>),
        &PlayerState<G>,
        PlayerComponents,
    )>,
    clients: Query<(), (With<ConnectedClient>, With<AuthorizedClient>)>,
    mut commands: Commands,
) -> Result {
    let player = insert.event_target();
    let (owner, player_id, controlled, (collider, layers), state, components) =
        players.get_mut(player)?;
    let Some(&player_id) = player_id else {
        return Err(format!("player {player} has a PlayerOwner but no PlayerId").into());
    };
    if owner.0 != Entity::PLACEHOLDER && !controlled {
        return Err(format!(
            "player {player} is owned by a client, which steps it, but has no \
             CharacterController: spawn it with the game's controller and collider"
        )
        .into());
    }
    if let Some(error) = player_layers_error(&format!("player {player}"), collider, layers) {
        return Err(error.into());
    }
    if owner.0 != Entity::PLACEHOLDER && !clients.contains(owner.0) {
        warn!(
            "player {player} is owned by {}, which is no authorized client: despawned",
            owner.0
        );
        commands.entity(player).despawn();
        return Ok(());
    }
    show(&state.moves, components);
    if owner.0 != Entity::PLACEHOLDER {
        commands.server_trigger(ToClients {
            targets: SendTargets::Single(ClientId::Client(owner.0)),
            message: JoinAccepted { player_id },
        });
    }
    Ok(())
}

/// A client's players leave with its connection, however it ends: a disconnect, a timeout, a
/// refusal, or a despawn that fired no `Disconnected`. Nothing else steps a player once its
/// client is gone.
fn players_leave_with_their_client(
    gone: On<Remove, ConnectedClient>,
    players: Query<(Entity, &PlayerOwner, &PlayerId)>,
    mut commands: Commands,
) {
    let client = gone.event_target();
    for (player, owner, id) in &players {
        if owner.0 == client {
            info!("player {} left with its client {client}", id.0);
            commands.entity(player).despawn();
        }
    }
}

/// Sends every client each event the steps made: the command buffers wrote each once.
fn broadcast_events<E: StepEvent>(mut commands: Commands, mut events: MessageReader<E>) {
    for &event in events.read() {
        commands.server_trigger(ToClients {
            targets: SendTargets::All,
            message: RemoteEvent(event),
        });
    }
}

/// The server's tick count, one more each fixed tick: what histories, snapshots and seen times
/// count in.
#[derive(Resource, Default)]
pub struct ServerTick(pub u64);

fn advance_server_tick(mut tick: ResMut<ServerTick>) {
    tick.0 = tick.0.wrapping_add(1);
}

impl<G: PlayerGame> TimelineStepper<G> for Stepper<'_, '_, G> {
    fn capture_frame(&mut self, player: Entity, command: UserCmd) -> Option<PredictionFrame<G>> {
        Stepper::capture_frame(self, player, command)
    }

    fn restore_frame(&mut self, player: Entity, frame: &PredictionFrame<G>) {
        Stepper::restore_frame(self, player, frame);
    }

    fn player_move(
        &mut self,
        player: Entity,
        outbox: &mut Outbox<G::Event>,
        command: UserCmd,
        previous_sequence: u32,
        previous_buttons: Buttons,
    ) -> Result<()> {
        Stepper::player_move(
            self,
            player,
            outbox,
            command,
            previous_sequence,
            previous_buttons,
        )
    }
}

fn apply_player_commands<G: PlayerGame>(
    tick: Res<ServerTick>,
    mut players: Query<(Entity, &mut ServerCommandBuffer<G>, &mut QueuedUserCmds)>,
    mut stats: ResMut<RollbackStats>,
    mut stepper: Stepper<G>,
    rollback: Res<InputRollback>,
    tick_rate: Res<TickRate>,
) {
    for (player, mut command_buffer, mut queued) in &mut players {
        run_player_tick(
            &mut stepper,
            player,
            tick.0,
            &mut command_buffer,
            &mut queued,
            &mut stats,
            rollback.enabled,
            *tick_rate,
        );
    }
}

#[cfg(test)]
mod tests {
    use avian3d::prelude::{LinearVelocity, Position};

    use super::*;
    use crate::{
        events::EventId,
        game::test_game::{TestEvent, TestGame},
    };

    const RATE: TickRate = TickRate::new(20);

    /// A server step over a real level with only the systems this needs. Steps the fixed loop's
    /// `apply_player_commands`; [`step`] reads its events as a game's reaction does.
    fn stepping_app() -> App {
        stepping_app_at(RATE)
    }

    /// [`stepping_app`] at `rate`.
    fn stepping_app_at(rate: TickRate) -> App {
        use std::time::Duration;

        use avian3d::prelude::*;
        use bevy::time::TimeUpdateStrategy;
        use bevy_ahoy::{input::AhoyInputPlugin, prelude::*};

        use crate::{KccSchedule, WORLD_COLLISION_LAYER};

        let mut app = App::new();
        app.add_plugins((
            MinimalPlugins,
            TransformPlugin,
            bevy::diagnostic::DiagnosticsPlugin,
            PhysicsPlugins::default(),
            AhoyPlugins::new(KccSchedule)
                .build()
                .disable::<AhoyInputPlugin>(),
        ))
        .insert_resource(rate)
        .insert_resource(PlayerShape::default())
        .insert_resource(Time::<Fixed>::from_hz(f64::from(rate.hz())))
        .insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(
            1.0 / f64::from(rate.hz()),
        )))
        .insert_resource(ServerTick(1))
        .add_message::<TestEvent>()
        .init_resource::<RollbackStats>()
        .init_resource::<InputRollback>()
        .init_resource::<avian3d::spatial_query::SpatialQueryDiagnostics>()
        .init_resource::<avian3d::collider_tree::ColliderTreeDiagnostics>()
        .init_resource::<avian3d::collision::CollisionDiagnostics>()
        .init_resource::<avian3d::dynamics::solver::SolverDiagnostics>();
        app.world_mut().spawn((
            Transform::from_translation(Vec3::new(0.0, -0.5, 0.0)),
            RigidBody::Static,
            Collider::cuboid(200.0, 1.0, 200.0),
            CollisionLayers::new(WORLD_COLLISION_LAYER, LayerMask::ALL),
        ));
        let reader = app.world_mut().register_system(fired_ids);
        app.insert_resource(Reader(reader));
        app
    }

    /// One reader of the events, kept across ticks like a game system.
    #[derive(Resource)]
    struct Reader(bevy::ecs::system::SystemId<(), Vec<EventId>>);

    fn fired_ids(mut events: MessageReader<TestEvent>) -> Vec<EventId> {
        events
            .read()
            .filter_map(|event| match *event {
                TestEvent::Fired(id) => Some(id),
                TestEvent::Hit(_) => None,
            })
            .collect()
    }

    const SHOOTER: crate::protocol::PlayerId = crate::protocol::PlayerId(1);

    /// A [`TestGame`] player joining as [`SHOOTER`], with a fresh command buffer as every
    /// connection has.
    fn join(app: &mut App) -> Entity {
        join_as::<TestGame>(app)
    }

    /// [`join`] for the game `G`.
    fn join_as<G: PlayerGame>(app: &mut App) -> Entity {
        use avian3d::prelude::*;
        use bevy_ahoy::{CharacterLook, prelude::*};

        use crate::{PLAYER_COLLISION_LAYER, WORLD_COLLISION_LAYER};

        let start = Vec3::new(0.0, 1.0, 0.0);
        let player = app
            .world_mut()
            .spawn((
                SHOOTER,
                PlayerState::<G>::at(start),
                CharacterLook::default(),
                CharacterController {
                    filter: SpatialQueryFilter::from_mask(WORLD_COLLISION_LAYER),
                    ..default()
                },
                PlayerShape::default().collider(),
                CollisionLayers::new(PLAYER_COLLISION_LAYER, LayerMask::ALL),
                Transform::from_translation(start),
                ServerCommandBuffer::<G>::default(),
                QueuedUserCmds::default(),
            ))
            .id();
        // Let avian and Ahoy set up the collider, the query trees and the hulls.
        for _ in 0..4 {
            app.update();
        }
        player
    }

    fn cmd(sequence: u32, click: bool) -> UserCmd {
        UserCmd {
            sequence,
            seen_server_tick: u64::from(sequence),
            fire: click.then_some(SubtickFire {
                frac: 0.5,
                look: Vec2::ZERO,
                origin: None,
            }),
            ..default()
        }
    }

    /// One server tick with `arrived` in the player's queue; the fires it let through.
    fn step(app: &mut App, player: Entity, arrived: &[UserCmd]) -> Vec<EventId> {
        step_as::<TestGame>(app, player, arrived);
        let world = app.world_mut();
        let reader = world.resource::<Reader>().0;
        world.run_system(reader).unwrap()
    }

    /// One server tick of the game `G` with `arrived` in the player's queue.
    fn step_as<G: PlayerGame>(app: &mut App, player: Entity, arrived: &[UserCmd]) {
        use bevy::ecs::system::RunSystemOnce;

        let world = app.world_mut();
        world.resource_mut::<ServerTick>().0 += 1;
        world
            .get_mut::<QueuedUserCmds>(player)
            .unwrap()
            .commands
            .extend(arrived.iter().copied());
        world.run_system_once(apply_player_commands::<G>).unwrap();
    }

    fn rocket(sequence: u32) -> EventId {
        EventId {
            owner: SHOOTER,
            sequence,
        }
    }

    /// Commands 1 to 4 with a click on 3; the fires they let through.
    fn session(app: &mut App, player: Entity) -> Vec<EventId> {
        (1..=4)
            .flat_map(|sequence| step(app, player, &[cmd(sequence, sequence == 3)]))
            .collect()
    }

    #[test]
    fn a_rejoining_player_can_reuse_an_event_id() {
        let mut app = stepping_app();
        let first = join(&mut app);
        assert_eq!(session(&mut app, first), [rocket(3)]);

        // The same id, sequences from the start: a new rocket, not a replay of the old one.
        app.world_mut().despawn(first);
        let second = join(&mut app);
        assert_eq!(
            session(&mut app, second),
            [rocket(3)],
            "the new connection's rocket must fire"
        );
    }

    #[test]
    fn a_new_command_buffer_is_a_new_timeline() {
        let mut app = stepping_app();
        let player = join(&mut app);
        assert_eq!(session(&mut app, player), [rocket(3)]);

        // The same player starts its commands over, as a looping demo does.
        app.world_mut().entity_mut(player).insert((
            ServerCommandBuffer::<TestGame>::default(),
            QueuedUserCmds::default(),
        ));
        assert_eq!(
            session(&mut app, player),
            [rocket(3)],
            "the restarted rocket must fire"
        );
    }

    #[test]
    fn a_rollback_over_a_fire_lets_it_through_once() {
        let mut app = stepping_app();
        let player = join(&mut app);

        let mut fired = step(&mut app, player, &[cmd(1, false)]);
        // Command 2 is late: its tick runs on a guess, then 3 fires on top of the guess.
        fired.extend(step(&mut app, player, &[]));
        fired.extend(step(&mut app, player, &[cmd(3, true)]));
        assert_eq!(fired, [rocket(3)]);

        // 2 lands and the server steps 2 and 3 again: the fire is written a second time.
        let rollbacks = app.world().resource::<RollbackStats>().rollbacks;
        let replayed = step(&mut app, player, &[cmd(2, false)]);
        assert_eq!(
            app.world().resource::<RollbackStats>().rollbacks,
            rollbacks + 1
        );
        assert_eq!(
            replayed,
            [],
            "a replayed fire must not be let through again"
        );
    }

    #[test]
    fn a_late_fire_that_only_the_rerun_makes_gets_through() {
        let mut app = stepping_app();
        let player = join(&mut app);

        let mut fired = step(&mut app, player, &[cmd(1, false)]);
        // Command 2 is late: its guess doesn't fire.
        fired.extend(step(&mut app, player, &[]));
        fired.extend(step(&mut app, player, &[cmd(3, false)]));
        assert_eq!(fired, []);

        // 2 lands with its click: the rollback is the first step that fires it.
        let rollbacks = app.world().resource::<RollbackStats>().rollbacks;
        let replayed = step(&mut app, player, &[cmd(2, true)]);
        assert_eq!(
            app.world().resource::<RollbackStats>().rollbacks,
            rollbacks + 1
        );
        assert_eq!(
            replayed,
            [rocket(2)],
            "a fire only the rerun makes must get through"
        );
    }

    /// State follows the last run, events follow the first. At 60 Hz a guessed slot's direct hit
    /// that the real command misses moves the blast to the rocket's fuse, 81 commands on, past
    /// the 64 slots a rollback reaches: the rocket blasts there, and the hit, already out, is
    /// not written again.
    #[test]
    fn a_hit_a_rollback_moves_past_its_window_is_written_once() {
        use crate::game::test_game::Fuse;

        fn written(mut events: MessageReader<TestEvent>) -> Vec<TestEvent> {
            events.read().copied().collect()
        }

        let rate = TickRate::new(60);
        let mut app = stepping_app_at(rate);
        let player = join_as::<Fuse>(&mut app);
        let reader = app.world_mut().register_system(written);
        let step = |app: &mut App, arrived: &[UserCmd]| {
            step_as::<Fuse>(app, player, arrived);
            app.world_mut().run_system(reader).unwrap()
        };
        let rocket_in_flight = |app: &App| {
            app.world()
                .get::<PlayerState<Fuse>>(player)
                .unwrap()
                .game
                .rocket
        };
        let (fired, hit) = (TestEvent::Fired(rocket(2)), TestEvent::Hit(rocket(2)));

        assert_eq!(step(&mut app, &[cmd(1, false)]), []);
        assert_eq!(step(&mut app, &[cmd(2, true)]), [fired]);
        // 3 is late: its guess sees a tick past 2's, the target's, and hits it directly.
        assert_eq!(Fuse::TARGET_TICK, 3);
        assert_eq!(step(&mut app, &[]), [hit]);
        assert_eq!(rocket_in_flight(&app), None);

        // The real 3 saw 2's tick again: the rocket misses and flies on.
        let late = UserCmd {
            seen_server_tick: 2,
            ..cmd(3, false)
        };
        assert_eq!(step(&mut app, &[late, cmd(4, false)]), []);
        assert_eq!(app.world().resource::<RollbackStats>().rollbacks, 1);
        assert_eq!(rocket_in_flight(&app), Some(2));

        let due = 2 + rate.ticks(Fuse::SECONDS) as u32;
        assert_eq!(due, 83);
        let mut later = Vec::new();
        for sequence in 5..=due + 10 {
            later.extend(step(&mut app, &[cmd(sequence, false)]));
            assert_eq!(
                rocket_in_flight(&app),
                (sequence < due).then_some(2),
                "the state blasts at the fuse, on {sequence}"
            );
        }
        assert_eq!(later, [], "the moved blast is the hit already written");
        assert_eq!(app.world().resource::<RollbackStats>().rollbacks, 1);
    }

    /// A rollback over a fire steps it again, and the clients still hear of it once.
    #[test]
    fn a_rerun_event_reaches_the_clients_once() {
        use bevy::state::app::StatesPlugin;
        use bevy_replicon::test_app::ServerTestAppExt;

        #[derive(Resource, Default)]
        struct Received(Vec<TestEvent>);

        let mut server = plugin_app();
        server.finish();
        let mut client = App::new();
        client
            .add_plugins((
                MinimalPlugins,
                StatesPlugin,
                RepliconPlugins,
                NetAhoyProtocolPlugin::<TestGame>::default(),
            ))
            .init_resource::<Received>()
            .add_observer(
                |event: On<RemoteEvent<TestEvent>>, mut received: ResMut<Received>| {
                    received.0.push(event.0);
                },
            );
        client.finish();
        server.connect_client(&mut client);
        let owner = connected_client(&mut server);
        let player = spawn_player(&mut server, owner, SHOOTER, Vec3::new(0.0, 0.76, 0.0));

        // 2 is late: its tick runs on a guess, 3 fires on top of it, and 2 landing steps 2 and
        // 3 again.
        for arrived in [&[cmd(1, false)][..], &[], &[cmd(3, true)], &[cmd(2, false)]] {
            tick(&mut server, player, arrived);
            server.exchange_with_client(&mut client);
            client.update();
        }
        assert_eq!(server.world().resource::<RollbackStats>().rollbacks, 1);
        assert_eq!(
            client.world().resource::<Received>().0,
            [TestEvent::Fired(rocket(3))]
        );
    }

    /// Every reader sees each event once, in any schedule and in no order with the others: the
    /// command buffer wrote it once, a rollback over it included.
    #[test]
    fn two_readers_in_two_schedules_see_each_event_once() {
        #[derive(Resource, Default)]
        struct Fired<const SCHEDULE: u8>(Vec<EventId>);

        fn read<const SCHEDULE: u8>(
            mut events: MessageReader<TestEvent>,
            mut fired: ResMut<Fired<SCHEDULE>>,
        ) {
            fired
                .0
                .extend(events.read().filter_map(|event| match *event {
                    TestEvent::Fired(id) => Some(id),
                    TestEvent::Hit(_) => None,
                }));
        }

        let mut app = plugin_app();
        app.init_resource::<Fired<0>>()
            .init_resource::<Fired<1>>()
            .add_systems(FixedUpdate, read::<0>)
            .add_systems(Update, read::<1>);
        app.finish();
        let owner = connected_client(&mut app);
        let player = spawn_player(&mut app, owner, SHOOTER, Vec3::new(0.0, 0.76, 0.0));

        // 2 is late: its tick runs on a guess, 3 fires on top of it, and 2 landing steps 2 and
        // 3 again. 5 fires once.
        let arrivals: [&[UserCmd]; 6] = [
            &[cmd(1, false)],
            &[],
            &[cmd(3, true)],
            &[cmd(2, false), cmd(4, false)],
            &[cmd(5, true)],
            &[cmd(6, false)],
        ];
        for arrived in arrivals {
            tick(&mut app, player, arrived);
        }
        let world = app.world();
        assert_eq!(world.resource::<RollbackStats>().rollbacks, 1);
        assert_eq!(world.resource::<Fired<0>>().0, [rocket(3), rocket(5)]);
        assert_eq!(world.resource::<Fired<1>>().0, [rocket(3), rocket(5)]);
    }

    /// A client as the backend and replicon leave it once it has connected and passed the
    /// protocol check: what a player's owner must be.
    fn connected_client(app: &mut App) -> Entity {
        app.world_mut()
            .spawn((ConnectedClient { max_size: 1200 }, AuthorizedClient))
            .id()
    }

    fn server_app() -> App {
        use bevy::state::app::StatesPlugin;

        let mut app = App::new();
        app.add_plugins((
            MinimalPlugins,
            StatesPlugin,
            RepliconPlugins.set(ServerPlugin::new(PostUpdate)),
            NetAhoyProtocolPlugin::<TestGame>::default(),
            NetAhoyServerPlugin::<TestGame>::default(),
        ));
        app.finish();
        app
    }

    /// The client that owns a player learns its id from the plugin, once; a bot owned by nobody
    /// tells nobody.
    #[test]
    fn the_owner_of_a_new_player_is_told_its_id() {
        use bevy::state::app::StatesPlugin;
        use bevy_replicon::test_app::{ServerTestAppExt, TestClientEntity};

        #[derive(Resource, Default)]
        struct Accepted(Vec<PlayerId>);

        let mut server = server_app();
        // No fixed tick: stepping needs a physics world this app doesn't have.
        server.insert_resource(bevy::time::TimeUpdateStrategy::ManualDuration(
            std::time::Duration::ZERO,
        ));
        let mut client = App::new();
        client
            .add_plugins((
                MinimalPlugins,
                StatesPlugin,
                RepliconPlugins,
                NetAhoyProtocolPlugin::<TestGame>::default(),
            ))
            .init_resource::<Accepted>()
            .add_observer(|accepted: On<JoinAccepted>, mut all: ResMut<Accepted>| {
                all.0.push(accepted.player_id);
            });
        client.finish();
        server.connect_client(&mut client);
        let owner = client.world().resource::<TestClientEntity>().entity();

        server.world_mut().spawn((
            PlayerOwner(owner),
            PlayerId(7),
            bevy_ahoy::CharacterController::default(),
        ));
        server
            .world_mut()
            .spawn((PlayerOwner(Entity::PLACEHOLDER), PlayerId(9001)));
        for _ in 0..2 {
            server.update();
            server.exchange_with_client(&mut client);
            client.update();
        }
        assert_eq!(client.world().resource::<Accepted>().0, [PlayerId(7)]);
    }

    /// A client's player goes with its connection, however the connection ends: here the way
    /// replicon refuses a client, a despawn that fires no `Disconnected`. A bot stays.
    #[test]
    fn a_player_leaves_with_its_client() {
        let mut app = server_app();
        let client = connected_client(&mut app);
        let player = app
            .world_mut()
            .spawn((
                PlayerOwner(client),
                PlayerId(7),
                bevy_ahoy::CharacterController::default(),
            ))
            .id();
        let bot = app
            .world_mut()
            .spawn((PlayerOwner(Entity::PLACEHOLDER), PlayerId(9001)))
            .id();
        app.update();
        assert!(app.world().get_entity(player).is_ok(), "joined");

        app.world_mut().despawn(client);
        app.update();
        assert!(app.world().get_entity(player).is_err(), "left with it");
        assert!(app.world().get_entity(bot).is_ok(), "the bot stays");
    }

    /// A player spawned for something that is no authorized client (one refused, one gone before
    /// its player spawned, or no client at all) is despawned at once rather than stepped forever.
    #[test]
    fn a_player_without_an_authorized_client_is_despawned() {
        let mut app = server_app();
        let refused = app
            .world_mut()
            .spawn(ConnectedClient { max_size: 1200 })
            .id();
        let nobody = app.world_mut().spawn_empty().id();
        let players: Vec<_> = [refused, nobody]
            .into_iter()
            .map(|owner| {
                app.world_mut()
                    .spawn((
                        PlayerOwner(owner),
                        PlayerId(7),
                        bevy_ahoy::CharacterController::default(),
                    ))
                    .id()
            })
            .collect();
        app.update();
        for player in players {
            assert!(
                app.world().get_entity(player).is_err(),
                "{player} despawned"
            );
        }
    }

    /// A game spawns a player with its owner and id; the plugin adds the rest, keeps what the
    /// game spawned it with, and shows the player where its state is.
    #[test]
    fn a_player_owner_brings_the_rest_of_a_server_player() {
        let mut app = server_app();
        let mut start = PlayerState::<TestGame>::at(Vec3::X);
        start.moves.velocity = Vec3::Z;
        let player = app
            .world_mut()
            .spawn((PlayerOwner(Entity::PLACEHOLDER), PlayerId(9001), start))
            .id();
        let player = app.world().entity(player);
        assert!(player.contains::<Replicated>());
        assert!(player.contains::<ServerPlayer>());
        assert!(player.contains::<ServerCommandBuffer<TestGame>>());
        assert!(player.contains::<QueuedUserCmds>());
        assert!(player.contains::<OwnerSnapshot<TestGame>>());
        assert_eq!(player.get::<PlayerState<TestGame>>(), Some(&start));
        assert_eq!(player.get::<Transform>().unwrap().translation, Vec3::X);
        assert_eq!(player.get::<Position>().unwrap().0, Vec3::X);
        assert_eq!(player.get::<LinearVelocity>().unwrap().0, Vec3::Z);
    }

    /// bevy_ahoy's controller on its own schedule would move every player a second time.
    #[test]
    #[should_panic(expected = "add AhoyPlugins::new(KccSchedule)")]
    fn ahoys_controller_on_its_own_schedule_panics_at_finish() {
        use bevy::{ecs::schedule::ScheduleLabel, state::app::StatesPlugin};

        let mut app = App::new();
        app.add_plugins((
            MinimalPlugins,
            StatesPlugin,
            RepliconPlugins.set(ServerPlugin::new(PostUpdate)),
            NetAhoyProtocolPlugin::<TestGame>::default(),
            NetAhoyServerPlugin::<TestGame>::default(),
            bevy_ahoy::AhoyKccPlugin {
                schedule: FixedPostUpdate.intern(),
            },
        ));
        app.finish();
    }

    #[test]
    #[should_panic(expected = "has a PlayerOwner but no PlayerId")]
    fn a_player_without_an_id_is_an_error_at_spawn() {
        server_app()
            .world_mut()
            .spawn(PlayerOwner(Entity::PLACEHOLDER));
    }

    /// A client's commands step its player: without a controller nothing could.
    #[test]
    #[should_panic(expected = "but has no CharacterController")]
    fn a_client_player_without_a_controller_is_an_error_at_spawn() {
        let mut app = server_app();
        let client = connected_client(&mut app);
        app.world_mut().spawn((PlayerOwner(client), PlayerId(1)));
    }

    /// A player's collider left on avian's default layers is on the world layer, where every
    /// step would meet it as level geometry.
    #[test]
    #[should_panic(expected = "is a member of WORLD_COLLISION_LAYER (avian's default layers)")]
    fn a_player_collider_on_the_world_layer_is_an_error_at_spawn() {
        let mut app = server_app();
        let client = connected_client(&mut app);
        app.world_mut().spawn((
            PlayerOwner(client),
            PlayerId(1),
            bevy_ahoy::CharacterController::default(),
            PlayerShape::default().collider(),
        ));
    }

    /// A server with the whole plugin over a real floor, one fixed tick per update. Finish it
    /// once the test's own systems are in.
    fn plugin_app() -> App {
        use avian3d::prelude::*;
        use bevy::{state::app::StatesPlugin, time::TimeUpdateStrategy};
        use bevy_ahoy::{input::AhoyInputPlugin, prelude::*};

        use crate::{KccSchedule, WORLD_COLLISION_LAYER};

        let mut app = App::new();
        app.add_plugins((
            MinimalPlugins,
            StatesPlugin,
            TransformPlugin,
            bevy::diagnostic::DiagnosticsPlugin,
            PhysicsPlugins::default(),
            AhoyPlugins::new(KccSchedule)
                .build()
                .disable::<AhoyInputPlugin>(),
            RepliconPlugins.set(ServerPlugin::new(PostUpdate)),
            NetAhoyProtocolPlugin::<TestGame>::default(),
            NetAhoyServerPlugin::<TestGame>::default(),
        ))
        .insert_resource(TimeUpdateStrategy::ManualDuration(
            std::time::Duration::from_secs_f64(RATE.seconds()),
        ))
        .init_resource::<avian3d::spatial_query::SpatialQueryDiagnostics>()
        .init_resource::<avian3d::collider_tree::ColliderTreeDiagnostics>()
        .init_resource::<avian3d::collision::CollisionDiagnostics>()
        .init_resource::<avian3d::dynamics::solver::SolverDiagnostics>();
        app.world_mut().spawn((
            Transform::from_translation(Vec3::new(0.0, -0.5, 0.0)),
            RigidBody::Static,
            Collider::cuboid(200.0, 1.0, 200.0),
            CollisionLayers::new(WORLD_COLLISION_LAYER, LayerMask::ALL),
        ));
        app
    }

    /// A player owned by `owner`, with a controller, standing at `at`; then a few ticks for
    /// avian and Ahoy to set up its collider and hulls.
    fn spawn_player(app: &mut App, owner: Entity, id: PlayerId, at: Vec3) -> Entity {
        use avian3d::prelude::*;
        use bevy_ahoy::prelude::*;

        use crate::{PLAYER_COLLISION_LAYER, WORLD_COLLISION_LAYER};

        let player = app
            .world_mut()
            .spawn((
                PlayerOwner(owner),
                id,
                PlayerState::<TestGame>::at(at),
                CharacterController {
                    filter: SpatialQueryFilter::from_mask(WORLD_COLLISION_LAYER),
                    ..default()
                },
                PlayerShape::default().collider(),
                CollisionLayers::new(PLAYER_COLLISION_LAYER, LayerMask::ALL),
            ))
            .id();
        for _ in 0..4 {
            app.update();
        }
        player
    }

    /// One server tick with `arrived` in `player`'s queue.
    fn tick(app: &mut App, player: Entity, arrived: &[UserCmd]) {
        let world = app.world_mut();
        let before = world.resource::<ServerTick>().0;
        let mut queued = world.get_mut::<QueuedUserCmds>(player).unwrap();
        for &command in arrived {
            queued.push(command);
        }
        app.update();
        assert_eq!(
            app.world().resource::<ServerTick>().0,
            before + 1,
            "one tick"
        );
    }

    /// A scripted bot, owned by nobody, has the command queue every player has, and the server
    /// steps it only on commands it is given. Without any, its state is the game's alone however
    /// many ticks run, even with a controller and in the air, where a step would let it fall.
    #[test]
    fn a_bot_is_never_stepped_without_commands() {
        let mut app = plugin_app();
        app.finish();
        let start = Vec3::new(0.0, 5.0, 0.0);
        let bot = spawn_player(&mut app, Entity::PLACEHOLDER, PlayerId(9001), start);
        for _ in 0..20 {
            tick(&mut app, bot, &[]);
        }
        let world = app.world();
        assert_eq!(
            world.get::<PlayerState<TestGame>>(bot),
            Some(&PlayerState::at(start))
        );
        let buffer = world.get::<ServerCommandBuffer<TestGame>>(bot).unwrap();
        assert!(
            buffer.timeline.is_empty(),
            "no tick stepped, not even a guess"
        );

        tick(&mut app, bot, &[cmd(1, false)]);
        let state = app.world().get::<PlayerState<TestGame>>(bot).unwrap();
        assert!(state.moves.position.y < start.y, "a command steps it");
    }

    /// A client's first packet can carry several commands: the ones it predicted before the
    /// server's first tick for its player (a stalled first packet, a long first frame). The
    /// server steps every one of them, so the owner's first snapshot is the player the client
    /// predicted at the newest, not the spawn stepped once.
    #[test]
    fn a_join_burst_steps_every_command_the_client_predicted() {
        use bevy::ecs::system::RunSystemOnce;

        let burst: Vec<UserCmd> = (1..=5)
            .map(|sequence| UserCmd {
                sequence,
                movement: Vec2::Y,
                buttons: if sequence == 3 {
                    Buttons::JUMP
                } else {
                    Buttons::empty()
                },
                seen_server_tick: 1,
                ..default()
            })
            .collect();

        // The client: each command stepped in turn from the spawn, as prediction steps them.
        let mut client = stepping_app();
        let prediction = join(&mut client);
        let start = client
            .world()
            .get::<PlayerState<TestGame>>(prediction)
            .unwrap()
            .moves
            .position;
        let mut previous = (0, Buttons::empty());
        for &command in &burst {
            client
                .world_mut()
                .run_system_once(move |mut stepper: Stepper<TestGame>| {
                    stepper
                        .player_move(
                            prediction,
                            &mut Outbox::default(),
                            command,
                            previous.0,
                            previous.1,
                        )
                        .unwrap();
                })
                .unwrap();
            previous = (command.sequence, command.buttons);
        }
        let predicted = *client
            .world()
            .get::<PlayerState<TestGame>>(prediction)
            .unwrap();

        // The server: the whole burst in the queue before the player's first tick.
        let mut server = plugin_app();
        server.finish();
        let owner = connected_client(&mut server);
        let player = spawn_player(&mut server, owner, SHOOTER, start);
        tick(&mut server, player, &burst);
        let snapshot = server
            .world()
            .get::<OwnerSnapshot<TestGame>>(player)
            .unwrap();
        assert_eq!(snapshot.last_processed_sequence, 5);
        assert!(!snapshot.provisional);
        assert_eq!(snapshot.moves, predicted.moves);
        assert_ne!(predicted.moves.position, start, "the burst moved it");
    }

    /// Pushes every player [`PUSH`] along X in [`NetAhoyServerSystems::Events`] on the ticks it
    /// is set: a game's reaction to a step's events.
    #[derive(Resource, Default)]
    struct PushNext(bool);

    const PUSH: f32 = 5.0;

    fn push_players(mut push: ResMut<PushNext>, mut players: Query<&mut PlayerState<TestGame>>) {
        if std::mem::take(&mut push.0) {
            for mut player in &mut players {
                player.moves.position.x += PUSH;
            }
        }
    }

    /// A reaction that edits a player after a tick that ran on a guess is a floor the player's
    /// late command can't roll back across: the guess stands as the input that ran, the edit
    /// holds, and the ack still crosses it. Without the edit the same late command rolls back.
    #[test]
    fn a_reaction_holds_against_a_late_command() {
        for pushed in [false, true] {
            let mut app = plugin_app();
            app.init_resource::<PushNext>().add_systems(
                FixedPreUpdate,
                push_players.in_set(NetAhoyServerSystems::Events),
            );
            app.finish();
            let client = connected_client(&mut app);
            let player = spawn_player(&mut app, client, SHOOTER, Vec3::new(0.0, 0.76, 0.0));
            for sequence in 1..=10 {
                tick(&mut app, player, &[cmd(sequence, false)]);
            }

            // 11 is late: its tick runs on a guess, and the reaction pushes the player after it.
            app.world_mut().resource_mut::<PushNext>().0 = pushed;
            tick(&mut app, player, &[]);
            tick(&mut app, player, &[cmd(11, false), cmd(12, false)]);

            let world = app.world();
            let stats = world.resource::<RollbackStats>();
            let buffer = world.get::<ServerCommandBuffer<TestGame>>(player).unwrap();
            let x = world
                .get::<PlayerState<TestGame>>(player)
                .unwrap()
                .moves
                .position
                .x;
            assert_eq!(buffer.last_processed_sequence, 12, "pushed {pushed}");
            if pushed {
                assert_eq!((stats.rollbacks, stats.floors), (0, 1));
                assert_eq!(x, PUSH, "the push holds");
            } else {
                assert_eq!((stats.rollbacks, stats.floors), (1, 0));
                assert_eq!(x, 0.0);
            }
        }
    }

    /// A game's reaction in [`NetAhoyServerSystems::Events`] edits a player's state after the
    /// steps, and the components everyone else reads the player by show the edit on that tick.
    #[test]
    fn a_reaction_in_events_reaches_the_players_components_the_same_tick() {
        use bevy_ahoy::CharacterLook;

        const PUSHED: Vec3 = Vec3::new(1.0, 2.0, 3.0);
        const MOVED: Vec3 = Vec3::new(4.0, 5.0, 6.0);

        let mut app = plugin_app();
        app.add_systems(
            FixedPreUpdate,
            (|mut players: Query<&mut PlayerState<TestGame>>| {
                for mut player in &mut players {
                    player.moves.velocity = PUSHED;
                    player.moves.position = MOVED;
                }
            })
            .in_set(NetAhoyServerSystems::Events),
        );
        app.finish();
        let player = app
            .world_mut()
            .spawn((
                SHOOTER,
                PlayerState::<TestGame>::at(Vec3::ZERO),
                CharacterLook::default(),
                Transform::default(),
                Position::default(),
                LinearVelocity::default(),
            ))
            .id();

        app.world_mut().run_schedule(FixedPreUpdate);
        let player = app.world().entity(player);
        assert_eq!(player.get::<LinearVelocity>().unwrap().0, PUSHED);
        assert_eq!(player.get::<Position>().unwrap().0, MOVED);
        assert_eq!(player.get::<Transform>().unwrap().translation, MOVED);
    }
}
