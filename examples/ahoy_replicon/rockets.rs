//! Client-only rocket eye candy. The blast simulation lives in the library
//! (`bevy_netahoy::extras`); this just draws the trail and explosion marker by
//! tracing the same `rocket_explosion_point` off the predicted player.
#![allow(dead_code)]

use avian3d::prelude::*;
use bevy::prelude::*;
use bevy_ahoy::CharacterLook;
use bevy_netahoy::*;

const ROCKET_DEBUG_SECONDS: f32 = 0.85;

pub fn add_client_rockets(app: &mut App) {
    app.add_systems(
        FixedPreUpdate,
        spawn_rocket_visual.after(ClientNetAhoySystems::Predict),
    )
    .add_systems(Update, update_rocket_markers);
}

#[derive(Component)]
struct RocketMarker {
    timer: Timer,
}

/// Spawns the explosion marker, one per shot. Client-only and never replayed, so it
/// fires once, redoing the raycast off the predicted player to match the real impulse.
fn spawn_rocket_visual(
    input: Res<ClientInput>,
    mut fired: Local<bool>,
    player: Query<(&Transform, &CharacterLook), With<ClientPredictionKcc>>,
    spatial: SpatialQuery,
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    let pressed = input.buttons.contains(ROCKET_FIRE);
    let edge = pressed && !*fired;
    *fired = pressed;
    if !edge {
        return;
    }

    let Ok((transform, look)) = player.single() else {
        return;
    };
    let look = Vec2::new(look.yaw, look.pitch);
    // The marker shows up right away even though the real blast has travel time.
    // It only marks where the rocket will land, so that is fine for now.
    let Some((explosion, _distance)) = rocket_explosion_point(transform.translation, look, &spatial)
    else {
        return;
    };
    let origin = transform.translation + Vec3::Y * ROCKET_EYE_HEIGHT;

    let material = materials.add(StandardMaterial {
        base_color: Color::srgba(0.1, 0.9, 1.0, 0.7),
        alpha_mode: AlphaMode::Blend,
        ..default()
    });

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
            Name::new("rocket trail"),
            RocketMarker {
                timer: Timer::from_seconds(ROCKET_DEBUG_SECONDS, TimerMode::Once),
            },
            Mesh3d(meshes.add(Cuboid::new(0.045, 0.045, length))),
            MeshMaterial3d(material.clone()),
            ray_transform,
        ));
    }

    commands.spawn((
        Name::new("rocket explosion"),
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
