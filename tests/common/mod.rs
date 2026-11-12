//! The step harness the step tests share: `Stepper::player_move` over a real avian level, with
//! bodies seen from their snapshot history at each command's seen time, as the server and client
//! prediction run it.

use std::time::Duration;

use avian3d::prelude::*;
use bevy::{
    ecs::{
        message::Messages,
        system::{RunSystemOnce, SystemState},
    },
    prelude::*,
    time::TimeUpdateStrategy,
};
use bevy_ahoy::{CharacterLook, input::AhoyInputPlugin, prelude::*};
use bevy_netahoy::{
    Buttons, History, KccSchedule, OwnerSnapshot, PLAYER_COLLISION_LAYER, PlayerGame, PlayerId,
    PlayerShape, PlayerSnapshot, PlayerState, ServerTick, StepEvent, TickRate, UserCmd,
    WORLD_COLLISION_LAYER,
    testing::{self, Outbox, PredictionFrame, Stepper},
};
use playground::{Arsenal, Shot};

pub const PLAYER: PlayerId = PlayerId(1);
/// The default shape's cylinder: its center stands this far above what it stands on.
pub const HALF_HEIGHT: f32 = 0.75;

pub fn shape() -> PlayerShape {
    PlayerShape::default()
}

pub fn controller() -> CharacterController {
    CharacterController {
        filter: SpatialQueryFilter::from_mask(WORLD_COLLISION_LAYER),
        acceleration_hz: 10.0,
        speed: 6.5,
        gravity: 23.0,
        friction_hz: 4.0,
        ..default()
    }
}

/// A 200 m square floor and [`PLAYER`] at `start`, stepping at the default tick rate.
pub fn make_app(start: Vec3) -> App {
    make_app_at(start, TickRate::default())
}

/// [`make_app`] stepping at `rate`.
pub fn make_app_at(start: Vec3, rate: TickRate) -> App {
    game_app::<Arsenal>(start, rate)
}

/// [`make_app_at`] for the game `G`.
pub fn game_app<G: PlayerGame>(start: Vec3, rate: TickRate) -> App {
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
    .insert_resource(shape())
    .insert_resource(Time::<Fixed>::from_hz(f64::from(rate.hz())))
    .insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(
        rate.seconds(),
    )))
    .insert_resource(ServerTick(1))
    .add_message::<G::Event>()
    // Avian's diagnostics resources, which DefaultPlugins would bring.
    .init_resource::<avian3d::spatial_query::SpatialQueryDiagnostics>()
    .init_resource::<avian3d::collider_tree::ColliderTreeDiagnostics>()
    .init_resource::<avian3d::collision::CollisionDiagnostics>()
    .init_resource::<avian3d::dynamics::solver::SolverDiagnostics>();

    let world = app.world_mut();
    world.spawn((
        Transform::from_translation(Vec3::new(0.0, -0.5, 0.0)),
        RigidBody::Static,
        Collider::cuboid(200.0, 1.0, 200.0),
        CollisionLayers::new(WORLD_COLLISION_LAYER, LayerMask::ALL),
    ));
    world.spawn((
        PLAYER,
        PlayerState::<G>::at(start),
        CharacterLook::default(),
        controller(),
        shape().collider(),
        CollisionLayers::new(PLAYER_COLLISION_LAYER, LayerMask::ALL),
        Transform::from_translation(start),
    ));
    // Let avian and Ahoy set up colliders, the query trees and the hulls.
    for _ in 0..4 {
        app.update();
    }
    app
}

pub fn player(world: &mut World) -> Entity {
    game_player::<Arsenal>(world)
}

/// [`player`] for the game `G`.
pub fn game_player<G: PlayerGame>(world: &mut World) -> Entity {
    world
        .query_filtered::<Entity, With<PlayerState<G>>>()
        .single(world)
        .unwrap()
}

pub fn command(sequence: u32, movement: Vec2, buttons: Buttons) -> UserCmd {
    UserCmd {
        sequence,
        movement,
        buttons,
        seen_server_tick: u64::from(sequence),
        ..default()
    }
}

/// Step `commands` one system run each, like the fixed loop, each on a timeline of its own (all
/// its events written); the frame after each.
pub fn run(app: &mut App, commands: &[UserCmd]) -> Vec<PredictionFrame<Arsenal>> {
    run_game::<Arsenal>(app, commands)
}

/// [`run`] for the game `G`.
pub fn run_game<G: PlayerGame>(app: &mut App, commands: &[UserCmd]) -> Vec<PredictionFrame<G>> {
    let world = app.world_mut();
    let entity = game_player::<G>(world);
    let mut previous = Buttons::empty();
    commands
        .iter()
        .map(|&command| {
            let frame = world
                .run_system_once(move |mut stepper: Stepper<G>| {
                    testing::player_move(
                        &mut stepper,
                        entity,
                        &mut Outbox::default(),
                        command,
                        command.sequence - 1,
                        previous,
                    )
                    .unwrap();
                    stepper.capture_frame(entity, command).unwrap()
                })
                .unwrap();
            previous = command.buttons;
            frame
        })
        .collect()
}

pub fn idle(sequences: std::ops::RangeInclusive<u32>) -> Vec<UserCmd> {
    sequences
        .map(|seq| command(seq, Vec2::ZERO, Buttons::empty()))
        .collect()
}

/// One command, its events through `outbox` (its timeline's), then the frame after it.
pub fn run_one(
    app: &mut App,
    outbox: &mut Outbox<Shot>,
    command: UserCmd,
    previous: Buttons,
) -> PredictionFrame<Arsenal> {
    run_one_game::<Arsenal>(app, outbox, command, previous)
}

/// [`run_one`] for the game `G`.
pub fn run_one_game<G: PlayerGame>(
    app: &mut App,
    outbox: &mut Outbox<G::Event>,
    command: UserCmd,
    previous: Buttons,
) -> PredictionFrame<G> {
    let world = app.world_mut();
    let entity = game_player::<G>(world);
    let mut state = SystemState::<Stepper<G>>::new(world);
    let mut stepper = state.get_mut(world).unwrap();
    testing::player_move(
        &mut stepper,
        entity,
        outbox,
        command,
        command.sequence - 1,
        previous,
    )
    .unwrap();
    stepper.capture_frame(entity, command).unwrap()
}

/// The events the steps wrote since the last call, in order.
pub fn written<E: StepEvent>(app: &mut App) -> Vec<E> {
    app.world_mut()
        .resource_mut::<Messages<E>>()
        .drain()
        .collect()
}

/// Put the player back to `frame` (the client's rewind to an ack).
pub fn rewind<G: PlayerGame>(app: &mut App, frame: PredictionFrame<G>) {
    let world = app.world_mut();
    let entity = game_player::<G>(world);
    world
        .run_system_once(move |mut stepper: Stepper<G>| {
            let mut snapshot = OwnerSnapshot::default();
            snapshot.moves = frame.state.moves;
            snapshot.game = frame.state.game;
            testing::restore(&mut stepper, entity, &snapshot);
        })
        .unwrap();
}

/// Another player, known only by its pose history: at `path(tick)` for ticks 1 to `ticks`.
pub fn spawn_seen_player(app: &mut App, id: PlayerId, ticks: u64, path: impl Fn(u64) -> Vec3) {
    let world = app.world_mut();
    let player = world.spawn((id, PlayerSnapshot::default())).id();
    let mut history = world.get_mut::<History<PlayerSnapshot>>(player).unwrap();
    for tick in 1..=ticks {
        let sample = PlayerSnapshot {
            server_tick: tick,
            position: path(tick),
            ..default()
        };
        testing::record(&mut history, tick, sample);
    }
}

/// A wall at `distance` ahead, 20 m wide and 4 m high.
pub fn spawn_wall(app: &mut App, distance: f32) {
    app.world_mut().spawn((
        Transform::from_translation(Vec3::new(0.0, 2.0, -distance - 0.5)),
        RigidBody::Static,
        Collider::cuboid(20.0, 4.0, 1.0),
        CollisionLayers::new(WORLD_COLLISION_LAYER, LayerMask::ALL),
    ));
    for _ in 0..3 {
        app.update();
    }
}
