//! Client-only rocket eye candy. The blast simulation lives in the library
//! (`bevy_netahoy::player`); this just draws the trail and explosion marker by
//! tracing the same rocket the predictor fires off the predicted player.
#![allow(dead_code)]

use avian3d::prelude::*;
use bevy::prelude::*;
use bevy_ahoy::CharacterLook;
use bevy_netahoy::*;

const ROCKET_DEBUG_SECONDS: f32 = 0.85;
const REMOTE_ROCKET_STREAK_METERS: f32 = 2.0;

pub fn add_client_rockets(app: &mut App) {
    app.add_observer(receive_rocket_fired)
        .add_observer(receive_rocket_hit)
        .add_systems(
            FixedPreUpdate,
            spawn_rocket_visual.after(ClientNetAhoySystems::Predict),
        )
        .add_systems(Update, update_rocket_markers);
}

#[derive(Component)]
struct RocketMarker {
    timer: Timer,
}

/// Spawns the fire visuals, exactly once per *accepted* shot. Edge-detecting
/// the raw fire button would flash on dry fire and cooldown-declined clicks;
/// instead this watermarks `weapon.shots_fired` from the predicted POD — the
/// counter only advances when the shared step accepts a fire, and after a
/// rewind that revokes a shot the watermark resyncs downward without replaying
/// effects. (You can't unplay a flash the server later declines; that one
/// mispredicted visual is the accepted cost of instant feedback.)
fn spawn_rocket_visual(
    mut last_shots_fired: Local<Option<u16>>,
    player: Query<(&Transform, &CharacterLook, &NetAhoyPlayerState), With<ClientPredictionKcc>>,
    spatial: SpatialQuery,
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    let Ok((transform, look, state)) = player.single() else {
        return;
    };

    let shots = state.weapon.shots_fired;
    let Some(last) = last_shots_fired.replace(shots) else {
        return;
    };
    // Wrapping "did it advance": a post-rewind decrease resyncs silently.
    let advanced = shots.wrapping_sub(last);
    if advanced == 0 || advanced > u16::MAX / 2 {
        return;
    }
    let look = Vec2::new(look.yaw, look.pitch);
    // Trace the same rocket the predictor will fire, so the marker lands where the
    // real blast goes off. It shows immediately even though the blast has travel
    // time; it only marks the landing spot, which is fine for now.
    let rocket = Rocket::fire(
        PlayerId::default(),
        0,
        transform.translation,
        look,
        &spatial,
    );
    let origin = rocket.start;
    let explosion = rocket.detonation_point();

    let material = rocket_material(&mut materials, Color::srgba(0.1, 0.9, 1.0, 0.7));

    spawn_rocket_trail(
        &mut commands,
        &mut meshes,
        material.clone(),
        origin,
        explosion,
        "rocket trail",
    );
    spawn_explosion_marker(
        &mut commands,
        &mut meshes,
        material,
        explosion,
        "rocket explosion",
    );
}

fn receive_rocket_fired(
    fired: On<RocketFired>,
    local: Res<LocalPlayerId>,
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    if local.0 == Some(fired.id.owner.0) {
        return;
    }

    let material = rocket_material(&mut materials, Color::srgba(1.0, 0.45, 0.1, 0.65));
    spawn_rocket_trail(
        &mut commands,
        &mut meshes,
        material,
        fired.start,
        fired.start + fired.dir * REMOTE_ROCKET_STREAK_METERS,
        "remote rocket trail",
    );
}

fn receive_rocket_hit(
    hit: On<RocketHit>,
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    let material = rocket_material(&mut materials, Color::srgba(1.0, 0.15, 0.08, 0.75));
    spawn_explosion_marker(
        &mut commands,
        &mut meshes,
        material,
        hit.point,
        "rocket hit",
    );
}

fn rocket_material(
    materials: &mut Assets<StandardMaterial>,
    color: Color,
) -> Handle<StandardMaterial> {
    materials.add(StandardMaterial {
        base_color: color,
        alpha_mode: AlphaMode::Blend,
        ..default()
    })
}

fn spawn_rocket_trail(
    commands: &mut Commands,
    meshes: &mut Assets<Mesh>,
    material: Handle<StandardMaterial>,
    origin: Vec3,
    explosion: Vec3,
    name: &'static str,
) {
    let segment = explosion - origin;
    let length = segment.length();
    if length > 0.001 {
        let direction = segment / length;
        let up = if direction.cross(Vec3::Y).length_squared() < 0.001 {
            Vec3::Z
        } else {
            Vec3::Y
        };
        let mut ray_transform = Transform::from_translation(origin + segment * 0.5);
        ray_transform.look_to(direction, up);
        commands.spawn((
            Name::new(name),
            RocketMarker {
                timer: Timer::from_seconds(ROCKET_DEBUG_SECONDS, TimerMode::Once),
            },
            Mesh3d(meshes.add(Cuboid::new(0.045, 0.045, length))),
            MeshMaterial3d(material),
            ray_transform,
        ));
    }
}

fn spawn_explosion_marker(
    commands: &mut Commands,
    meshes: &mut Assets<Mesh>,
    material: Handle<StandardMaterial>,
    explosion: Vec3,
    name: &'static str,
) {
    commands.spawn((
        Name::new(name),
        RocketMarker {
            timer: Timer::from_seconds(ROCKET_DEBUG_SECONDS, TimerMode::Once),
        },
        Mesh3d(meshes.add(Cuboid::new(0.35, 0.35, 0.35))),
        MeshMaterial3d(material),
        Transform::from_translation(explosion),
    ));
}

fn update_rocket_markers(
    mut commands: Commands,
    time: Res<Time>,
    mut markers: Query<(Entity, &mut RocketMarker, &mut Transform)>,
) {
    for (entity, mut marker, mut transform) in &mut markers {
        marker.timer.tick(time.delta());
        let remaining = marker.timer.remaining_secs() / ROCKET_DEBUG_SECONDS;
        transform.scale = Vec3::splat(remaining.max(0.15));
        if marker.timer.is_finished() {
            commands.entity(entity).despawn();
        }
    }
}
