//! What the player sees of the players, the platform and the gate: the local prediction and its
//! presentation (smoothed through an [`ErrorBridge`]), the remote players' visuals (drawn as
//! demons by `demon`) with their name tags, and capsules for the debug ghosts (F3): the server's
//! players and this player's prediction.

use avian3d::prelude::*;
use bevy::prelude::*;
use bevy_ahoy::CharacterLook;
use bevy_msdf_text::{MsdfText, MsdfTextMode};
use bevy_netahoy::*;

use playground::{
    GATE_CENTER, GATE_SIZE, Platform, ShieldGate,
    bots::player_name,
    level::{PLATFORM_SIZE, platform_collider},
    shared::{player_collision_layers, player_controller, player_mesh},
};

use crate::{dev_texture, scene::DevMaterials, text::Font};

/// `--show-ghosts`: the server's players drawn from the start.
pub fn remote_ghost_debug_from_args() -> RemoteGhostDebug {
    RemoteGhostDebug {
        visible: std::env::args()
            .any(|arg| arg == "--show-ghosts" || arg == "--show-remote-ghosts"),
    }
}

/// Whether the debug ghosts are drawn (F3): the server's players and this player's prediction.
#[derive(Resource, Default)]
pub struct RemoteGhostDebug {
    pub visible: bool,
}

/// The platform, once its first pose is in: its collider (static: only
/// [`place_platform`] moves it here) and a box to see it by.
pub fn attach_platform(
    mut commands: Commands,
    dev: Res<DevMaterials>,
    mut meshes: ResMut<Assets<Mesh>>,
    added: Query<(Entity, &BodySnapshot), (With<Platform>, Without<Collider>)>,
) {
    for (entity, snapshot) in &added {
        if snapshot.tick == 0 {
            continue;
        }
        commands.entity(entity).insert((
            platform_collider(),
            RigidBody::Static,
            Position::new(snapshot.position),
            Rotation(snapshot.rotation),
            Transform::from_translation(snapshot.position),
            Mesh3d(meshes.add(dev_texture::grid_box(PLATFORM_SIZE, Vec3::ZERO))),
            MeshMaterial3d(dev.blue.clone()),
        ));
    }
}

/// The platform where its history has it at the presented remote time
/// ([`PresentedFrame`]): drawn with the remote players, where this player's
/// commands see it. With no remote time yet it stays put, as they do.
pub fn place_platform(
    presented: Res<PresentedFrame>,
    tick_rate: Res<TickRate>,
    mut platforms: Query<(&History<BodySnapshot>, &mut Position, &mut Transform), With<Platform>>,
) {
    let Some(time) = presented.remote else {
        return;
    };
    for (history, mut position, mut transform) in &mut platforms {
        if let Some(pose) = body_at(history, time, *tick_rate) {
            position.0 = pose.position;
            transform.translation = pose.position;
        }
    }
}

/// The gate, once replicated: its panel, and a sill on the floor that shows where it stands
/// while it is down.
pub fn attach_gate(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    added: Query<Entity, Added<ShieldGate>>,
) {
    for entity in &added {
        let sill = Vec3::new(GATE_SIZE.x, 0.04, 0.3);
        commands.entity(entity).insert((
            Transform::from_translation(GATE_CENTER),
            Mesh3d(meshes.add(Cuboid::from_size(GATE_SIZE))),
            MeshMaterial3d(materials.add(StandardMaterial {
                base_color: Color::srgba(0.25, 0.80, 1.0, 0.55),
                emissive: LinearRgba::rgb(0.2, 0.9, 1.6),
                alpha_mode: AlphaMode::Blend,
                ..default()
            })),
            Visibility::Hidden,
            children![(
                Transform::from_xyz(0.0, sill.y / 2.0 - GATE_CENTER.y, 0.0),
                Mesh3d(meshes.add(Cuboid::from_size(sill))),
                MeshMaterial3d(materials.add(Color::srgb(0.20, 0.45, 0.60))),
                // Shown while the panel is hidden too.
                Visibility::Visible,
            )],
        ));
    }
}

/// The gate up or down as its history has it at the presented remote time ([`PresentedFrame`]):
/// the time this player's commands see it at, so a shot meets the gate drawn.
pub fn place_gate(
    presented: Res<PresentedFrame>,
    mut gates: Query<(&History<ShieldGate>, &mut Visibility)>,
) {
    let Some(time) = presented.remote else {
        return;
    };
    for (history, mut visibility) in &mut gates {
        visibility.set_if_neq(if ShieldGate::raised_at(history, time) {
            Visibility::Visible
        } else {
            Visibility::Hidden
        });
    }
}

/// A see-through capsule on every replicated player, its ghost: hidden unless asked for.
pub fn attach_player_meshes(
    mut commands: Commands,
    shape: Res<PlayerShape>,
    own: Res<OwnPlayerId>,
    debug_ghosts: Res<RemoteGhostDebug>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    players: Query<(Entity, &PlayerId), Added<ServerPlayer>>,
) {
    for (entity, player_id) in &players {
        let base_color = if own.is_assigned_to(*player_id) {
            Color::srgba(0.25, 0.65, 1.0, 0.28)
        } else {
            Color::srgba(1.0, 0.3, 0.2, 0.22)
        };

        commands.entity(entity).insert((
            Mesh3d(meshes.add(player_mesh(&shape))),
            MeshMaterial3d(materials.add(StandardMaterial {
                base_color,
                alpha_mode: AlphaMode::Blend,
                perceptual_roughness: 0.8,
                ..default()
            })),
            ghost_visibility(&debug_ghosts),
        ));
    }
}

/// The ghosts are the replicated player entities, drawn at the newest pose
/// the server sent (their history's newest entry; a late tick doesn't move
/// them back).
pub fn sync_debug_ghosts_from_snapshots(
    mut players: Query<
        (
            &History<PlayerSnapshot>,
            &mut Transform,
            Option<&mut CharacterLook>,
        ),
        (With<ServerPlayer>, Changed<History<PlayerSnapshot>>),
    >,
) {
    for (history, mut transform, look) in &mut players {
        let Some((_, newest)) = history.newest() else {
            continue;
        };
        transform.translation = newest.position;
        if let Some(mut look) = look {
            look.yaw = newest.look.x;
            look.pitch = newest.look.y;
        }
    }
}

/// Shown or hidden as the debug ghosts are.
fn ghost_visibility(debug_ghosts: &RemoteGhostDebug) -> Visibility {
    if debug_ghosts.visible {
        Visibility::Visible
    } else {
        Visibility::Hidden
    }
}

/// The local player's prediction: netahoy gives it the player's id and the server's state, and
/// places it there. Its capsule is a debug ghost; the demon is drawn on its presentation.
pub fn spawn_predicted_player(
    mut commands: Commands,
    shape: Res<PlayerShape>,
    debug_ghosts: Res<RemoteGhostDebug>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    local_players: Query<Entity, Added<OwnPlayer>>,
    predictions: Query<Entity, With<PredictedPlayer>>,
) {
    if predictions.iter().next().is_some() {
        return;
    }

    for server_entity in &local_players {
        let prediction_entity = commands
            .spawn((
                Name::new("client prediction kcc"),
                PredictedPlayer { server_entity },
                player_controller(),
                shape.collider(),
                player_collision_layers(),
                Mesh3d(meshes.add(player_mesh(&shape))),
                MeshMaterial3d(materials.add(StandardMaterial {
                    base_color: Color::srgba(0.1, 1.0, 0.45, 0.38),
                    alpha_mode: AlphaMode::Blend,
                    perceptual_roughness: 0.65,
                    ..default()
                })),
                ghost_visibility(&debug_ghosts),
            ))
            .id();

        // Placed on the prediction by `update_local_presentation`, later this frame.
        commands.spawn((
            Name::new("local presentation player"),
            OwnVisual { prediction_entity },
            Transform::default(),
            Visibility::default(),
        ));
    }
}

/// Peak acceleration (m/s²) a correction eases with; sets the spring's stiffness per fold.
const BRIDGE_MAX_ACCEL: f32 = 40.0;
/// Settle-window clamp for the bridge (95% converged ≈ 4.7/ω).
const BRIDGE_SETTLE_MIN_SECS: f32 = 0.08;
const BRIDGE_SETTLE_MAX_SECS: f32 = 0.35;

/// Critically damped error bridge: `rendered = sim + offset`, the offset
/// decaying along x(t) = (A + Bt)·e^(−ωt) (exact step, framerate
/// independent). Folding a correction keeps both offset and its velocity, so
/// the rendered path stays C1 through it.
#[derive(Default, Clone, Copy)]
pub struct ErrorBridge {
    offset: Vec3,
    velocity: Vec3,
    omega: f32,
}

impl ErrorBridge {
    fn advance(&mut self, dt: f32) {
        if self.offset.length_squared() < 1e-6 && self.velocity.length_squared() < 1e-4 {
            *self = Self::default();
            return;
        }
        let a = self.offset;
        let b = self.velocity + self.omega * self.offset;
        let decay = (-self.omega * dt).exp();
        self.offset = (a + b * dt) * decay;
        self.velocity = (b - self.omega * (a + b * dt)) * decay;
    }

    /// Fold a correction in, re-sizing ω from the comfort accel so big
    /// corrections take longer instead of yanking: peak accel is ω²·e₀
    /// (position term) and 2ω·v₀ (velocity term).
    fn fold(&mut self, de: Vec3, dv: Vec3) {
        self.offset += de;
        self.velocity += dv;
        let e0 = self.offset.length().max(1e-5);
        let v0 = self.velocity.length().max(1e-4);
        let omega_pos = (BRIDGE_MAX_ACCEL / e0).sqrt();
        let omega_vel = BRIDGE_MAX_ACCEL / (2.0 * v0);
        self.omega = omega_pos
            .min(omega_vel)
            .clamp(4.7 / BRIDGE_SETTLE_MAX_SECS, 4.7 / BRIDGE_SETTLE_MIN_SECS);
    }
}

/// The local player's visual: the prediction plus an [`ErrorBridge`] each correction folds
/// into, so the sim takes the correction at once and the drawn pose eases there. Past its
/// `snap_gate` a correction pops instead.
pub fn update_local_presentation(
    time: Res<Time>,
    mut predictions: Query<
        (&Transform, &mut PendingCorrection),
        (With<PredictedPlayer>, Without<OwnVisual>),
    >,
    mut presentations: Query<(&OwnVisual, &mut Transform), Without<PredictedPlayer>>,
    mut bridge: Local<ErrorBridge>,
) {
    let Ok((presentation, mut transform)) = presentations.single_mut() else {
        *bridge = ErrorBridge::default();
        return;
    };
    let Ok((prediction, mut pending)) = predictions.get_mut(presentation.prediction_entity) else {
        return;
    };
    match pending.take() {
        PendingCorrection::None => {}
        PendingCorrection::Snap => *bridge = ErrorBridge::default(),
        PendingCorrection::Fold {
            position,
            velocity,
            snap_gate,
        } => {
            if position.length() >= snap_gate {
                *bridge = ErrorBridge::default();
            } else {
                bridge.fold(position, velocity);
            }
        }
    }
    bridge.advance(time.delta_secs());
    transform.translation = prediction.translation + bridge.offset;
}

/// A visual for every remote player, once it has a pose to stand at.
pub fn spawn_remote_visuals(
    mut commands: Commands,
    remotes: Query<(Entity, &PlayerId, &History<PlayerSnapshot>), With<RemotePlayer>>,
    visuals: Query<&RemoteVisual>,
) {
    for (server_entity, player_id, history) in &remotes {
        let Some((_, newest)) = history.newest() else {
            continue;
        };
        if visuals
            .iter()
            .any(|visual| visual.server_entity == server_entity)
        {
            continue;
        }

        let (position, look) = (newest.position, newest.look);

        commands.spawn((
            Name::new(format!("remote player {} visual", player_id.0)),
            RemoteVisual {
                server_entity,
                player_id: *player_id,
            },
            CharacterLook {
                yaw: look.x,
                pitch: look.y,
            },
            Transform::from_translation(position),
            Visibility::default(),
        ));
    }
}

/// F3 shows or hides the debug ghosts.
pub fn toggle_remote_ghost_debug(
    keys: Res<ButtonInput<KeyCode>>,
    mut debug_ghosts: ResMut<RemoteGhostDebug>,
    mut ghosts: Query<&mut Visibility, Or<(With<ServerPlayer>, With<PredictedPlayer>)>>,
) {
    if !keys.just_pressed(KeyCode::F3) {
        return;
    }

    debug_ghosts.visible = !debug_ghosts.visible;
    let visibility = ghost_visibility(&debug_ghosts);
    for mut ghost_visibility in &mut ghosts {
        *ghost_visibility = visibility;
    }
}

/// On a remote player's visual: its name tag, and the speed it last showed.
#[derive(Component)]
pub struct NameTag {
    text: Entity,
    shown: Option<u32>,
}

/// A name tag over every new remote player's head: a fixed size on screen, whatever the distance.
pub fn attach_name_tags(
    mut commands: Commands,
    font: Res<Font>,
    shape: Res<PlayerShape>,
    visuals: Query<(Entity, &RemoteVisual), Added<RemoteVisual>>,
) {
    for (visual, remote) in &visuals {
        let text = commands
            .spawn((
                font.text(
                    player_name(remote.player_id),
                    MsdfTextMode::WorldFixedPx,
                    15.0,
                )
                .with_anchor(Vec2::new(0.0, -0.5)),
                Transform::from_xyz(0.0, shape.half_height + 0.35, 0.0),
                ChildOf(visual),
            ))
            .id();
        commands
            .entity(visual)
            .insert(NameTag { text, shown: None });
    }
}

/// Each tag says how fast its player is going, from the velocity its visual is drawn with.
pub fn update_name_tags(
    mut tags: Query<(&RemoteVisual, &RemoteSampledFrame, &mut NameTag)>,
    mut texts: Query<&mut MsdfText>,
) {
    for (visual, frame, mut tag) in &mut tags {
        let speed = frame.0.velocity.xz().length().round() as u32;
        if tag.shown == Some(speed) {
            continue;
        }
        tag.shown = Some(speed);
        if let Ok(mut text) = texts.get_mut(tag.text) {
            text.text = format!("{}  {speed} m/s", player_name(visual.player_id));
        }
    }
}
