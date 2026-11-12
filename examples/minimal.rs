//! The smallest game on bevy_netahoy: players run and jump among a few boxes and count their
//! jumps. One binary is both sides:
//!
//! ```text
//! cargo run --example minimal -- --server
//! cargo run --example minimal                # a client in a window; start as many as you like
//! cargo run --example minimal -- --headless  # a client with no window that runs and jumps itself
//! ```
//!
//! Each client predicts its own player and draws the others where the server's snapshots had
//! them. WASD runs, Space jumps, the mouse looks around while the left button is held.
//! `SERVER_ADDR=<ip>:<port>` moves both sides off `127.0.0.1:5000`.

use std::time::Duration;

use aeronet::io::SessionEndpoint;
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
    app::ScheduleRunnerPlugin, ecs::entity::MapEntities, input::mouse::AccumulatedMouseMotion,
    log::LogPlugin, prelude::*, state::app::StatesPlugin,
};
use bevy_ahoy::prelude::*;
use bevy_netahoy::*;
use bevy_replicon::prelude::*;
use serde::{Deserialize, Serialize};

/// The game half of a player: what its steps keep besides the movement.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize, MapEntities)]
struct Jumper {
    jumps: u32,
}

/// A player left the ground with a jump: the jump's id and the player's count with it.
#[derive(Message, Clone, Copy, Debug, Serialize, Deserialize)]
struct Jumped(EventId, u32);

impl StepEvent for Jumped {
    fn id(&self) -> EventId {
        self.0
    }
}

impl PlayerGame for Jumper {
    type Event = Jumped;
    type Context = ();

    fn step(player: &mut PlayerState<Self>, step: &mut Step<Self>) {
        step.pmove(&mut player.moves);
        step.separate(&mut player.moves);
        if player.moves.since_jump == 0 {
            player.game.jumps += 1;
            let (owner, sequence) = (step.owner(), step.command().sequence);
            step.emit(Jumped(EventId { owner, sequence }, player.game.jumps));
        }
    }
}

/// The level, the same on both sides: a floor and a few boxes, as (center, size).
const LEVEL: [(Vec3, Vec3); 5] = [
    (Vec3::new(0.0, -0.5, 0.0), Vec3::new(200.0, 1.0, 200.0)),
    (Vec3::new(4.0, 0.5, -6.0), Vec3::new(2.0, 1.0, 2.0)),
    (Vec3::new(-5.0, 1.0, -3.0), Vec3::new(3.0, 2.0, 3.0)),
    (Vec3::new(0.0, 0.25, -14.0), Vec3::new(6.0, 0.5, 4.0)),
    (Vec3::new(9.0, 1.5, 3.0), Vec3::new(1.0, 3.0, 8.0)),
];

fn main() -> AppExit {
    let server = std::env::args().any(|arg| arg == "--server");
    let headless = std::env::args().any(|arg| arg == "--headless")
        || std::env::var("HEADLESS").is_ok_and(|value| value == "1");

    let mut app = App::new();
    if server || headless {
        let loop_60_hz = ScheduleRunnerPlugin::run_loop(Duration::from_secs_f64(1.0 / 60.0));
        app.add_plugins((
            MinimalPlugins.set(loop_60_hz),
            StatesPlugin,
            TransformPlugin,
            LogPlugin::default(),
        ));
    } else {
        app.add_plugins(DefaultPlugins);
    }
    app.add_plugins((
        PhysicsPlugins::default(),
        // netahoy steps the controllers itself, on this schedule.
        AhoyPlugins::new(KccSchedule),
        RepliconPlugins,
        NetAhoyProtocolPlugin::<Jumper>::default(),
    ))
    .add_systems(Startup, spawn_level)
    .add_systems(Update, log_jumps);
    if server {
        add_server(&mut app);
    } else {
        add_client(&mut app, headless);
    }
    app.run()
}

fn server_addr() -> String {
    std::env::var("SERVER_ADDR").unwrap_or_else(|_| "127.0.0.1:5000".to_owned())
}

/// What steps a player, the same on both sides. Players move through each other's colliders:
/// [`Step::separate`] pushes them apart where each saw the other.
fn player_body(shape: &PlayerShape) -> impl Bundle {
    let filter = SpatialQueryFilter::from_mask(WORLD_COLLISION_LAYER);
    (
        CharacterController {
            filter,
            ..default()
        },
        shape.collider(),
        CollisionLayers::new(PLAYER_COLLISION_LAYER, LayerMask::ALL),
    )
}

/// The level's colliders.
fn spawn_level(mut commands: Commands) {
    for (center, size) in LEVEL {
        commands.spawn((
            Transform::from_translation(center),
            RigidBody::Static,
            Collider::cuboid(size.x, size.y, size.z),
            CollisionLayers::new(WORLD_COLLISION_LAYER, LayerMask::ALL),
        ));
    }
}

/// Each jump once, however often its command steps: on the server every player's, on a client
/// its own player's as predicted.
fn log_jumps(mut jumps: MessageReader<Jumped>) {
    for Jumped(id, count) in jumps.read() {
        info!("stepped: player {} jump {count}", id.owner.0);
    }
}

fn add_server(app: &mut App) {
    app.add_plugins((
        WebSocketServerPlugin,
        AeronetRepliconServerPlugin,
        NetAhoyServerPlugin::<Jumper>::default(),
    ))
    .add_systems(Startup, listen)
    .add_observer(join)
    // One fixed tick per frame, so every tick's snapshot reaches the clients.
    .set_runner(one_tick_per_frame);
}

fn listen(mut commands: Commands) {
    let addr = server_addr().parse().expect("SERVER_ADDR is <ip>:<port>");
    let config = ServerConfig::builder()
        .with_bind_address(addr)
        .with_no_encryption();
    commands
        .spawn(AeronetRepliconServer)
        .queue(WebSocketServer::open(config));
    info!("listening on ws://{addr}");
}

/// A player for each client that joins; netahoy tells the client which player is its own. Its
/// owner brings what replicates it and marks it a player on both sides.
///
/// Only an authorized client gets one: replicon refuses a client whose protocol differs (another
/// build, another tick rate), and checks it before the join arrives.
fn join(
    join: On<FromClient<JoinRequest>>,
    mut commands: Commands,
    shape: Res<PlayerShape>,
    players: Query<&PlayerOwner>,
    authorized: Query<(), With<AuthorizedClient>>,
    mut last_id: Local<u64>,
) {
    let Some(client) = join.client_id.entity() else {
        return;
    };
    if !authorized.contains(client) {
        return;
    }
    // A repeated join request must not spawn a second player for the same client.
    if players.iter().any(|owner| owner.0 == client) {
        return;
    }
    *last_id += 1;
    let id = *last_id;
    commands.spawn((
        PlayerId(id),
        PlayerOwner(client),
        PlayerState::<Jumper>::at(Vec3::new(id as f32 * 1.5, 1.0, 5.0)),
        player_body(&shape),
    ));
}

fn add_client(app: &mut App, headless: bool) {
    app.add_plugins((
        WebSocketClientPlugin,
        AeronetRepliconClientPlugin,
        NetAhoyClientPlugin::<Jumper>::default(),
    ))
    .add_observer(
        |session: On<Add, SessionEndpoint>, mut commands: Commands| {
            commands
                .entity(session.event_target())
                .insert(AeronetRepliconClient);
        },
    )
    // Every jump the server's step made. This client's own confirm what its prediction logged
    // (`log_jumps`) a round trip earlier, or are the only copy of a jump it didn't predict.
    .add_observer(|jump: On<RemoteEvent<Jumped>>| {
        let RemoteEvent(Jumped(id, count)) = *jump;
        info!("from the server: player {} jump {count}", id.owner.0);
    })
    // The local player's prediction: the game gives it a body, netahoy its id and state.
    .add_observer(
        |ours: On<Add, OwnPlayer>, mut commands: Commands, shape: Res<PlayerShape>| {
            let server_entity = ours.event_target();
            commands.spawn((PredictedPlayer { server_entity }, player_body(&shape)));
        },
    )
    .add_systems(Startup, connect);
    // From here on netahoy runs `connect` again whenever the link drops or goes silent.
    let connect = app.register_system(connect);
    app.insert_resource(Reconnect::new(connect));

    let gather_input = if headless {
        autopilot.into_configs()
    } else {
        app.add_systems(Startup, spawn_view)
            .add_systems(Update, (draw_players, follow_camera));
        keyboard_and_mouse.into_configs()
    };
    app.add_systems(
        RunFixedMainLoop,
        gather_input.in_set(NetAhoyClientSystems::GatherInput),
    );
}

fn connect(mut commands: Commands) {
    let url = format!("ws://{}", server_addr());
    let config = ClientConfig::builder().with_no_encryption().disable_nagle();
    commands
        .spawn_empty()
        .queue(WebSocketClient::connect(config, url.clone()));
    info!("connecting to {url}");
}

/// A windowed client's input.
fn keyboard_and_mouse(
    keys: Res<ButtonInput<KeyCode>>,
    mouse: Res<ButtonInput<MouseButton>>,
    motion: Res<AccumulatedMouseMotion>,
    mut input: ResMut<ClientInput>,
) {
    if mouse.pressed(MouseButton::Left) {
        input.look -= motion.delta * 0.003;
        input.look.y = input.look.y.clamp(-1.5, 1.5);
    }
    let key = |code| f32::from(u8::from(keys.pressed(code)));
    let movement = Vec2::new(
        key(KeyCode::KeyD) - key(KeyCode::KeyA),
        key(KeyCode::KeyW) - key(KeyCode::KeyS),
    );
    input.movement = movement.normalize_or_zero();
    let jump = keys.pressed(KeyCode::Space);
    input.buttons.set(Buttons::JUMP, jump);
    // A tap between two ticks still reaches the next command.
    if keys.just_pressed(KeyCode::Space) {
        input.pressed |= Buttons::JUMP;
    }
}

/// The headless client's input: run in a circle, jump every 1.5 s.
fn autopilot(time: Res<Time>, mut input: ResMut<ClientInput>, mut next_jump: Local<f32>) {
    let now = time.elapsed_secs();
    input.movement = Vec2::Y;
    input.look.x = now * 0.5;
    if now >= *next_jump {
        *next_jump = now + 1.5;
        input.pressed |= Buttons::JUMP;
    }
}

/// A windowed client's camera, light and level meshes.
fn spawn_view(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    commands.spawn(Camera3d::default());
    let sun = Transform::from_xyz(4.0, 10.0, 6.0).looking_at(Vec3::ZERO, Vec3::Y);
    commands.spawn((DirectionalLight::default(), sun));
    let material = materials.add(Color::srgb(0.35, 0.4, 0.45));
    for (center, size) in LEVEL {
        let mesh = meshes.add(Cuboid::from_size(size));
        let at = Transform::from_translation(center);
        commands.spawn((Mesh3d(mesh), MeshMaterial3d(material.clone()), at));
    }
}

/// A mesh on the local prediction and a visual for each remote player; netahoy moves both.
fn draw_players(
    mut commands: Commands,
    shape: Res<PlayerShape>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    ours: Query<Entity, Added<PredictedPlayer>>,
    remotes: Query<(Entity, &PlayerId, &History<PlayerSnapshot>), Added<RemotePlayer>>,
) {
    let mut drawn = |color| {
        let mesh = meshes.add(Cylinder::new(shape.radius, shape.half_height * 2.0));
        (Mesh3d(mesh), MeshMaterial3d(materials.add(color)))
    };
    for prediction in &ours {
        commands
            .entity(prediction)
            .insert(drawn(Color::srgb(1.0, 0.8, 0.2)));
    }
    for (server_entity, &player_id, history) in &remotes {
        let at = history
            .newest()
            .map_or(Vec3::ZERO, |(_, pose)| pose.position);
        commands.spawn((
            RemoteVisual {
                server_entity,
                player_id,
            },
            Transform::from_translation(at),
            drawn(Color::srgb(0.3, 0.6, 1.0)),
        ));
    }
}

/// A camera behind the predicted player, turned by the look the input holds.
fn follow_camera(
    input: Res<ClientInput>,
    player: Single<&Transform, (With<PredictedPlayer>, Without<Camera3d>)>,
    mut camera: Single<&mut Transform, With<Camera3d>>,
) {
    camera.rotation = Quat::from_euler(EulerRot::YXZ, input.look.x, input.look.y, 0.0);
    camera.translation = player.translation + camera.rotation * Vec3::new(0.0, 1.5, 6.0);
}
