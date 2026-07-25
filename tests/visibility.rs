//! Proves the AhoySnapshot visibility split over a real replicon exchange:
//! every client gets every player's PlayerSnapshot, but the reconcile-only
//! AhoySnapshot reaches exactly the owning client — and a bot (no PlayerOwner)
//! sends it to nobody.

use bevy::{prelude::*, state::app::StatesPlugin};
use bevy_netahoy::{AhoySnapshot, NetAhoyProtocolPlugin, NetworkedPlayer, PlayerId, PlayerOwner, PlayerSnapshot};
use bevy_replicon::{prelude::*, test_app::{ServerTestAppExt, TestClientEntity}};

fn make_app() -> App {
    let mut app = App::new();
    app.add_plugins((
        MinimalPlugins,
        StatesPlugin,
        RepliconPlugins.set(ServerPlugin::new(PostUpdate)),
        NetAhoyProtocolPlugin,
    ));
    app
}

fn snapshot_components(app: &mut App, player: u64) -> (bool, bool) {
    let mut query = app
        .world_mut()
        .query::<(&PlayerId, Option<&PlayerSnapshot>, Option<&AhoySnapshot>)>();
    let (_, player_snapshot, ahoy_snapshot) = query
        .iter(app.world())
        .find(|(id, ..)| id.0 == player)
        .unwrap_or_else(|| panic!("player {player} should have replicated"));
    (player_snapshot.is_some(), ahoy_snapshot.is_some())
}

#[test]
fn ahoy_snapshot_is_owner_only() {
    let mut server = make_app();
    // The same registration ServerNetAhoyPlugin performs (the full plugin
    // drags in physics; the filter is what's under test here).
    server.add_visibility_filter::<PlayerOwner>();
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
        NetworkedPlayer,
        PlayerId(1),
        PlayerOwner(owner),
        PlayerSnapshot::default(),
        AhoySnapshot::default(),
    ));
    // A bot: owned by nobody, so its AhoySnapshot should reach nobody.
    // (No PlayerOwner at all would mean UNfiltered — visible to everyone.)
    server.world_mut().spawn((
        Replicated,
        NetworkedPlayer,
        PlayerId(9001),
        PlayerOwner(Entity::PLACEHOLDER),
        PlayerSnapshot::default(),
        AhoySnapshot::default(),
    ));

    server.update();
    server.exchange_with_client(&mut client_a);
    server.exchange_with_client(&mut client_b);
    client_a.update();
    client_b.update();

    assert_eq!(snapshot_components(&mut client_a, 1), (true, true), "owner gets both");
    assert_eq!(snapshot_components(&mut client_b, 1), (true, false), "remote gets only PlayerSnapshot");
    assert_eq!(snapshot_components(&mut client_a, 9001), (true, false), "bot reconcile data goes to nobody");
    assert_eq!(snapshot_components(&mut client_b, 9001), (true, false), "bot reconcile data goes to nobody");
}
