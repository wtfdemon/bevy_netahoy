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
mod jumppad;
mod rockets;
mod shared;
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
            MovementExtrasPlugin,
            ServerPlugin,
        ))
        .run()
}

struct ServerPlugin;

impl Plugin for ServerPlugin {
    fn build(&self, app: &mut App) {
        hitscan::add_server_hitscan(app);

        app.add_plugins((WebSocketServerPlugin, AeronetRepliconServerPlugin))
            .add_observer(join_player)
            .add_observer(clean_up_disconnected_player)
            .add_systems(Startup, setup_server)
            .add_systems(
                FixedPreUpdate,
                (update_flying_target, reset_fallen_players)
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
        mode: SendMode::Direct(ClientId::Client(client)),
        message: JoinAccepted { player_id },
    });

    info!("client {client} joined as player {player_id}");

    commands.spawn((
        Name::new(format!("player {player_id}")),
        Replicated,
        NetworkedPlayer,
        PlayerId(player_id),
        AhoySnapshot::default(),
        PlayerOwner(client),
        ServerCommandBuffer::default(),
        QueuedUserCmds::default(),
        CharacterLook::default(),
        MovementExtrasState::default(),
        player_controller(),
        Collider::cylinder(0.45, 1.5),
        player_collision_layers(),
        Transform::from_translation(player_spawn_point(player_id)),
    ));
}

fn spawn_flying_target(commands: &mut Commands) {
    let position = flying_target_position(0);
    commands.spawn((
        Name::new("flying target player"),
        Replicated,
        NetworkedPlayer,
        PlayerId(FLYING_TARGET_PLAYER_ID),
        AhoySnapshot::default(),
        ServerCommandBuffer::default(),
        CharacterLook::default(),
        CharacterControllerState::default(),
        Position::new(position),
        Rotation::IDENTITY,
        LinearVelocity::ZERO,
        Transform::from_translation(position),
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

fn update_flying_target(
    tick: Res<ServerTick>,
    mut targets: Query<
        (
            &PlayerId,
            &mut Position,
            &mut Transform,
            &mut LinearVelocity,
            &mut CharacterLook,
        ),
        Without<PlayerOwner>,
    >,
) {
    let position = flying_target_position(tick.0);
    let previous = flying_target_position(tick.0.saturating_sub(1));
    let velocity = (position - previous) * FIXED_TIMESTEP_HZ as f32;
    let yaw = velocity.x.atan2(velocity.z) + std::f32::consts::PI;

    for (player_id, mut physics_position, mut transform, mut velocity_component, mut look) in
        &mut targets
    {
        if player_id.0 != FLYING_TARGET_PLAYER_ID {
            continue;
        }

        physics_position.0 = position;
        transform.translation = position;
        **velocity_component = velocity;
        look.yaw = yaw;
        look.pitch = 0.0;
    }
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
