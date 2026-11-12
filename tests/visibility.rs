//! The OwnerSnapshot visibility split over a real replicon exchange: every client gets every
//! player, but the reconcile-only OwnerSnapshot reaches exactly the owning client, and a bot's
//! (owned by a placeholder entity) reaches nobody. And the pose and body histories keep every
//! tick of a burst that lands in one client frame, where a plain replicated component keeps
//! only the newest.

use bevy::{prelude::*, state::app::StatesPlugin};
use bevy_netahoy::{
    BodySnapshot, History, NetAhoyProtocolPlugin, OwnerSnapshot, PlayerId, PlayerOwner,
    PlayerSnapshot, ServerPlayer, ServerTick,
    testing::{self, OwnerSnapshotPlugin, stamp_replication_tick},
};
use bevy_replicon::{
    prelude::*,
    test_app::{ServerTestAppExt, TestClientEntity},
};
use playground::Arsenal;
use serde::{Deserialize, Serialize};

/// A server tick as a plain replicated component, no history marker: what
/// replicon does to a burst without one.
#[derive(Component, Serialize, Deserialize, Clone, Copy, Default)]
struct ReplicatedTick(u64);

fn make_app() -> App {
    let mut app = App::new();
    app.add_plugins((
        MinimalPlugins,
        StatesPlugin,
        RepliconPlugins.set(ServerPlugin::new(PostUpdate)),
        NetAhoyProtocolPlugin::<Arsenal>::default(),
    ))
    .replicate::<ReplicatedTick>();
    app
}

/// Whether `player` reached this client with its reconcile snapshot.
fn has_ahoy_snapshot(app: &mut App, player: u64) -> bool {
    let mut query = app
        .world_mut()
        .query::<(&PlayerId, Option<&OwnerSnapshot<Arsenal>>)>();
    let (_, ahoy_snapshot) = query
        .iter(app.world())
        .find(|(id, _)| id.0 == player)
        .unwrap_or_else(|| panic!("player {player} should have replicated"));
    ahoy_snapshot.is_some()
}

#[test]
fn ahoy_snapshot_is_owner_only() {
    let mut server = make_app();
    // The plugin NetAhoyServerPlugin adds (the full plugin drags in physics; the filter is
    // what's under test here).
    server.add_plugins(OwnerSnapshotPlugin::<Arsenal>::default());
    let mut client_a = make_app();
    let mut client_b = make_app();
    server.finish();
    client_a.finish();
    client_b.finish();

    server.connect_client(&mut client_a);
    server.connect_client(&mut client_b);
    let owner = client_a.world().resource::<TestClientEntity>().entity();

    server.world_mut().spawn((
        Replicated,
        ServerPlayer,
        PlayerId(1),
        PlayerOwner(owner),
        OwnerSnapshot::<Arsenal>::default(),
    ));
    // A bot, owned by no client: its OwnerSnapshot reaches nobody. Without a PlayerOwner at
    // all it would reach everyone.
    server.world_mut().spawn((
        Replicated,
        ServerPlayer,
        PlayerId(9001),
        PlayerOwner(Entity::PLACEHOLDER),
        OwnerSnapshot::<Arsenal>::default(),
    ));

    server.update();
    server.exchange_with_client(&mut client_a);
    server.exchange_with_client(&mut client_b);
    client_a.update();
    client_b.update();

    assert!(
        has_ahoy_snapshot(&mut client_a, 1),
        "the owner gets its reconcile snapshot"
    );
    assert!(!has_ahoy_snapshot(&mut client_b, 1), "nobody else does");
    assert!(
        !has_ahoy_snapshot(&mut client_a, 9001),
        "bot reconcile data goes to nobody"
    );
    assert!(
        !has_ahoy_snapshot(&mut client_b, 9001),
        "bot reconcile data goes to nobody"
    );
}

/// Removing a player's owner unfilters its reconcile snapshot: every client gets it, as for a
/// player spawned without one.
#[test]
fn a_player_that_loses_its_owner_is_unfiltered() {
    let mut server = make_app();
    server.add_plugins(OwnerSnapshotPlugin::<Arsenal>::default());
    let mut client_a = make_app();
    let mut client_b = make_app();
    server.finish();
    client_a.finish();
    client_b.finish();

    server.connect_client(&mut client_a);
    server.connect_client(&mut client_b);
    let owner = client_a.world().resource::<TestClientEntity>().entity();
    let player = server
        .world_mut()
        .spawn((
            Replicated,
            ServerPlayer,
            PlayerId(1),
            PlayerOwner(owner),
            OwnerSnapshot::<Arsenal>::default(),
        ))
        .id();
    let mut exchange = |server: &mut App| {
        server.update();
        for client in [&mut client_a, &mut client_b] {
            server.exchange_with_client(client);
            client.update();
        }
        [
            has_ahoy_snapshot(&mut client_a, 1),
            has_ahoy_snapshot(&mut client_b, 1),
        ]
    };
    assert_eq!(
        exchange(&mut server),
        [true, false],
        "owned: the owner only"
    );

    server
        .world_mut()
        .entity_mut(player)
        .remove::<PlayerOwner>();
    assert_eq!(exchange(&mut server), [true, true], "no owner: everyone");

    server
        .world_mut()
        .entity_mut(player)
        .insert(PlayerOwner(owner));
    assert_eq!(
        exchange(&mut server),
        [true, false],
        "owned again: the owner only"
    );

    // A despawn removes the owner too: nothing is left to unfilter, and nothing panics.
    server.world_mut().despawn(player);
    server.update();
}

fn pose(x: f32) -> PlayerSnapshot {
    PlayerSnapshot {
        position: Vec3::X * x,
        ..default()
    }
}

fn body(tick: u64) -> BodySnapshot {
    BodySnapshot {
        tick,
        position: Vec3::Y * tick as f32,
        rotation: Quat::IDENTITY,
        ..default()
    }
}

/// Four server ticks replicate before the client runs a frame (a TCP stall,
/// or a client below the tick rate). The plain component ends at the newest
/// tick, but the histories hold all four, keyed by netahoy's tick (replicon
/// userdata, not replicon's own tick, which starts elsewhere), exactly the
/// log the server recorded.
#[test]
fn histories_keep_every_tick_of_a_burst() {
    let mut server = make_app();
    // Netahoy's tick runs ahead of replicon's: the server ran before it listened.
    server.insert_resource(ServerTick(1_000)).add_systems(
        PostUpdate,
        stamp_replication_tick.before(ServerSystems::Send),
    );
    let mut client = make_app();
    server.finish();
    client.finish();
    server.connect_client(&mut client);

    let (player, prop) = {
        let world = server.world_mut();
        let player = world
            .spawn((
                Replicated,
                ServerPlayer,
                PlayerId(1),
                pose(0.0),
                ReplicatedTick(1_000),
            ))
            .id();
        let prop = world.spawn((Replicated, body(1_000))).id();
        (player, prop)
    };
    let record = |server: &mut App, tick: u64| {
        let world = server.world_mut();
        world.resource_mut::<ServerTick>().0 = tick;
        let mut entity = world.entity_mut(player);
        entity.insert((pose(tick as f32), ReplicatedTick(tick)));
        let mut history = entity.get_mut::<History<PlayerSnapshot>>().unwrap();
        testing::record(&mut history, tick, pose(tick as f32));
        let mut entity = world.entity_mut(prop);
        entity.insert(body(tick));
        let mut history = entity.get_mut::<History<BodySnapshot>>().unwrap();
        testing::record(&mut history, tick, body(tick));
    };
    record(&mut server, 1_000);
    server.update();
    server.exchange_with_client(&mut client);
    client.update();

    for tick in 1_001..=1_004 {
        record(&mut server, tick);
        server.update();
    }
    server.exchange_with_client(&mut client);
    client.update();

    let world = client.world_mut();
    let plain = world.query::<&ReplicatedTick>().single(world).unwrap().0;
    assert_eq!(plain, 1_004, "a plain component keeps only the newest");

    let (snapshot, history) = world
        .query::<(&PlayerSnapshot, &History<PlayerSnapshot>)>()
        .single(world)
        .unwrap();
    let ticks: Vec<_> = history
        .iter()
        .map(|(tick, pose)| (tick, pose.position.x))
        .collect();
    assert_eq!(
        ticks,
        [
            (1_000, 1_000.0),
            (1_001, 1_001.0),
            (1_002, 1_002.0),
            (1_003, 1_003.0),
            (1_004, 1_004.0)
        ]
    );
    assert_eq!(snapshot.server_tick, 1_004, "the component is the newest");
    let server_history = server
        .world()
        .get::<History<PlayerSnapshot>>(player)
        .unwrap();
    assert!(
        server_history.iter().eq(history.iter()),
        "both peers hold the same log"
    );

    let world = client.world_mut();
    let bodies = world
        .query::<&History<BodySnapshot>>()
        .single(world)
        .unwrap();
    let ticks: Vec<_> = bodies.iter().map(|(tick, _)| tick).collect();
    assert_eq!(ticks, [1_000, 1_001, 1_002, 1_003, 1_004]);
}

/// A body that comes to rest goes quiet on the wire, while the server's
/// history keeps an entry every tick. The client's fills the quiet ticks in
/// once it knows they happened (the owner snapshot's heartbeat, here called
/// by hand): both peers end with the same entry at every tick.
#[test]
fn a_resting_body_holds_the_same_history_on_both_peers() {
    let mut server = make_app();
    server.insert_resource(ServerTick(500)).add_systems(
        PostUpdate,
        stamp_replication_tick.before(ServerSystems::Send),
    );
    let mut client = make_app();
    server.finish();
    client.finish();
    server.connect_client(&mut client);

    let prop = server.world_mut().spawn((Replicated, body(500))).id();
    let rest = 503;
    for tick in 500..=510 {
        let world = server.world_mut();
        world.resource_mut::<ServerTick>().0 = tick;
        let moved = body(tick.min(rest));
        if tick <= rest {
            world.entity_mut(prop).insert(moved);
        }
        let mut entity = world.entity_mut(prop);
        let mut history = entity.get_mut::<History<BodySnapshot>>().unwrap();
        testing::record(&mut history, tick, moved);
        server.update();
        server.exchange_with_client(&mut client);
        client.update();
    }

    let world = client.world_mut();
    let mut history = world
        .query::<&mut History<BodySnapshot>>()
        .single_mut(world)
        .unwrap();
    testing::hold_until(&mut history, 510);
    let server_history = server.world().get::<History<BodySnapshot>>(prop).unwrap();
    let log = |history: &History<BodySnapshot>| {
        history
            .iter()
            .map(|(tick, body)| (tick, body.tick, body.position))
            .collect::<Vec<_>>()
    };
    assert_eq!(log(&history), log(server_history));
    assert_eq!(history.len(), 11);
    assert_eq!(history.newest().unwrap().1.position, Vec3::Y * rest as f32);
}
