//! Demo seeking: record a real session (server and recording client in one process, over
//! WebSocket on localhost), play it back, and check that a seek to any time gives exactly the
//! world straight playback has then: a backward seek keeps nothing from the future (an entity
//! spawned after the target, newer component values) and doesn't duplicate the world, however
//! often it's done.
//!
//! Every app steps a fixed frame of its own clock per update, the recording's timestamps
//! included, so the waits count updates, with a short sleep in each for the sockets.

use std::{
    net::{Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
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
use bevy::{prelude::*, state::app::StatesPlugin, time::TimeUpdateStrategy};
use bevy_netahoy::{
    DemoPlayback, DemoSeeked, NetAhoyDemoPlaybackPlugin, NetAhoyDemoRecordPlugin,
    NetAhoyProtocolPlugin,
    testing::{NetAhoyClientSessionPlugin, NetAhoyServerSessionPlugin},
};
use bevy_replicon::{client::Remote, prelude::*};
use playground::Arsenal;
use serde::{Deserialize, Serialize};

/// Counts server frames; replicated every frame, so its value is a clock.
#[derive(Component, Serialize, Deserialize, Clone, Copy, PartialEq, Debug)]
struct Counter(u32);

/// Lives for a window of server frames: spawned, then despawned.
#[derive(Component, Serialize, Deserialize, Clone, Copy, PartialEq, Debug)]
struct Spark(u32);

const SPARK_FROM: u32 = 30;
const SPARK_TO: u32 = 60;

/// Mutated once ([`LOOK_AT`]) on the counter's entity, which gets [`Tag`]
/// inserted later ([`TAG_AT`]): a seek past both must still show the
/// mutation, though the later update message outdates it (a player's
/// cosmetics, changed once, then any insert on the player).
#[derive(Component, Serialize, Deserialize, Clone, Copy, PartialEq, Debug)]
struct Look(u32);

#[derive(Component, Serialize, Deserialize, Clone, Copy, PartialEq, Debug)]
struct Tag;

const LOOK_AT: u32 = 20;
const TAG_AT: u32 = 40;

/// Where the story ends: the counter stops here.
const END: u32 = SPARK_TO + 31;

fn protocol(app: &mut App) {
    app.replicate::<Counter>()
        .replicate::<Spark>()
        .replicate::<Look>()
        .replicate::<Tag>();
}

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
    .insert_resource(TimeUpdateStrategy::ManualDuration(STEP));
    protocol(&mut app);
    app.add_systems(
        Update,
        |mut commands: Commands,
         clients: Query<(), With<ConnectedClient>>,
         mut counters: Query<(Entity, &mut Counter, &mut Look)>,
         sparks: Query<Entity, With<Spark>>| {
            // Nothing counts before the recorder connects, so its demo holds every step.
            if clients.is_empty() {
                return;
            }
            for (entity, mut counter, mut look) in &mut counters {
                if counter.0 == END {
                    continue;
                }
                counter.0 += 1;
                if counter.0 == LOOK_AT {
                    look.0 = 7;
                }
                if counter.0 == TAG_AT {
                    commands.entity(entity).insert(Tag);
                }
                if counter.0 == SPARK_FROM {
                    commands.spawn((Replicated, Spark(counter.0)));
                }
                if counter.0 == SPARK_TO {
                    for spark in &sparks {
                        commands.entity(spark).despawn();
                    }
                }
            }
        },
    );
    app.world_mut()
        .commands()
        .spawn((Name::new("Server"), AeronetRepliconServer))
        .queue(WebSocketServer::open(
            ServerConfig::builder()
                .with_bind_address(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
                .with_no_encryption(),
        ));
    app.world_mut().flush();
    app.world_mut().spawn((Replicated, Counter(0), Look(0)));
    app.finish();
    app.cleanup();
    app
}

fn recorder_app(port: u16, path: PathBuf) -> App {
    let mut app = App::new();
    app.add_plugins((
        MinimalPlugins,
        StatesPlugin,
        RepliconPlugins,
        WebSocketClientPlugin,
        AeronetRepliconClientPlugin,
        NetAhoyProtocolPlugin::<Arsenal>::default(),
        NetAhoyClientSessionPlugin,
        NetAhoyDemoRecordPlugin { path },
    ))
    // The recording's timestamps are this clock's: one frame an update.
    .insert_resource(TimeUpdateStrategy::ManualDuration(STEP));
    protocol(&mut app);
    app.add_observer(
        |session: On<Add, SessionEndpoint>, mut commands: Commands| {
            commands
                .entity(session.event_target())
                .insert(AeronetRepliconClient);
        },
    );
    app.finish();
    app.cleanup();
    app.world_mut()
        .commands()
        .spawn(Name::new("Client"))
        .queue(WebSocketClient::connect(
            ClientConfig::builder().with_no_encryption(),
            format!("ws://127.0.0.1:{port}"),
        ));
    app.world_mut().flush();
    app
}

const STEP: Duration = Duration::from_micros(16_667);

fn playback_app(path: PathBuf) -> App {
    let mut app = App::new();
    app.add_plugins((
        MinimalPlugins,
        StatesPlugin,
        RepliconPlugins,
        NetAhoyProtocolPlugin::<Arsenal>::default(),
        NetAhoyClientSessionPlugin,
        NetAhoyDemoPlaybackPlugin::file(path),
    ));
    protocol(&mut app);
    app.insert_resource(TimeUpdateStrategy::ManualDuration(STEP))
        .init_resource::<Seeks>()
        .add_systems(
            Update,
            |mut seeked: MessageReader<DemoSeeked>,
             playback: Res<DemoPlayback>,
             mut seeks: ResMut<Seeks>| {
                for seek in seeked.read() {
                    assert!(playback.bursting(), "a seek's frame is a burst");
                    seeks.0.push(*seek);
                }
            },
        );
    app.finish();
    app.cleanup();
    app
}

#[derive(Resource, Default)]
struct Seeks(Vec<DemoSeeked>);

/// What a viewer could see: every replicated entity's state.
#[derive(PartialEq, Debug)]
struct Seen {
    remotes: usize,
    counters: Vec<u32>,
    sparks: Vec<u32>,
    looks: Vec<u32>,
    tags: usize,
}

fn seen(app: &mut App) -> Seen {
    let world = app.world_mut();
    let remotes = world
        .query_filtered::<(), With<Remote>>()
        .iter(world)
        .count();
    let counters = world.query::<&Counter>().iter(world).map(|c| c.0).collect();
    let sparks = world.query::<&Spark>().iter(world).map(|s| s.0).collect();
    let looks = world.query::<&Look>().iter(world).map(|l| l.0).collect();
    let tags = world.query::<&Tag>().iter(world).count();
    Seen {
        remotes,
        counters,
        sparks,
        looks,
        tags,
    }
}

fn clock(app: &App) -> f64 {
    app.world().resource::<DemoPlayback>().clock
}

/// The wall time each update of the recording waits for the sockets.
const PAUSE: Duration = Duration::from_millis(2);

/// The port `server` listens on, once its listener is up.
fn bound_port(server: &mut App) -> u16 {
    for _ in 0..5000 {
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

/// The counter's value in `app`, if it has the counter yet.
fn counter(app: &mut App) -> Option<u32> {
    let world = app.world_mut();
    world.query::<&Counter>().iter(world).next().map(|c| c.0)
}

fn record(path: &Path) {
    let mut server = server_app();
    let port = bound_port(&mut server);
    let mut recorder = recorder_app(port, path.to_path_buf());
    // Until the recorder has the counter's last value.
    for _ in 0..5000 {
        server.update();
        recorder.update();
        if counter(&mut recorder) == Some(END) {
            return;
        }
        sleep(PAUSE);
    }
    panic!("recording stalled at {:?}", counter(&mut recorder));
}

/// A demo file in the temp directory, removed when it goes out of scope, the test failed or not.
struct TempDemo(PathBuf);

impl Drop for TempDemo {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[test]
fn seek_matches_straight_playback() {
    let demo =
        TempDemo(std::env::temp_dir().join(format!("netahoy-seek-{}.ahoydem", std::process::id())));
    let path = demo.0.clone();
    record(&path);

    // Straight playback: what the world looks like at every frame's clock.
    let mut straight = playback_app(path.clone());
    let mut timeline = Vec::new();
    for _ in 0..2000 {
        straight.update();
        // Connected, and the opening seek (to 0) applied.
        if straight
            .world()
            .get_resource::<DemoPlayback>()
            .is_some_and(|playback| playback.pending_seek().is_none())
        {
            let t = clock(&straight);
            timeline.push((t, seen(&mut straight)));
            if straight.world().resource::<DemoPlayback>().finished() {
                break;
            }
        }
    }
    let duration = straight.world().resource::<DemoPlayback>().duration();
    assert!(duration > 0.5, "demo too short: {duration}");
    assert!(
        timeline.iter().any(|(_, seen)| !seen.sparks.is_empty()),
        "the spark never showed up in straight playback"
    );
    let last = &timeline.last().unwrap().1;
    assert!(
        last.sparks.is_empty(),
        "the spark should be gone at the end"
    );
    assert_eq!(
        (last.looks.as_slice(), last.tags),
        (&[7][..], 1),
        "look and tag at the end"
    );

    // Seek around in one session: jump to the end, then back to times
    // straight playback saw, forward and backward in a scrambled order.
    let mut seeking = playback_app(path.clone());
    for _ in 0..5 {
        seeking.update();
    }
    seeking
        .world_mut()
        .resource_mut::<DemoPlayback>()
        .seek(duration);
    seeking.update();
    // Straight playback's clock runs on past the last record, where a seek
    // clamps to the end: only times inside the demo.
    let mut order: Vec<usize> = (0..timeline.len())
        .step_by(7)
        .filter(|&index| timeline[index].0 <= duration)
        .collect();
    // A fixed shuffle: deterministic, mixes long and short jumps both ways.
    let n = order.len();
    for i in 0..n {
        order.swap(i, (i * 37 + 11) % n);
    }
    let mut checked_spark = false;
    for index in order {
        let (t, expected) = &timeline[index];
        seeking.world_mut().resource_mut::<DemoPlayback>().seek(*t);
        seeking.update();
        assert_eq!(clock(&seeking), *t, "a seek lands on its time");
        let got = seen(&mut seeking);
        assert_eq!(
            &got, expected,
            "seek to {t:.3}s differs from straight playback"
        );
        checked_spark |= !expected.sparks.is_empty();
    }
    assert!(checked_spark, "no seek landed inside the spark's lifetime");

    let seeks = seeking.world().resource::<Seeks>();
    assert!(
        seeks.0.iter().any(|seek| seek.reset),
        "backward seeks reset"
    );
    assert!(
        seeks.0.iter().any(|seek| !seek.reset),
        "forward seeks don't"
    );

    // After the last seek playback carries on as normal.
    for _ in 0..10 {
        seeking.update();
    }
    assert!(!seeking.world().resource::<DemoPlayback>().bursting());
}
