//! Reconnect over real sockets: a server app and client apps in one process, talking WebSocket
//! on localhost through aeronet and replicon, with netahoy's session plugins on both ends.
//!
//! Each test breaks the link a different way and asserts the client comes back on its own, with
//! exactly one copy of the replicated world (replicon keeps old entities on disconnect; a
//! reconnect without the session reset would duplicate it).
//!
//! The server listens on a port the OS picks, and every app's clock runs a fixed `FRAME` per
//! update: the timeouts count frames, so a slow machine can't fire one early.

use std::{
    net::{Ipv4Addr, SocketAddr},
    thread::sleep,
    time::Duration,
};

use aeronet::io::{
    SessionEndpoint,
    connection::{Disconnect, Disconnected, LocalAddr},
};
use aeronet_replicon::{
    client::{AeronetRepliconClient, AeronetRepliconClientPlugin},
    server::{AeronetRepliconServer, AeronetRepliconServerPlugin},
};
use aeronet_websocket::{
    client::{ClientConfig, WebSocketClient, WebSocketClientPlugin},
    server::{ServerConfig, WebSocketServer, WebSocketServerPlugin},
};
use bevy::{prelude::*, state::app::StatesPlugin, time::TimeUpdateStrategy};
use bevy_netahoy::{
    ClientTimeout, JoinRequest, NetAhoyProtocolPlugin, OwnerSnapshot, Reconnect, RejoinToken,
    ServerPlayer, TickRate,
    testing::{HeldRejoinToken, NetAhoyClientSessionPlugin, NetAhoyServerSessionPlugin},
};
use bevy_replicon::{client::Remote, prelude::*};
use playground::Arsenal;

/// One update of any app here, in its own clock and (at least) on the wall.
const FRAME: Duration = Duration::from_millis(5);

#[derive(Resource)]
struct Port(u16);

#[derive(Resource, Default)]
struct Joins(Vec<Option<RejoinToken>>);

#[derive(Resource, Default)]
struct Drops(u32);

fn server_app() -> App {
    let mut app = App::new();
    app.add_plugins((
        MinimalPlugins,
        StatesPlugin,
        RepliconPlugins.set(ServerPlugin::new(PostUpdate)),
        WebSocketServerPlugin,
        AeronetRepliconServerPlugin,
        NetAhoyProtocolPlugin::<Arsenal>::default(),
        NetAhoyServerSessionPlugin,
    ))
    .insert_resource(ClientTimeout(Duration::from_millis(1500)))
    .insert_resource(TimeUpdateStrategy::ManualDuration(FRAME))
    .init_resource::<Joins>()
    .init_resource::<Drops>()
    .add_observer(
        |join: On<FromClient<JoinRequest>>, mut joins: ResMut<Joins>| {
            joins.0.push(join.rejoin);
        },
    )
    .add_observer(|_: On<Disconnected>, mut drops: ResMut<Drops>| drops.0 += 1)
    // Something changes every frame, as snapshots do, so the link is never legitimately quiet.
    .add_systems(Update, |mut players: Query<&mut OwnerSnapshot<Arsenal>>| {
        for mut snapshot in &mut players {
            snapshot.server_tick += 1;
        }
    });
    app.world_mut()
        .commands()
        .spawn((Name::new("Server"), AeronetRepliconServer))
        .queue(WebSocketServer::open(
            ServerConfig::builder()
                .with_bind_address(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
                .with_no_encryption(),
        ));
    app.world_mut().flush();
    app.world_mut().spawn((
        Replicated,
        ServerPlayer,
        OwnerSnapshot::<Arsenal>::default(),
    ));
    app.finish();
    app.cleanup();
    app
}

fn connect(mut commands: Commands, port: Res<Port>) {
    commands
        .spawn(Name::new("Client"))
        .queue(WebSocketClient::connect(
            ClientConfig::builder().with_no_encryption(),
            format!("ws://127.0.0.1:{}", port.0),
        ));
}

fn client_app(port: u16) -> App {
    client_app_at(port, TickRate::default())
}

/// A client stepping at `rate`: at another rate than the server's, another protocol.
fn client_app_at(port: u16, rate: TickRate) -> App {
    let mut app = App::new();
    app.add_plugins((
        MinimalPlugins,
        StatesPlugin,
        RepliconPlugins,
        WebSocketClientPlugin,
        AeronetRepliconClientPlugin,
        NetAhoyProtocolPlugin::<Arsenal>::new(rate),
        NetAhoyClientSessionPlugin,
    ))
    .insert_resource(Port(port))
    .insert_resource(TimeUpdateStrategy::ManualDuration(FRAME))
    .init_resource::<Drops>()
    .add_observer(
        |session: On<Add, SessionEndpoint>, mut commands: Commands| {
            commands
                .entity(session.event_target())
                .insert(AeronetRepliconClient);
        },
    )
    .add_observer(|_: On<Disconnected>, mut drops: ResMut<Drops>| drops.0 += 1);
    let connect = app.register_system(connect);
    let mut reconnect = Reconnect::new(connect);
    reconnect.server_silence = Duration::from_millis(800);
    reconnect.connect_timeout = Duration::from_secs(3);
    reconnect.backoff_min = Duration::from_millis(100);
    app.insert_resource(reconnect);
    app.finish();
    app.cleanup();
    app
}

fn connected(app: &App) -> bool {
    *app.world().resource::<State<ClientState>>().get() == ClientState::Connected
}

/// Replicated players this client holds. One server entity, so anything
/// but 1 once connected is a stale duplicate (or a missing world).
fn players(app: &mut App) -> usize {
    app.world_mut()
        .query_filtered::<(), (With<Remote>, With<ServerPlayer>)>()
        .iter(app.world())
        .count()
}

fn rejoin_token(app: &App) -> Option<RejoinToken> {
    app.world().resource::<HeldRejoinToken>().0
}

/// Update `apps` a frame at a time until `done` (panics after `frames`); the frames it took.
fn pump_until(
    apps: &mut [&mut App],
    frames: u32,
    what: &str,
    mut done: impl FnMut(&mut [&mut App]) -> bool,
) -> u32 {
    for frame in 1..=frames {
        for app in apps.iter_mut() {
            app.update();
        }
        if done(apps) {
            return frame;
        }
        sleep(FRAME);
    }
    panic!("timed out waiting for: {what}");
}

fn pump_for(apps: &mut [&mut App], frames: u32) {
    for _ in 0..frames {
        for app in apps.iter_mut() {
            app.update();
        }
        sleep(FRAME);
    }
}

/// One update of `app` carrying `frames` frames at once: the first frame of a process that was
/// stopped for them.
fn resume(app: &mut App, frames: u32) {
    app.insert_resource(TimeUpdateStrategy::ManualDuration(FRAME * frames));
    app.update();
    app.insert_resource(TimeUpdateStrategy::ManualDuration(FRAME));
}

/// The port `server` listens on, once its listener is up.
fn bound_port(server: &mut App) -> u16 {
    let mut port = None;
    pump_until(&mut [server], 2000, "the listener", |apps| {
        port = apps[0]
            .world_mut()
            .query_filtered::<&LocalAddr, With<WebSocketServer>>()
            .iter(apps[0].world())
            .next()
            .map(|addr| addr.port());
        port.is_some()
    });
    port.unwrap()
}

/// Connected, joined (token in hand) and holding exactly one world.
fn settled(client: &mut App) -> bool {
    connected(client) && rejoin_token(client).is_some() && players(client) == 1
}

/// A server and a client connected to it, settled; and the server's port.
fn boot() -> (App, App, u16) {
    let mut server = server_app();
    let port = bound_port(&mut server);
    let mut client = client_app(port);
    client.world_mut().run_system_once(connect).unwrap();
    pump_until(
        &mut [&mut server, &mut client],
        2000,
        "first connect",
        |apps| settled(apps[1]),
    );
    (server, client, port)
}

trait RunOnce {
    fn run_system_once<M>(
        &mut self,
        system: impl IntoSystem<(), (), M>,
    ) -> Result<(), bevy::ecs::system::RunSystemError>;
}

impl RunOnce for World {
    fn run_system_once<M>(
        &mut self,
        system: impl IntoSystem<(), (), M>,
    ) -> Result<(), bevy::ecs::system::RunSystemError> {
        bevy::ecs::system::RunSystemOnce::run_system_once(self, system)
    }
}

fn server_sessions(server: &mut App) -> usize {
    server
        .world_mut()
        .query_filtered::<(), With<ConnectedClient>>()
        .iter(server.world())
        .count()
}

#[test]
fn kicked_client_reconnects_into_one_clean_world() {
    let (mut server, mut client, _) = boot();
    let first_token = rejoin_token(&client);

    // The server drops the connection (what a memory-limit disconnect or a
    // restart looks like from the client).
    let session = server
        .world_mut()
        .query_filtered::<Entity, With<ConnectedClient>>()
        .single(server.world())
        .unwrap();
    server
        .world_mut()
        .trigger(Disconnect::new(session, "test kick"));

    pump_until(
        &mut [&mut server, &mut client],
        2000,
        "client noticed the kick",
        |apps| apps[1].world().resource::<Drops>().0 >= 1,
    );
    pump_until(
        &mut [&mut server, &mut client],
        2000,
        "reconnected into one world",
        |apps| settled(apps[1]) && rejoin_token(apps[1]) != first_token,
    );
    // Hold it there a moment: no late duplicate spawns.
    pump_for(&mut [&mut server, &mut client], 60);
    assert_eq!(players(&mut client), 1);
    assert_eq!(server_sessions(&mut server), 1);
    let joins = &server.world().resource::<Joins>().0;
    assert_eq!(joins.len(), 2);
    assert_eq!(joins[1], first_token, "rejoin presented the old token");
}

#[test]
fn silent_server_is_dropped_and_rejoined() {
    let (mut server, mut client, _) = boot();

    // The server stops sending anything at all (hung process, or a socket
    // stuck open behind a dead route): only the client runs.
    let hung = pump_until(&mut [&mut client], 1000, "client watchdog fired", |apps| {
        apps[0].world().resource::<Drops>().0 >= 1
    });
    resume(&mut server, hung);
    pump_until(
        &mut [&mut server, &mut client],
        2000,
        "reconnected after the server came back",
        settled_client_at(1),
    );
    pump_for(&mut [&mut server, &mut client], 60);
    assert_eq!(players(&mut client), 1);
    assert_eq!(server_sessions(&mut server), 1, "old connection cleaned up");
}

#[test]
fn silent_client_is_timed_out_and_comes_back() {
    let (mut server, mut client, _) = boot();

    // The client stops entirely (hidden tab, frozen process): only the
    // server runs, and drops it after its ClientTimeout.
    let hidden = pump_until(
        &mut [&mut server],
        1000,
        "server timed the client out",
        |apps| apps[0].world().resource::<Drops>().0 >= 1,
    );
    assert_eq!(server_sessions(&mut server), 0);
    resume(&mut client, hidden);
    pump_until(
        &mut [&mut server, &mut client],
        2000,
        "client came back",
        settled_client_at(1),
    );
    pump_for(&mut [&mut server, &mut client], 60);
    assert_eq!(players(&mut client), 1);
    assert_eq!(server_sessions(&mut server), 1);
}

#[test]
fn rejoin_token_replaces_a_stale_connection() {
    let (mut server, mut client, port) = boot();
    let token = rejoin_token(&client);

    // A second connection presents the first one's token: from the server's
    // side that's the same player back on a new socket while the old one
    // still looks alive. The old one must go, not linger as a ghost.
    let mut newcomer = client_app(port);
    newcomer.insert_resource(HeldRejoinToken(token));
    newcomer.world_mut().run_system_once(connect).unwrap();
    pump_until(
        &mut [&mut server, &mut client, &mut newcomer],
        2000,
        "stale connection dropped",
        |apps| apps[1].world().resource::<Drops>().0 >= 1,
    );
    // The replaced client is a real client too: it just reconnects.
    pump_until(
        &mut [&mut server, &mut client, &mut newcomer],
        2000,
        "both settled",
        |apps| {
            let (first, rest) = apps.split_at_mut(2);
            settled(first[1]) && settled(rest[0])
        },
    );
    pump_for(&mut [&mut server, &mut client, &mut newcomer], 60);
    assert_eq!(server_sessions(&mut server), 2);
    assert_eq!(players(&mut client), 1);
    assert_eq!(players(&mut newcomer), 1);
}

fn settled_client_at(index: usize) -> impl FnMut(&mut [&mut App]) -> bool {
    move |apps| settled(apps[index])
}

/// A client at another tick rate speaks another protocol: the server refuses it, issues it no
/// rejoin token and keeps no session for it, and the client, told why, stops reconnecting
/// rather than knocking every few seconds.
#[test]
fn a_client_with_another_protocol_is_refused_and_stops_trying() {
    let mut server = server_app();
    let port = bound_port(&mut server);
    let mut client = client_app_at(port, TickRate::new(128));
    client.world_mut().run_system_once(connect).unwrap();
    pump_until(
        &mut [&mut server, &mut client],
        2000,
        "the refusal",
        |apps| !apps[1].world().contains_resource::<Reconnect>(),
    );
    // Long past the client's backoff: nothing comes back.
    pump_for(&mut [&mut server, &mut client], 400);
    assert_eq!(server_sessions(&mut server), 0, "no session kept");
    assert_eq!(rejoin_token(&client), None, "no token issued");
    assert!(!connected(&client));
}
