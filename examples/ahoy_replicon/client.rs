use aeronet::io::{
    connection::{DisconnectReason, Disconnected},
    Session, SessionEndpoint,
};
use aeronet_replicon::client::{AeronetRepliconClient, AeronetRepliconClientPlugin};
use aeronet_websocket::client::{ClientConfig, WebSocketClient, WebSocketClientPlugin};
use avian3d::prelude::*;
use bevy::{
    app::{RunFixedMainLoop, RunFixedMainLoopSystems},
    input::{common_conditions::input_just_pressed, mouse::AccumulatedMouseMotion},
    prelude::*,
    window::{CursorGrabMode, CursorOptions, PresentMode},
    winit::{UpdateMode::Continuous, WinitSettings},
};
use bevy_ahoy::{prelude::*, CharacterLook};
use bevy_enhanced_input::prelude::EnhancedInputPlugin;
use bevy_netahoy::*;
use bevy_replicon::prelude::*;

mod hitscan;
mod rockets;
mod shared;
mod vehicle;
use hitscan::ExampleHitscanClientSystems;
use shared::*;

const CAMERA_DISTANCE: f32 = 5.2;
const CAMERA_HEIGHT: f32 = 0.85;
const CAMERA_SHOULDER_OFFSET: f32 = 1.25;
const CAMERA_AIM_RIGHT_OFFSET: f32 = 0.85;

fn main() -> AppExit {
    let poor_network = poor_network_from_args();
    let time_scale = debug_time_scale_from_args();
    let remote_ghost_debug = remote_ghost_debug_from_args();

    let mut app = App::new();
    app.insert_resource(ClientLook::default())
        .insert_resource(KccStateDebug::default())
        .insert_resource(remote_ghost_debug)
        .insert_resource(time_scale)
        .insert_resource(WinitSettings {
            focused_mode: Continuous,
            unfocused_mode: Continuous,
        });

    if poor_network {
        app.add_plugins(AeronetNetworkConditionerPlugin::poor_condition());
    }

    app.add_plugins((
        DefaultPlugins.set(WindowPlugin {
            primary_window: Some(Window {
                title: "NetAhoy client".to_string(),
                resolution: (1280, 720).into(),
                present_mode: PresentMode::AutoVsync,
                ..default()
            }),
            ..default()
        }),
        PhysicsPlugins::default(),
        EnhancedInputPlugin,
        AhoyPlugins::new(NetAhoyKccSchedule),
        ExampleSharedPlugin,
        ClientNetAhoyPlugin,
        ClientPlugin,
    ))
    .run()
}

fn remote_ghost_debug_from_args() -> RemoteGhostDebug {
    RemoteGhostDebug {
        visible: std::env::args()
            .any(|arg| arg == "--show-ghosts" || arg == "--show-remote-ghosts"),
    }
}

#[derive(Resource, Default)]
struct ClientLook {
    yaw: f32,
    pitch: f32,
}

#[derive(Resource, Default)]
struct KccStateDebug {
    predicted_seen_mantle: bool,
    server_seen_mantle: bool,
}

#[derive(Resource, Default)]
struct RemoteGhostDebug {
    visible: bool,
}

#[derive(Component)]
struct SpeedText;

#[derive(Component)]
struct StatusText;

#[derive(Component)]
struct PredictionText;

#[derive(Component)]
struct KccStateText;

type CameraRigFilter = (
    With<Camera3d>,
    Without<ClientPredictionKcc>,
    Without<ServerTruthGhost>,
    Without<LocalPresentationPlayer>,
);

struct ClientPlugin;

impl Plugin for ClientPlugin {
    fn build(&self, app: &mut App) {
        hitscan::add_client_hitscan(app);
        rockets::add_client_rockets(app);
        vehicle::add_client_vehicles(app);

        app.add_plugins((WebSocketClientPlugin, AeronetRepliconClientPlugin))
            .add_observer(use_replicon_for_session)
            .add_observer(set_window_title_on_join)
            .add_observer(log_connected)
            .add_observer(log_disconnected)
            .add_systems(
                Startup,
                (setup_client, setup_scene, setup_hud),
            )
            .add_systems(
                Update,
                (
                    capture_cursor.run_if(input_just_pressed(MouseButton::Left)),
                    release_cursor.run_if(input_just_pressed(KeyCode::Escape)),
                    update_client_look,
                )
                    .chain(),
            )
            .add_systems(
                RunFixedMainLoop,
                gather_client_input.in_set(RunFixedMainLoopSystems::BeforeFixedMainLoop),
            )
            .add_systems(
                Update,
                (
                    attach_player_meshes,
                    sync_debug_ghosts_from_snapshots,
                    spawn_client_prediction_kcc,
                    spawn_remote_player_visuals,
                    attach_remote_player_colliders,
                    apply_remote_player_capsule_poses,
                    toggle_remote_ghost_debug,
                    update_camera_from_local_presentation,
                    update_speed_text,
                    update_status_text,
                    update_kcc_state_text,
                    update_prediction_text,
                )
                    .chain()
                    .after(ClientNetAhoySystems::Interpolate)
                    .before(ExampleHitscanClientSystems::Fire),
            );
    }
}

fn setup_client(mut commands: Commands) {
    commands
        .spawn(Name::new("Client"))
        .queue(WebSocketClient::connect(
            client_config(),
            DEFAULT_SERVER_URL,
        ));

    info!("websocket client connecting to {DEFAULT_SERVER_URL}");
}

fn set_window_title_on_join(accepted: On<JoinAccepted>, mut windows: Query<&mut Window>) {
    for mut window in &mut windows {
        window.title = format!("NetAhoy client {}", accepted.player_id);
    }
}

fn use_replicon_for_session(session: On<Add, SessionEndpoint>, mut commands: Commands) {
    commands
        .entity(session.event_target())
        .insert(AeronetRepliconClient);
}

fn log_connected(session: On<Add, Session>) {
    info!("websocket client connected: {}", session.event_target());
}

fn log_disconnected(disconnected: On<Disconnected>) {
    match &disconnected.reason {
        DisconnectReason::ByUser(reason) => info!("websocket client disconnected: {reason}"),
        DisconnectReason::ByPeer(reason) => {
            warn!("websocket client disconnected by server: {reason}")
        }
        DisconnectReason::ByError(err) => warn!("websocket client disconnected: {err:#}"),
    }
}

#[cfg(target_family = "wasm")]
fn client_config() -> ClientConfig {
    ClientConfig::default()
}

#[cfg(not(target_family = "wasm"))]
fn client_config() -> ClientConfig {
    ClientConfig::builder().with_no_encryption()
}

fn setup_scene(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    commands.spawn((
        DirectionalLight {
            illuminance: 18_000.0,
            shadows_enabled: true,
            ..default()
        },
        Transform::from_xyz(-6.0, 12.0, 8.0).looking_at(Vec3::ZERO, Vec3::Y),
    ));

    spawn_world_render(&mut commands, &mut meshes, &mut materials);
    spawn_world_colliders(&mut commands);

    commands.spawn((
        Camera3d::default(),
        Projection::Perspective(PerspectiveProjection {
            fov: 80.0_f32.to_radians(),
            ..default()
        }),
        Transform::from_translation(SPAWN_POINT + Vec3::new(0.0, 3.0, 7.0))
            .looking_at(SPAWN_POINT, Vec3::Y),
    ));
}

fn setup_hud(mut commands: Commands) {
    commands.spawn((
        Node {
            position_type: PositionType::Absolute,
            top: px(16.0),
            left: px(16.0),
            ..default()
        },
        Text::new("connecting"),
        TextColor(Color::WHITE.with_alpha(0.65)),
        StatusText,
    ));

    commands.spawn((
        Node {
            position_type: PositionType::Absolute,
            top: px(40.0),
            left: px(16.0),
            ..default()
        },
        Text::new("prediction: waiting"),
        TextColor(Color::WHITE.with_alpha(0.55)),
        PredictionText,
    ));

    commands.spawn((
        Node {
            position_type: PositionType::Absolute,
            top: px(88.0),
            left: px(16.0),
            ..default()
        },
        Text::new("kcc: waiting"),
        TextColor(Color::WHITE.with_alpha(0.55)),
        KccStateText,
    ));

    commands.spawn((
        Node {
            position_type: PositionType::Absolute,
            bottom: px(56.0),
            width: Val::Percent(100.0),
            justify_content: JustifyContent::Center,
            ..default()
        },
        Text::new("0.000"),
        TextColor(Color::WHITE.with_alpha(0.55)),
        SpeedText,
    ));

    commands
        .spawn(Node {
            position_type: PositionType::Absolute,
            width: Val::Percent(100.0),
            height: Val::Percent(100.0),
            justify_content: JustifyContent::Center,
            align_items: AlignItems::Center,
            ..default()
        })
        .with_children(|parent| {
            parent.spawn((
                Node {
                    position_type: PositionType::Absolute,
                    width: px(14.0),
                    height: px(2.0),
                    ..default()
                },
                BackgroundColor(Color::WHITE.with_alpha(0.35)),
            ));
            parent.spawn((
                Node {
                    position_type: PositionType::Absolute,
                    width: px(2.0),
                    height: px(14.0),
                    ..default()
                },
                BackgroundColor(Color::WHITE.with_alpha(0.35)),
            ));
        });
}

fn player_display_color(player_id: PlayerId) -> Color {
    if player_id.0 == FLYING_TARGET_PLAYER_ID {
        Color::srgb(1.0, 0.25, 0.68)
    } else {
        player_color(player_id.0)
    }
}

fn attach_player_meshes(
    mut commands: Commands,
    local: Res<LocalPlayerId>,
    debug_ghosts: Res<RemoteGhostDebug>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    players: Query<(Entity, &PlayerId), Added<NetworkedPlayer>>,
) {
    for (entity, player_id) in &players {
        let is_local_server_truth = local.is_assigned_to(player_id.0);
        let (base_color, alpha_mode, visibility) = if is_local_server_truth {
            (
                Color::srgba(0.25, 0.65, 1.0, 0.28),
                AlphaMode::Blend,
                Visibility::Visible,
            )
        } else {
            (
                player_display_color(*player_id).with_alpha(0.18),
                AlphaMode::Blend,
                if debug_ghosts.visible {
                    Visibility::Visible
                } else {
                    Visibility::Hidden
                },
            )
        };

        commands.entity(entity).insert((
            Mesh3d(meshes.add(Capsule3d::new(PLAYER_CAPSULE_RADIUS, 1.5))),
            MeshMaterial3d(materials.add(StandardMaterial {
                base_color,
                alpha_mode,
                perceptual_roughness: 0.8,
                ..default()
            })),
            visibility,
        ));
    }
}

fn sync_debug_ghosts_from_snapshots(
    mut players: Query<
        (&AhoySnapshot, &mut Transform, Option<&mut CharacterLook>),
        (
            With<NetworkedPlayer>,
            Or<(Changed<AhoySnapshot>, Added<Transform>)>,
        ),
    >,
) {
    for (snapshot, mut transform, look) in &mut players {
        if snapshot.server_tick == 0 {
            continue;
        }

        transform.translation = snapshot.position;
        if let Some(mut look) = look {
            look.yaw = snapshot.look.x;
            look.pitch = snapshot.look.y;
        }
    }
}

fn spawn_client_prediction_kcc(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    local_players: Query<(Entity, &PlayerId, &Transform, Option<&AhoySnapshot>), Added<ServerTruthGhost>>,
    predictions: Query<Entity, With<ClientPredictionKcc>>,
) {
    if predictions.iter().next().is_some() {
        return;
    }

    for (server_entity, player_id, transform, authoritative_state) in &local_players {
        let position = authoritative_state
            .map(|state| state.position)
            .unwrap_or(transform.translation);
        let look = authoritative_state
            .map(|state| state.look)
            .unwrap_or(Vec2::ZERO);

        let prediction_entity = commands
            .spawn((
                Name::new("client prediction kcc"),
                ClientPredictionKcc { server_entity },
                *player_id,
                CharacterLook {
                    yaw: look.x,
                    pitch: look.y,
                },
                player_controller(),
                Collider::cylinder(PLAYER_CAPSULE_RADIUS, 1.5),
                player_collision_layers(),
                Position::new(position),
                Rotation::IDENTITY,
                LinearVelocity::ZERO,
                Mesh3d(meshes.add(Capsule3d::new(PLAYER_CAPSULE_RADIUS, 1.5))),
                MeshMaterial3d(materials.add(StandardMaterial {
                    base_color: Color::srgba(0.1, 1.0, 0.45, 0.38),
                    alpha_mode: AlphaMode::Blend,
                    perceptual_roughness: 0.65,
                    ..default()
                })),
                Transform::from_translation(position),
                Visibility::Visible,
            ))
            .id();

        commands.spawn((
            Name::new("local presentation player"),
            LocalPresentationPlayer { prediction_entity },
            Mesh3d(meshes.add(Capsule3d::new(PLAYER_CAPSULE_RADIUS, 1.5))),
            MeshMaterial3d(materials.add(StandardMaterial {
                base_color: Color::srgb(1.0, 0.83, 0.22),
                perceptual_roughness: 0.7,
                ..default()
            })),
            Transform::from_translation(position),
            Visibility::default(),
        ));
    }
}

/// Remote players get a solid capsule at their newest-snapshot pose, so
/// prediction collides where the server will — NOT at the interpolated visual
/// pose, which is ~200 ms in the past. Standing players block perfectly;
/// running ones cost a small (~RTT) ghost offset, the accepted trade.
/// `RigidBody::Static` per the KCC-solidity rule: the controller only treats
/// colliders attached to a body as solid, and static bodies never integrate
/// velocity, so the airlock below stays the pose's only author. The local
/// player's ghost never matches (no `RemoteInterpolationBuffer`), and the
/// flying target is skipped — it has no server collider, so a client capsule
/// would make prediction collide with something the server ghosts through.
fn attach_remote_player_colliders(
    mut commands: Commands,
    remotes: Query<
        (Entity, &PlayerId, &AhoySnapshot),
        (With<RemoteInterpolationBuffer>, Without<Collider>),
    >,
) {
    for (entity, player_id, snapshot) in &remotes {
        if player_id.0 == FLYING_TARGET_PLAYER_ID || snapshot.server_tick == 0 {
            continue;
        }
        commands.entity(entity).insert((
            Collider::cylinder(PLAYER_CAPSULE_RADIUS, 1.5),
            player_collision_layers(),
            RigidBody::Static,
            Position::new(snapshot.position),
        ));
    }
}

/// The airlock for remote player capsules: newest snapshot → Position, every
/// frame, unconditionally. (The flying target never gets a Position, so it
/// never matches.)
fn apply_remote_player_capsule_poses(
    mut remotes: Query<(&AhoySnapshot, &mut Position), With<RemoteInterpolationBuffer>>,
) {
    for (snapshot, mut position) in &mut remotes {
        if snapshot.server_tick != 0 {
            position.0 = snapshot.position;
        }
    }
}

fn spawn_remote_player_visuals(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    remotes: Query<
        (Entity, &PlayerId, &Transform, Option<&AhoySnapshot>),
        Added<RemoteInterpolationBuffer>,
    >,
    visuals: Query<&RemotePlayerVisual>,
) {
    for (server_entity, player_id, transform, snapshot) in &remotes {
        if visuals
            .iter()
            .any(|visual| visual.server_entity == server_entity)
        {
            continue;
        }

        let snapshot = snapshot.filter(|snapshot| snapshot.server_tick != 0);
        let position = snapshot
            .map(|snapshot| snapshot.position)
            .unwrap_or(transform.translation);
        let look = snapshot.map(|snapshot| snapshot.look).unwrap_or(Vec2::ZERO);

        commands.spawn((
            Name::new(format!("remote player {} visual", player_id.0)),
            RemotePlayerVisual {
                server_entity,
                player_id: *player_id,
            },
            CharacterLook {
                yaw: look.x,
                pitch: look.y,
            },
            Mesh3d(meshes.add(Capsule3d::new(PLAYER_CAPSULE_RADIUS, 1.5))),
            MeshMaterial3d(materials.add(StandardMaterial {
                base_color: player_display_color(*player_id),
                perceptual_roughness: 0.75,
                ..default()
            })),
            Transform::from_translation(position),
            Visibility::default(),
        ));
    }
}

fn toggle_remote_ghost_debug(
    keys: Res<ButtonInput<KeyCode>>,
    mut debug_ghosts: ResMut<RemoteGhostDebug>,
    mut ghosts: Query<
        &mut Visibility,
        (With<RemoteInterpolationBuffer>, Without<ServerTruthGhost>),
    >,
) {
    if !keys.just_pressed(KeyCode::F3) {
        return;
    }

    debug_ghosts.visible = !debug_ghosts.visible;
    let visibility = if debug_ghosts.visible {
        Visibility::Visible
    } else {
        Visibility::Hidden
    };

    for mut ghost_visibility in &mut ghosts {
        *ghost_visibility = visibility;
    }
}

fn capture_cursor(mut cursor: Single<&mut CursorOptions>) {
    cursor.grab_mode = CursorGrabMode::Locked;
    cursor.visible = false;
}

fn release_cursor(mut cursor: Single<&mut CursorOptions>) {
    cursor.visible = true;
    cursor.grab_mode = CursorGrabMode::None;
}

fn update_client_look(
    cursor: Single<&CursorOptions>,
    mouse_motion: Res<AccumulatedMouseMotion>,
    mut look: ResMut<ClientLook>,
) {
    if cursor.grab_mode != CursorGrabMode::Locked {
        return;
    }

    const MOUSE_SENSITIVITY: f32 = 0.07_f32.to_radians();
    look.yaw -= mouse_motion.delta.x * MOUSE_SENSITIVITY;
    look.pitch = (look.pitch - mouse_motion.delta.y * MOUSE_SENSITIVITY).clamp(-1.5, 1.5);
}

fn gather_client_input(
    keys: Res<ButtonInput<KeyCode>>,
    mouse: Res<ButtonInput<MouseButton>>,
    look: Res<ClientLook>,
    mut input: ResMut<ClientInput>,
) {
    let mut movement = Vec2::ZERO;
    if keys.pressed(KeyCode::KeyW) {
        movement.y += 1.0;
    }
    if keys.pressed(KeyCode::KeyS) {
        movement.y -= 1.0;
    }
    if keys.pressed(KeyCode::KeyA) {
        movement.x -= 1.0;
    }
    if keys.pressed(KeyCode::KeyD) {
        movement.x += 1.0;
    }

    input.movement = movement;
    input.look = Vec2::new(look.yaw, look.pitch);
    let mut buttons = AhoyButtons::empty();
    buttons.set(AhoyButtons::JUMP, keys.pressed(KeyCode::Space));
    buttons.set(
        AhoyButtons::CROUCH,
        keys.pressed(KeyCode::ControlLeft) || keys.pressed(KeyCode::KeyC),
    );
    // Tac shares the jump button, exactly like Ahoy (Space drives jump/tac/
    // crane/mantle and the state machine picks). ShiftLeft kept to isolate-test.
    buttons.set(
        AhoyButtons::TAC,
        keys.pressed(KeyCode::Space) || keys.pressed(KeyCode::ShiftLeft),
    );
    // Like Ahoy, Space also drives crane + mantle; E/Q kept to isolate-test.
    buttons.set(
        AhoyButtons::MANTLE,
        keys.pressed(KeyCode::Space) || keys.pressed(KeyCode::KeyE),
    );
/*     buttons.set(
        AhoyButtons::CRANE,
        keys.pressed(KeyCode::Space) || keys.pressed(KeyCode::KeyQ),
    ); */
    buttons.set(AhoyButtons::CLIMBDOWN, keys.pressed(KeyCode::KeyZ));
    buttons.set(AhoyButtons::SWIM_UP, keys.pressed(KeyCode::Space));
    buttons.set(
        ROCKET_FIRE,
        mouse.pressed(MouseButton::Right) || keys.pressed(KeyCode::KeyF),
    );
    input.buttons = buttons;
}

fn update_camera_from_local_presentation(
    look: Res<ClientLook>,
    driven: Query<&Transform, (With<vehicle::LocalVehicleSim>, Without<Camera3d>)>,
    presentations: Query<&Transform, (With<LocalPresentationPlayer>, Without<Camera3d>)>,
    server_players: Query<&AhoySnapshot, With<ServerTruthGhost>>,
    mut camera: Single<&mut Transform, CameraRigFilter>,
) {
    // While driving, orbit the locally simulated buggy (there is no capsule).
    let target = driven
        .single()
        .map(|transform| transform.translation + Vec3::Y * 0.9)
        .or_else(|_| {
            presentations
                .single()
                .map(|transform| transform.translation + Vec3::Y * 0.6)
        })
        .or_else(|_| {
            server_players
                .single()
                .map(|snapshot| snapshot.position + Vec3::Y * 0.6)
        })
        .unwrap_or(SPAWN_POINT);
    let rotation = Quat::from_euler(EulerRot::YXZ, look.yaw, look.pitch, 0.0);
    let forward = rotation * Vec3::NEG_Z;
    let right = rotation * Vec3::X;
    let camera_target = target + forward * 8.0 + right * CAMERA_AIM_RIGHT_OFFSET;

    camera.translation = target - forward * CAMERA_DISTANCE
        + right * CAMERA_SHOULDER_OFFSET
        + Vec3::Y * CAMERA_HEIGHT;
    camera.look_at(camera_target, Vec3::Y);
}

fn update_speed_text(
    mut text: Single<&mut Text, With<SpeedText>>,
    sim_velocity: Option<Single<&LinearVelocity, With<vehicle::LocalVehicleSim>>>,
    predicted_velocity: Option<Single<&LinearVelocity, With<ClientPredictionKcc>>>,
    server_velocity: Option<Single<&LinearVelocity, With<ServerTruthGhost>>>,
) {
    if let Some(velocity) = sim_velocity {
        text.0 = format!("driving {:.3}", velocity.xz().length());
    } else if let Some(velocity) = predicted_velocity {
        text.0 = format!("predicted {:.3}", velocity.xz().length());
    } else if let Some(velocity) = server_velocity {
        text.0 = format!("{:.3}", velocity.xz().length());
    }
}

fn update_status_text(
    local: Res<LocalPlayerId>,
    state: Res<State<ClientState>>,
    time_scale: Res<DebugTimeScale>,
    debug_ghosts: Res<RemoteGhostDebug>,
    mut text: Single<&mut Text, With<StatusText>>,
) {
    let mut status = format!("{} - {:?}", local.label(), state.get());
    if time_scale.is_scaled() {
        status.push_str(&format!(" - slowmo {:.2}x", time_scale.factor));
    }
    if debug_ghosts.visible {
        status.push_str(" - ghosts");
    }
    text.0 = status;
}

fn update_kcc_state_text(
    mut text: Single<&mut Text, With<KccStateText>>,
    predicted_state: Option<Single<&CharacterControllerState, With<ClientPredictionKcc>>>,
    server_snapshot: Option<Single<&AhoySnapshot, With<ServerTruthGhost>>>,
    mut debug: ResMut<KccStateDebug>,
) {
    let predicted = predicted_state.map(|state| NetAhoyMoveState::from_controller_state(*state));
    let server = server_snapshot
        .filter(|snapshot| snapshot.server_tick != 0)
        .map(|snapshot| snapshot.state);

    debug.predicted_seen_mantle |= predicted
        .as_ref()
        .is_some_and(|state| state.mantle_height_left.is_some());
    debug.server_seen_mantle |= server
        .as_ref()
        .is_some_and(|state| state.mantle_height_left.is_some());

    let predicted_label = predicted
        .map(format_kcc_state)
        .unwrap_or_else(|| "predicted waiting".to_string());
    let server_label = server
        .map(format_kcc_state)
        .unwrap_or_else(|| "server waiting".to_string());
    let mantle_seen = match (debug.predicted_seen_mantle, debug.server_seen_mantle) {
        (true, true) => "mantle seen predicted+server",
        (true, false) => "mantle seen predicted",
        (false, true) => "mantle seen server",
        (false, false) => "mantle not seen",
    };

    text.0 = format!("kcc: {predicted_label} | {server_label} | {mantle_seen}");
}

fn format_kcc_state(state: NetAhoyMoveState) -> String {
    if let Some(height_left) = state.mantle_height_left {
        format!("mantle {height_left:.2}m")
    } else if let Some(height_left) = state.crane_height_left {
        format!("crane {height_left:.2}m")
    } else if state.crouching {
        "crouch".to_string()
    } else if state.grounded {
        "ground".to_string()
    } else {
        "air".to_string()
    }
}

fn update_prediction_text(
    mut text: Single<&mut Text, With<PredictionText>>,
    input_state: Res<ClientInputState>,
    prediction: Option<Single<&PredictionCorrection, With<ClientPredictionKcc>>>,
) {
    let Some(prediction) = prediction else {
        text.0 = "prediction: waiting".to_string();
        return;
    };

    let ack_lag = input_state
        .next_sequence
        .wrapping_sub(prediction.last_ack_sequence);
    text.0 = format!(
        "prediction: {:?} | error {:.3}m | ack {} | lag {} | replay {} | offset {:.3}m",
        prediction.mode,
        prediction.last_error,
        prediction.last_ack_sequence,
        ack_lag,
        prediction.replayed_commands,
        prediction.presentation_offset.length()
    );
}
