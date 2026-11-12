//! The playground's server: it listens over WebSocket and WebTransport (the library's transport
//! server), gives every client that joins a player, steps every player's commands (its two bots'
//! too, `playground::bots`), moves the platform, cycles the shield gate, and answers each hit with
//! its push.

use aeronet::io::connection::{DisconnectReason, Disconnected};
use avian3d::prelude::*;
use bevy::{
    prelude::*,
    window::{ExitCondition, WindowPlugin},
};
use bevy_ahoy::prelude::*;
use bevy_enhanced_input::prelude::EnhancedInputPlugin;
use bevy_netahoy::{
    transport::{NetAhoyTransportServerPlugin, ServerTransportConfig, Transport},
    *,
};
use bevy_replicon::prelude::*;

mod link_report;

use playground::{
    Arsenal, Platform, ShieldGate, blast,
    bots::BotsPlugin,
    input_rollback_setting,
    level::{self, platform_collider, platform_position},
    shared::*,
    time_scale_from_args,
};

fn main() -> AppExit {
    let time_scale = time_scale_from_args();
    let mut app = App::new();

    // `--net=<preset|spec>` conditions every player's link from here; a
    // client can condition its own instead (and tune it live). Not both.
    app.add_plugins(bevy_link_conditioner::LinkConditionerPlugin::from_args())
        .add_systems(Update, link_report::report_link_conditions);

    let loop_hz = server_loop_hz();
    // No winit: the runner below drives the loop.
    let default_plugins = DefaultPlugins
        .set(WindowPlugin {
            primary_window: None,
            exit_condition: ExitCondition::DontExit,
            ..default()
        })
        .disable::<bevy::winit::WinitPlugin>();

    app.insert_resource(time_scale).add_plugins((
        default_plugins,
        PhysicsPlugins::default(),
        EnhancedInputPlugin,
        AhoyPlugins::new(KccSchedule),
        ExampleSharedPlugin,
        NetAhoyServerPlugin::<Arsenal>::default().with_input_rollback(input_rollback_setting(
            std::env::var("NETAHOY_ROLLBACK").ok().as_deref(),
        )),
        NetAhoyTransportServerPlugin::new(ServerTransportConfig::from_env()),
        ServerPlugin,
        BotsPlugin,
    ));
    add_server_loop(&mut app, loop_hz);
    app.run()
}

/// `SERVER_LOOP_HZ=<n>`: the loop runs at n Hz instead of one frame per fixed tick
/// ([`one_tick_per_frame`]). A pinned loop can run two ticks in one frame after a hitch, and
/// replication merges them (the server warns). The tick rate stays.
fn server_loop_hz() -> Option<f64> {
    let raw = std::env::var("SERVER_LOOP_HZ").ok()?;
    match raw.trim().parse::<f64>() {
        Ok(hz) if hz.is_finite() && hz > 0.0 => Some(hz),
        _ => panic!("SERVER_LOOP_HZ={raw:?}: expected a positive number of Hz"),
    }
}

/// Seconds per `server loop:` rate line (Updates and FixedUpdates per second).
const LOOP_RATE_WINDOW_S: f64 = 10.0;

#[derive(Resource, Default)]
struct ServerLoopRate {
    hz: Option<f64>,
    since: Option<std::time::Instant>,
    updates: u64,
    fixed: u64,
}

fn add_server_loop(app: &mut App, loop_hz: Option<f64>) {
    match loop_hz {
        // Added after DefaultPlugins, so this is the runner the app keeps.
        Some(hz) => {
            app.add_plugins(bevy::app::ScheduleRunnerPlugin::run_loop(
                std::time::Duration::from_secs_f64(1.0 / hz),
            ));
        }
        None => {
            app.set_runner(one_tick_per_frame);
        }
    }
    app.insert_resource(ServerLoopRate {
        hz: loop_hz,
        ..default()
    })
    .add_systems(Startup, log_server_loop_mode)
    .add_systems(FixedUpdate, |mut rate: ResMut<ServerLoopRate>| {
        rate.fixed += 1
    })
    .add_systems(Update, log_server_loop_rate);
}

fn log_server_loop_mode(rate: Res<ServerLoopRate>) {
    match rate.hz {
        Some(hz) => info!(
            "server loop: SERVER_LOOP_HZ={hz}: ScheduleRunner, {:.3} ms per Update, no winit",
            1000.0 / hz
        ),
        None => info!("server loop: one fixed tick per frame (SERVER_LOOP_HZ unset)"),
    }
}

fn log_server_loop_rate(mut rate: ResMut<ServerLoopRate>) {
    let now = std::time::Instant::now();
    let since = *rate.since.get_or_insert(now);
    rate.updates += 1;
    let secs = now.duration_since(since).as_secs_f64();
    if secs >= LOOP_RATE_WINDOW_S {
        info!(
            "server loop: {:.1} Updates/s, {:.2} FixedUpdates/s over {secs:.1} s",
            rate.updates as f64 / secs,
            rate.fixed as f64 / secs
        );
        rate.since = Some(now);
        rate.updates = 0;
        rate.fixed = 0;
    }
}

struct ServerPlugin;

impl Plugin for ServerPlugin {
    fn build(&self, app: &mut App) {
        playground::totals::add_shot_totals(app);

        app.add_systems(FixedPreUpdate, blast.in_set(NetAhoyServerSystems::Events))
            .add_observer(join_player)
            .add_observer(log_disconnect)
            .add_systems(Startup, setup_server)
            .add_systems(FixedUpdate, cycle_gate)
            // Before the events: the blasts push players where this tick left them, and the
            // players' components are written after it.
            .add_systems(
                FixedPreUpdate,
                (move_platform, reset_fallen_players)
                    .chain()
                    .after(NetAhoyServerSystems::ApplyCommands)
                    .before(NetAhoyServerSystems::Events),
            );
    }
}

fn setup_server(mut commands: Commands) {
    level::spawn_colliders(&mut commands);
    spawn_platform(&mut commands);
    // The library records its history every tick and replicates it.
    commands.spawn((Name::new("shield gate"), ShieldGate::default(), Replicated));
}

/// The gate's timer, in the fixed schedule, so its history's entry at a tick is that tick's.
fn cycle_gate(tick: Res<ServerTick>, rate: Res<TickRate>, mut gates: Query<&mut ShieldGate>) {
    for mut gate in &mut gates {
        gate.set_if_neq(ShieldGate::at_tick(tick.0, *rate));
    }
}

/// A player for each client that joins. Only for a client replicon has authorized: one whose
/// protocol (this game's types, the library's wire and the tick rate) differs is sent away, and
/// must not leave a player behind. Replicon checks the protocol on the client's first event, so a
/// matching client is authorized by the time its join arrives.
fn join_player(
    join: On<FromClient<JoinRequest>>,
    mut commands: Commands,
    shape: Res<PlayerShape>,
    players: Query<(&PlayerOwner, &PlayerId)>,
    authorized: Query<(), With<AuthorizedClient>>,
    transports: Query<&Transport>,
) {
    let Some(client) = join.client_id.entity() else {
        return;
    };
    if !authorized.contains(client) {
        info!("client {client} asked to join with another protocol: refused");
        return;
    }

    let mut assigned_ids = Vec::new();
    for (owner, player_id) in &players {
        if owner.0 == client {
            return;
        }
        assigned_ids.push(player_id.0);
    }

    let player_id = (1u64..)
        .find(|id| !assigned_ids.contains(id))
        .expect("all player IDs exhausted");
    let transport = transports
        .get(client)
        .map_or("?", |transport| transport.name());
    info!("client {client} joined as player {player_id} over {transport}");

    spawn_player(
        &mut commands,
        &shape,
        client,
        player_id,
        player_spawn_point(player_id),
        Vec3::ZERO,
    );
}

fn spawn_platform(commands: &mut Commands) {
    let position = platform_position(0, TickRate::default());
    commands.spawn((
        Name::new("moving platform"),
        Platform,
        Replicated,
        // All that crosses the wire: the library publishes it from Position
        // every tick, into the history every step meets the platform in.
        BodySnapshot::default(),
        RigidBody::Kinematic,
        platform_collider(),
        Position::new(position),
        Transform::from_translation(position),
    ));
}

fn move_platform(
    tick: Res<ServerTick>,
    tick_rate: Res<TickRate>,
    mut platforms: Query<&mut Position, With<Platform>>,
) {
    for mut position in &mut platforms {
        position.0 = platform_position(tick.0, *tick_rate);
    }
}

/// Why a client left, for the log.
fn log_disconnect(disconnected: On<Disconnected>) {
    let client = disconnected.event_target();
    match &disconnected.reason {
        DisconnectReason::ByUser(reason) => info!("{client} disconnected: {reason}"),
        DisconnectReason::ByPeer(reason) => info!("{client} disconnected by peer: {reason}"),
        DisconnectReason::ByError(err) => warn!("{client} disconnected: {err:#}"),
    }
}

fn reset_fallen_players(mut players: Query<(&PlayerId, &mut PlayerState<Arsenal>)>) {
    for (player_id, mut state) in &mut players {
        if state.moves.position.y < -12.0 {
            // The game half (the arsenal) survives the fall; the movement starts over.
            state.moves = PlayerState::<Arsenal>::at(player_spawn_point(player_id.0)).moves;
        }
    }
}
