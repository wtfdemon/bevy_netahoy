use aeronet::io::connection::{DisconnectReason, Disconnected};
use aeronet_replicon::server::{AeronetRepliconServer, AeronetRepliconServerPlugin};
use aeronet_websocket::server::{ServerConfig, WebSocketServer, WebSocketServerPlugin};
use avian3d::prelude::*;
use bevy::{
    prelude::*,
    window::{ExitCondition, WindowPlugin},
};
use bevy_ahoy::{prelude::*, CharacterLook};
use bevy_enhanced_input::prelude::EnhancedInputPlugin;
use bevy_netahoy::*;
use bevy_replicon::prelude::*;

mod hitscan;
mod pickup;
mod rockets;
mod shared;
mod vehicle;
use shared::*;

fn main() -> AppExit {
    let time_scale = debug_time_scale_from_args();
    let mut app = App::new();

    if poor_network_from_args() {
        app.add_plugins(AeronetNetworkConditionerPlugin::poor_condition());
    }

    app.insert_resource(time_scale)
        .add_plugins((
            DefaultPlugins.set(WindowPlugin {
                primary_window: None,
                exit_condition: ExitCondition::DontExit,
                ..default()
            }),
            PhysicsPlugins::default(),
            EnhancedInputPlugin,
            AhoyPlugins::new(NetAhoyKccSchedule),
            ExampleSharedPlugin,
            ServerNetAhoyPlugin,
            ServerPlugin,
        ))
        .run()
}

struct ServerPlugin;

impl Plugin for ServerPlugin {
    fn build(&self, app: &mut App) {
        hitscan::add_server_hitscan(app);
        vehicle::add_server_vehicles(app);
        pickup::add_server_pickup(app);

        app.add_plugins((WebSocketServerPlugin, AeronetRepliconServerPlugin))
            .add_observer(join_player)
            .add_observer(clean_up_disconnected_player)
            .add_systems(Startup, setup_server)
            .add_systems(
                FixedPreUpdate,
                (update_scripted_targets, reset_fallen_players)
                    .chain()
                    .after(ServerNetAhoySystems::ApplyCommands),
            );
    }
}

fn setup_server(mut commands: Commands) {
    commands
        .spawn((Name::new("Server"), AeronetRepliconServer))
        .queue(WebSocketServer::open(
            ServerConfig::builder()
                .with_bind_address(DEFAULT_SERVER_ADDR)
                .with_no_encryption(),
        ));
    spawn_world_colliders(&mut commands);
    spawn_flying_target(&mut commands);
    spawn_walking_target(&mut commands);
    vehicle::spawn_buggy(&mut commands, Vec3::new(-8.0, 1.5, 10.0));
    pickup::spawn_prop(&mut commands, Vec3::new(1.5, 0.8, 10.0));
    pickup::spawn_prop(&mut commands, Vec3::new(2.5, 0.8, 10.5));
    pickup::spawn_prop(&mut commands, Vec3::new(2.0, 1.6, 10.2));

    info!("websocket server listening on {DEFAULT_SERVER_URL}");
}

fn join_player(
    join: On<FromClient<JoinRequest>>,
    mut commands: Commands,
    players: Query<(&PlayerOwner, &PlayerId)>,
) {
    let Some(client) = join.client_id.entity() else {
        return;
    };

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
    commands.server_trigger(ToClients {
        targets: SendTargets::Single(ClientId::Client(client)),
        message: JoinAccepted { player_id },
    });

    info!("client {client} joined as player {player_id}");

    spawn_player(
        &mut commands,
        client,
        player_id,
        player_spawn_point(player_id),
        Vec3::ZERO,
    );
}

fn spawn_flying_target(commands: &mut Commands) {
    let position = flying_target_position(0);
    commands.spawn((
        Name::new("flying target player"),
        Replicated,
        NetworkedPlayer,
        PlayerId(FLYING_TARGET_PLAYER_ID),
        AhoySnapshot::default(),
        PlayerSnapshot::default(),
        // Owned by nobody: keeps the bot's AhoySnapshot off every client's wire.
        PlayerOwner(Entity::PLACEHOLDER),
        ServerCommandBuffer::default(),
        NetAhoyPlayerState::default(),
        CharacterLook::default(),
        CharacterControllerState::default(),
        Position::new(position),
        Rotation::IDENTITY,
        LinearVelocity::ZERO,
        Transform::from_translation(position),
    ));
}

/// Ground-level bot that paces back and forth — a rocket target you can walk
/// into (the shared step pushes you apart) and blast point-blank.
fn spawn_walking_target(commands: &mut Commands) {
    let position = walking_target_position(0);
    commands.spawn((
        Name::new("walking target player"),
        Replicated,
        NetworkedPlayer,
        PlayerId(WALKING_TARGET_PLAYER_ID),
        AhoySnapshot::default(),
        PlayerSnapshot::default(),
        // Owned by nobody: keeps the bot's AhoySnapshot off every client's wire.
        PlayerOwner(Entity::PLACEHOLDER),
        ServerCommandBuffer::default(),
        NetAhoyPlayerState::default(),
        CharacterLook::default(),
        CharacterControllerState::default(),
        (
            RigidBody::Static,
            Collider::cylinder(0.45, 1.5),
            player_collision_layers(),
            Position::new(position),
            Rotation::IDENTITY,
            LinearVelocity::ZERO,
            Transform::from_translation(position),
        ),
    ));
}

fn clean_up_disconnected_player(
    disconnected: On<Disconnected>,
    mut commands: Commands,
    players: Query<(Entity, &PlayerOwner)>,
) {
    let client = disconnected.event_target();
    let Some(player) = players
        .iter()
        .find_map(|(player, owner)| (owner.0 == client).then_some(player))
    else {
        return;
    };

    match &disconnected.reason {
        DisconnectReason::ByUser(reason) => info!("{client} disconnected: {reason}"),
        DisconnectReason::ByPeer(reason) => info!("{client} disconnected by peer: {reason}"),
        DisconnectReason::ByError(err) => warn!("{client} disconnected: {err:#}"),
    }

    commands.entity(player).despawn();
}

fn update_scripted_targets(
    tick: Res<ServerTick>,
    mut targets: Query<(
        &PlayerId,
        &mut Position,
        &mut Transform,
        &mut LinearVelocity,
        &mut CharacterLook,
    )>,
) {
    for (player_id, mut physics_position, mut transform, mut velocity_component, mut look) in
        &mut targets
    {
        let path: fn(u64) -> Vec3 = match player_id.0 {
            FLYING_TARGET_PLAYER_ID => flying_target_position,
            WALKING_TARGET_PLAYER_ID => walking_target_position,
            _ => continue,
        };
        let position = path(tick.0);
        let previous = path(tick.0.saturating_sub(1));
        let velocity = (position - previous) * FIXED_TIMESTEP_HZ as f32;

        physics_position.0 = position;
        transform.translation = position;
        **velocity_component = velocity;
        look.yaw = velocity.x.atan2(velocity.z) + std::f32::consts::PI;
        look.pitch = 0.0;
    }
}

/// Paces ~2 m/s along Z near the spawn area, on the floor (capsule center at
/// 0.75 = half the 1.5 cylinder height above the floor top at y=0).
fn walking_target_position(tick: u64) -> Vec3 {
    const CENTER: Vec3 = Vec3::new(-6.0, 0.75, 8.0);
    let seconds = tick as f32 / FIXED_TIMESTEP_HZ as f32;
    CENTER + Vec3::Z * (seconds * 0.7).sin() * 3.0
}

fn flying_target_position(tick: u64) -> Vec3 {
    const CENTER: Vec3 = Vec3::new(0.0, 4.2, -2.5);
    const RADIUS: f32 = 8.0;
    const ANGULAR_SPEED: f32 = 1.65;

    let seconds = tick as f32 / FIXED_TIMESTEP_HZ as f32;
    let angle = seconds * ANGULAR_SPEED;
    CENTER
        + Vec3::new(
            angle.cos() * RADIUS,
            angle.sin() * 0.9,
            angle.sin() * RADIUS,
        )
}

fn reset_fallen_players(
    mut players: Query<(
        &PlayerId,
        &mut Position,
        &mut Transform,
        &mut LinearVelocity,
        &mut CharacterControllerState,
    )>,
) {
    for (player_id, mut position, mut transform, mut velocity, mut controller_state) in &mut players
    {
        if position.y < -12.0 {
            let spawn = player_spawn_point(player_id.0);
            position.0 = spawn;
            transform.translation = spawn;
            **velocity = Vec3::ZERO;
            *controller_state = CharacterControllerState::default();
        }
    }
}
