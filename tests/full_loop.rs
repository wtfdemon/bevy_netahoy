//! The whole loop, in one process over WebSocket on localhost: a server app with
//! `NetAhoyServerPlugin` and a client app with `NetAhoyClientPlugin`, for a small game of its own.
//! The client predicts its player and sends its commands, the server steps them and sends the
//! owner snapshots back, and reconciliation judges every ack. Walking and jumping, the
//! prediction holds; a server-side teleport is one correction, which the client replays.
//!
//! The server also runs a [`Beacon`], game state it keeps in a `History` on both peers, which
//! every step reads at its command's seen time through the game's context.
//!
//! Both apps step one 50 ms fixed tick of their own clock per update; the waits count updates,
//! with a short sleep in each for the sockets.

use std::{
    net::{Ipv4Addr, SocketAddr},
    thread::sleep,
    time::Duration,
};

use aeronet::io::{SessionEndpoint, connection::LocalAddr};
use aeronet_replicon::{
    client::{AeronetRepliconClient, AeronetRepliconClientPlugin},
    server::{AeronetRepliconServer, AeronetRepliconServerPlugin},
};
use aeronet_websocket::{
    client::{ClientConfig, WebSocketClient, WebSocketClientPlugin},
    server::{ServerConfig, WebSocketServer, WebSocketServerPlugin},
};
use avian3d::prelude::*;
use bevy::{
    app::RunFixedMainLoop, ecs::entity::MapEntities, prelude::*, state::app::StatesPlugin,
    time::TimeUpdateStrategy,
};
use bevy_ahoy::{input::AhoyInputPlugin, prelude::*};
use bevy_netahoy::{
    AppHistoryExt, Buttons, ClientInput, History, JoinRequest, KccSchedule, NetAhoyClientPlugin,
    NetAhoyClientSystems, NetAhoyProtocolPlugin, NetAhoyServerPlugin, NoEvents, OwnPlayer,
    OwnerSnapshot, PLAYER_COLLISION_LAYER, PlayerGame, PlayerId, PlayerOwner, PlayerShape,
    PlayerState, PredictedPlayer, Recorded, ServerTick, Step, TickRate, WORLD_COLLISION_LAYER,
    debug::{CorrectionMode, PredictionStatus},
    testing,
};
use bevy_replicon::prelude::*;
use serde::{Deserialize, Serialize};

/// The game: the stock move, a count of the steps and the beacon each step saw, which reconcile
/// with it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize, MapEntities)]
struct Walker {
    steps: u32,
    beacon: Option<u64>,
}

impl PlayerGame for Walker {
    type Event = NoEvents;
    /// The beacon's history, read at each command's seen time.
    type Context = Query<'static, 'static, &'static History<Beacon>>;

    fn step(player: &mut PlayerState<Self>, step: &mut Step<Self>) {
        step.pmove(&mut player.moves);
        player.game.steps += 1;
        player.game.beacon = step
            .context()
            .single()
            .ok()
            .and_then(|history| history.sample(step.seen_time()))
            .map(|beacon| beacon.from.0);
    }
}

/// Game state on the server's timer: the tick over [`Beacon::PERIOD`], written in the fixed
/// schedule only when it changes, so most ticks send nothing.
#[derive(Component, Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
struct Beacon(u64);

impl Recorded for Beacon {}

impl Beacon {
    const PERIOD: u64 = 7;
}

fn cycle_beacon(tick: Res<ServerTick>, mut beacons: Query<&mut Beacon>) {
    for mut beacon in &mut beacons {
        beacon.set_if_neq(Beacon(tick.0 / Beacon::PERIOD));
    }
}

/// One update of either app, in its own clock: one fixed tick.
const TICK: Duration = Duration::from_millis(50);
/// The wall time each update waits for the sockets.
const PAUSE: Duration = Duration::from_millis(2);

const PLAYER: PlayerId = PlayerId(1);

/// Both apps: the stepping stack, the protocol and the level, one fixed tick an update.
fn base_app() -> App {
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
        RepliconPlugins,
    ))
    .add_plugins(NetAhoyProtocolPlugin::<Walker>::new(TickRate::new(20)))
    .replicate_with_history::<Beacon>()
    .insert_resource(TimeUpdateStrategy::ManualDuration(TICK))
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

/// What steps a player, the same on both sides.
fn player_body(shape: &PlayerShape) -> impl Bundle {
    (
        CharacterController {
            filter: SpatialQueryFilter::from_mask(WORLD_COLLISION_LAYER),
            ..default()
        },
        shape.collider(),
        CollisionLayers::new(PLAYER_COLLISION_LAYER, LayerMask::ALL),
    )
}

fn server_app() -> App {
    let mut app = base_app();
    app.add_plugins((
        WebSocketServerPlugin,
        AeronetRepliconServerPlugin,
        NetAhoyServerPlugin::<Walker>::default(),
    ))
    .add_observer(
        |join: On<FromClient<JoinRequest>>, mut commands: Commands, shape: Res<PlayerShape>| {
            let Some(client) = join.client_id.entity() else {
                return;
            };
            commands.spawn((
                PLAYER,
                PlayerOwner(client),
                PlayerState::<Walker>::at(Vec3::new(0.0, 1.0, 0.0)),
                player_body(&shape),
            ));
        },
    )
    .add_systems(FixedUpdate, cycle_beacon);
    app.world_mut().spawn((Replicated, Beacon::default()));
    app.world_mut()
        .commands()
        .spawn(AeronetRepliconServer)
        .queue(WebSocketServer::open(
            ServerConfig::builder()
                .with_bind_address(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
                .with_no_encryption(),
        ));
    app.world_mut().flush();
    app.finish();
    app.cleanup();
    app
}

/// Every verdict reconciliation reached, in order: one per owner snapshot it judged.
#[derive(Resource, Default)]
struct Verdicts(Vec<Verdict>);

#[derive(Clone, Copy, Debug, PartialEq)]
struct Verdict {
    mode: CorrectionMode,
    ack: u32,
    replayed: usize,
}

impl Verdict {
    /// Whether it rewound and replayed.
    fn corrected(self) -> bool {
        matches!(
            self.mode,
            CorrectionMode::Replayed | CorrectionMode::Snapped | CorrectionMode::MissingHistory
        )
    }
}

fn record_verdict(
    predictions: Query<&PredictionStatus, With<PredictedPlayer>>,
    mut verdicts: ResMut<Verdicts>,
    mut last_tick: Local<u64>,
) {
    let Ok(status) = predictions.single() else {
        return;
    };
    if status.last_server_tick() == *last_tick {
        return;
    }
    *last_tick = status.last_server_tick();
    if status.mode() != CorrectionMode::Waiting {
        verdicts.0.push(Verdict {
            mode: status.mode(),
            ack: status.last_ack_sequence(),
            replayed: status.replayed_commands(),
        });
    }
}

/// Walk forward, and hop every second.
fn walk(time: Res<Time>, mut input: ResMut<ClientInput>, mut next_jump: Local<f32>) {
    input.movement = Vec2::Y;
    input.look.x = 0.3;
    if time.elapsed_secs() >= *next_jump {
        *next_jump = time.elapsed_secs() + 1.0;
        input.pressed |= Buttons::JUMP;
    }
}

fn client_app(port: u16) -> App {
    let mut app = base_app();
    app.add_plugins((
        WebSocketClientPlugin,
        AeronetRepliconClientPlugin,
        NetAhoyClientPlugin::<Walker>::default(),
    ))
    .init_resource::<Verdicts>()
    .add_observer(
        |session: On<Add, SessionEndpoint>, mut commands: Commands| {
            commands
                .entity(session.event_target())
                .insert(AeronetRepliconClient);
        },
    )
    .add_observer(
        |ours: On<Add, OwnPlayer>, mut commands: Commands, shape: Res<PlayerShape>| {
            let server_entity = ours.event_target();
            commands.spawn((PredictedPlayer { server_entity }, player_body(&shape)));
        },
    )
    .add_systems(
        RunFixedMainLoop,
        walk.in_set(NetAhoyClientSystems::GatherInput),
    )
    .add_systems(
        FixedPreUpdate,
        record_verdict
            .after(NetAhoyClientSystems::Reconcile)
            .before(NetAhoyClientSystems::Predict),
    );
    app.finish();
    app.cleanup();
    app.world_mut()
        .commands()
        .spawn_empty()
        .queue(WebSocketClient::connect(
            ClientConfig::builder().with_no_encryption(),
            format!("ws://127.0.0.1:{port}"),
        ));
    app.world_mut().flush();
    app
}

/// Update both apps an update at a time until `done`, or panic after `updates`.
fn pump_until(
    server: &mut App,
    client: &mut App,
    updates: u32,
    what: &str,
    mut done: impl FnMut(&mut App, &mut App) -> bool,
) {
    for _ in 0..updates {
        server.update();
        client.update();
        if done(server, client) {
            return;
        }
        sleep(PAUSE);
    }
    panic!("timed out waiting for: {what}");
}

fn pump_for(server: &mut App, client: &mut App, updates: u32) {
    for _ in 0..updates {
        server.update();
        client.update();
        sleep(PAUSE);
    }
}

/// The port `server` listens on, once its listener is up.
fn bound_port(server: &mut App) -> u16 {
    for _ in 0..2000 {
        server.update();
        let world = server.world_mut();
        if let Some(addr) = world
            .query_filtered::<&LocalAddr, With<WebSocketServer>>()
            .iter(world)
            .next()
        {
            return addr.port();
        }
        sleep(PAUSE);
    }
    panic!("the listener never came up");
}

/// The newest ack the client's prediction was judged at.
fn ack(client: &mut App) -> u32 {
    let world = client.world_mut();
    world
        .query_filtered::<&PredictionStatus, With<PredictedPlayer>>()
        .single(world)
        .map_or(0, |status| status.last_ack_sequence())
}

fn server_player(server: &mut App) -> Entity {
    let world = server.world_mut();
    world
        .query_filtered::<Entity, With<PlayerOwner>>()
        .single(world)
        .unwrap()
}

fn predicted(client: &mut App) -> PlayerState<Walker> {
    let world = client.world_mut();
    *world
        .query_filtered::<&PlayerState<Walker>, With<PredictedPlayer>>()
        .single(world)
        .unwrap()
}

#[test]
fn the_client_predicts_and_the_server_agrees_until_it_teleports_the_player() {
    let mut server = server_app();
    let port = bound_port(&mut server);
    let mut client = client_app(port);
    pump_until(
        &mut server,
        &mut client,
        2000,
        "the first ack",
        |_, client| ack(client) > 0,
    );

    // Three seconds of walking and hopping: every ack agrees with the prediction.
    let from = predicted(&mut client);
    pump_for(&mut server, &mut client, 60);
    let walked = predicted(&mut client);
    assert!(
        walked.moves.position.distance(from.moves.position) > 5.0,
        "the prediction walked: {from:?} to {walked:?}"
    );
    assert!(walked.game.steps > from.game.steps + 50);
    assert!(
        walked.game.beacon > from.game.beacon,
        "the steps saw the beacon move on: {from:?} to {walked:?}"
    );
    let verdicts = std::mem::take(&mut client.world_mut().resource_mut::<Verdicts>().0);
    let corrections: Vec<_> = verdicts
        .iter()
        .filter(|verdict| verdict.corrected())
        .collect();
    assert_eq!(
        corrections,
        [] as [&Verdict; 0],
        "mispredicted while walking"
    );
    assert!(
        verdicts.len() >= 40,
        "owner snapshots came back and were judged: {} verdicts",
        verdicts.len()
    );
    let player = server_player(&mut server);
    let snapshot = *server.world().get::<OwnerSnapshot<Walker>>(player).unwrap();
    assert!(testing::ack(&snapshot) >= ack(&mut client));

    // The server moves the player 1.5 m aside, outside any command: one correction, replayed
    // from the ack it shows up at, then agreement again, every ack judged on the moved player.
    server
        .world_mut()
        .get_mut::<PlayerState<Walker>>(player)
        .unwrap()
        .moves
        .position
        .x += 1.5;
    pump_for(&mut server, &mut client, 40);
    let verdicts = &client.world().resource::<Verdicts>().0;
    let corrections: Vec<_> = verdicts
        .iter()
        .filter(|verdict| verdict.corrected())
        .collect();
    assert_eq!(corrections.len(), 1, "one correction: {verdicts:?}");
    assert!(
        corrections[0].replayed > 0 && corrections[0].ack > testing::ack(&snapshot),
        "replayed, from an ack past the teleport: {:?}",
        corrections[0]
    );
    let after = verdicts
        .iter()
        .position(|verdict| verdict.corrected())
        .unwrap();
    assert!(
        verdicts[after + 1..]
            .iter()
            .all(|verdict| verdict.mode == CorrectionMode::Ignored),
        "agreement after it: {verdicts:?}"
    );
    assert!(
        verdicts.len() > after + 20,
        "judged on after it: {verdicts:?}"
    );
}

fn beacon_history(app: &mut App) -> History<Beacon> {
    let world = app.world_mut();
    world
        .query::<&History<Beacon>>()
        .single(world)
        .unwrap()
        .clone()
}

/// A game's own history: the server records the beacon every tick as that tick left it, the
/// client holds what replication brings through the quiet ticks, and both have the same entry
/// at every tick they share.
#[test]
fn a_game_history_has_the_same_entry_on_both_peers() {
    let mut server = server_app();
    let port = bound_port(&mut server);
    let mut client = client_app(port);
    pump_until(
        &mut server,
        &mut client,
        2000,
        "the first ack",
        |_, client| ack(client) > 0,
    );
    pump_for(&mut server, &mut client, 60);

    let on_server = beacon_history(&mut server);
    let on_client = beacon_history(&mut client);
    assert_eq!(on_server.len(), on_server.capacity(), "a value every tick");
    for (tick, beacon) in on_server.iter() {
        assert_eq!(beacon.0, tick / Beacon::PERIOD, "tick {tick}: what it left");
    }
    let (newest, _) = on_client.newest().unwrap();
    // The newest two may still be held over a mutation that is on its way.
    let shared: Vec<_> = on_client
        .iter()
        .filter(|(tick, _)| *tick + 2 <= newest)
        .map(|(tick, beacon)| (tick, *beacon, *on_server.at_or_before(tick).unwrap().1))
        .collect();
    assert!(shared.len() >= 30, "{} shared ticks", shared.len());
    for (tick, client, server) in shared {
        assert_eq!(client, server, "tick {tick}");
    }
}
