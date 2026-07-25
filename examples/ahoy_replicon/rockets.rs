//! Client rocket presentation, fire-and-forget: a visual spawns on a fire
//! edge, flies on its own clock, and dies on a hit edge — after birth it
//! never reads netcode state again.
//!
//! The edges come from two places. The local player's rockets drain from
//! [`NetAhoyPlayerEvents`], the same outbox the server broadcasts from —
//! predicted, so fire flash and explosion land the instant the shared step
//! says so, at the exact (possibly direct-hit-truncated) point. Replay pushes
//! duplicate events; deduping by [`RocketId`] absorbs them: skip a fire whose
//! visual already exists, skip a blast whose visual doesn't. Remote players'
//! rockets arrive as the replicated `RocketFired`/`RocketHit` events.
#![allow(dead_code)] // the server binary compiles this module without using it

use bevy::prelude::*;
use bevy_netahoy::*;

const EXPLOSION_MARKER_SECONDS: f32 = 0.85;
const DAMAGE_FLASH_SECONDS: f32 = 0.3;
/// Backstop despawn margin past the flight time, for rockets whose hit edge
/// never comes (revoked by a rewind, or a lost remote event).
const ROCKET_TIMEOUT_GRACE: f32 = 0.35;

pub fn add_client_rockets(app: &mut App) {
    app.add_observer(receive_remote_rocket_fired)
        .add_observer(receive_rocket_hit)
        .add_systems(
            Update,
            (
                consume_predicted_rocket_events,
                update_rocket_visuals,
                update_rocket_markers,
                update_damage_flashes,
            )
                .chain()
                .after(ClientNetAhoySystems::Interpolate),
        );
}

/// One in-flight rocket, self-animating: age maps to distance along the baked
/// path, timeout despawns it silently if no hit edge ever arrives.
#[derive(Component)]
struct RocketVisual {
    id: RocketId,
    start: Vec3,
    dir: Vec3,
    distance: f32,
    travel_seconds: f32,
    age: f32,
}

#[derive(Component)]
struct RocketMarker {
    timer: Timer,
}

/// A player visual taking damage: tint red, restore on expiry.
#[derive(Component)]
struct DamageFlash {
    timer: Timer,
    original: Color,
}

/// The local player's predicted fire/blast edges, drained from the outbox.
fn consume_predicted_rocket_events(
    mut events: ResMut<NetAhoyPlayerEvents>,
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    local_id: Res<LocalPlayerId>,
    visuals: Query<(Entity, &RocketVisual)>,
    mut flashables: FlashableVisuals,
) {
    // Fires first, then hits, so a point-blank rocket (fired and blasted in
    // the same tick) resolves as fire → immediate blast instead of a visual
    // that lingers to its timeout.
    let mut pending: Vec<RocketFired> = Vec::new();
    for fired in events.fired.drain(..) {
        let duplicate = pending.iter().any(|p| p.id == fired.id)
            || visuals.iter().any(|(_, v)| v.id == fired.id);
        if !duplicate {
            pending.push(fired);
        }
    }

    for hit in events.hit.drain(..) {
        // A blast for a rocket fired this very frame: cancel the spawn and
        // just explode. Otherwise kill the flying visual. Neither found =
        // replay duplicate, already presented — skip.
        if let Some(index) = pending.iter().position(|p| p.id == hit.id) {
            pending.remove(index);
        } else if let Some((entity, _)) = visuals.iter().find(|(_, v)| v.id == hit.id) {
            commands.entity(entity).despawn();
        } else {
            continue;
        }

        spawn_explosion_marker(
            &mut commands,
            &mut meshes,
            &mut materials,
            hit.point,
            Color::srgba(1.0, 0.55, 0.15, 0.8),
            "predicted rocket explosion",
        );
        flash_blast_victims(&hit, &local_id, &mut flashables, &mut materials, &mut commands);
    }

    for fired in pending {
        spawn_rocket_visual(
            &mut commands,
            &mut meshes,
            &mut materials,
            &fired,
            Color::srgba(0.1, 0.9, 1.0, 0.9),
        );
    }
}

/// Remote players' fire edges, from the replicated event. The local player's
/// rockets are skipped — they were already predicted from the outbox.
fn receive_remote_rocket_fired(
    fired: On<RocketFired>,
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    local_id: Res<LocalPlayerId>,
) {
    if local_id.is_assigned_to(fired.id.owner.0) {
        return;
    }
    spawn_rocket_visual(
        &mut commands,
        &mut meshes,
        &mut materials,
        &fired,
        Color::srgba(1.0, 0.45, 0.1, 0.9),
    );
}

/// The server-truth blast: a red marker (gold for a direct hit) for every
/// rocket, plus the despawn/flash for remote rockets the client never
/// simulated.
fn receive_rocket_hit(
    hit: On<RocketHit>,
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    local_id: Res<LocalPlayerId>,
    visuals: Query<(Entity, &RocketVisual)>,
    mut flashables: FlashableVisuals,
) {
    let (color, name) = if hit.direct_hit.is_some() {
        (Color::srgba(1.0, 0.85, 0.15, 0.9), "rocket direct hit")
    } else {
        (Color::srgba(1.0, 0.15, 0.08, 0.75), "rocket hit")
    };
    spawn_explosion_marker(&mut commands, &mut meshes, &mut materials, hit.point, color, name);

    // The local player's own blasts already despawned and flashed predictively.
    if local_id.is_assigned_to(hit.id.owner.0) {
        return;
    }
    for (entity, visual) in &visuals {
        if visual.id == hit.id {
            commands.entity(entity).despawn();
        }
    }
    flash_blast_victims(&hit, &local_id, &mut flashables, &mut materials, &mut commands);
}

fn spawn_rocket_visual(
    commands: &mut Commands,
    meshes: &mut Assets<Mesh>,
    materials: &mut Assets<StandardMaterial>,
    fired: &RocketFired,
    color: Color,
) {
    let up = if fired.dir.cross(Vec3::Y).length_squared() < 0.001 {
        Vec3::Z
    } else {
        Vec3::Y
    };
    let mut transform = Transform::from_translation(fired.start);
    transform.look_to(fired.dir, up);

    commands.spawn((
        Name::new("rocket"),
        RocketVisual {
            id: fired.id,
            start: fired.start,
            dir: fired.dir,
            distance: fired.hit_distance,
            travel_seconds: fired.hit_distance / ROCKET_SPEED,
            age: 0.0,
        },
        Mesh3d(meshes.add(Cuboid::new(0.12, 0.12, 0.45))),
        MeshMaterial3d(rocket_material(materials, color)),
        transform,
    ));
}

fn update_rocket_visuals(
    mut commands: Commands,
    time: Res<Time>,
    mut rockets: Query<(Entity, &mut RocketVisual, &mut Transform)>,
) {
    for (entity, mut rocket, mut transform) in &mut rockets {
        rocket.age += time.delta_secs();
        let frac = (rocket.age / rocket.travel_seconds.max(0.001)).clamp(0.0, 1.0);
        transform.translation = rocket.start + rocket.dir * (rocket.distance * frac);

        if rocket.age >= rocket.travel_seconds + ROCKET_TIMEOUT_GRACE {
            // No hit edge came: revoked or lost. Vanish without an explosion.
            commands.entity(entity).despawn();
        }
    }
}

type FlashableVisuals<'w, 's> = Query<
    'w,
    's,
    (
        Entity,
        &'static Transform,
        &'static MeshMaterial3d<StandardMaterial>,
        Option<&'static RemotePlayerVisual>,
        Option<&'static mut DamageFlash>,
    ),
    Or<(With<RemotePlayerVisual>, With<LocalPresentationPlayer>)>,
>;

/// Tint every player visual caught in a blast: the direct victim always, plus
/// anyone inside the splash radius. Real damage numbers are game code; this
/// is the presentation hook a health system would share.
fn flash_blast_victims(
    hit: &RocketHit,
    local_id: &LocalPlayerId,
    flashables: &mut FlashableVisuals,
    materials: &mut Assets<StandardMaterial>,
    commands: &mut Commands,
) {
    for (entity, transform, material, remote, flash) in flashables.iter_mut() {
        // A remote visual carries its player id; the presentation player is
        // the local player.
        let player_id = remote.map(|visual| visual.player_id.0).or(local_id.0);
        let direct = player_id.is_some() && hit.direct_hit.map(|id| id.0) == player_id;
        let splashed = transform.translation.distance(hit.point) < SPLASH_RADIUS;
        if !direct && !splashed {
            continue;
        }

        let Some(mut material) = materials.get_mut(&material.0) else {
            continue;
        };
        if let Some(mut flash) = flash {
            flash.timer.reset();
        } else {
            let original = material.base_color;
            material.base_color = Color::srgb(1.0, 0.1, 0.1);
            commands.entity(entity).insert(DamageFlash {
                timer: Timer::from_seconds(DAMAGE_FLASH_SECONDS, TimerMode::Once),
                original,
            });
        }
    }
}

fn update_damage_flashes(
    mut commands: Commands,
    time: Res<Time>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    mut flashes: Query<(Entity, &mut DamageFlash, &MeshMaterial3d<StandardMaterial>)>,
) {
    for (entity, mut flash, material) in &mut flashes {
        flash.timer.tick(time.delta());
        if flash.timer.is_finished() {
            if let Some(mut material) = materials.get_mut(&material.0) {
                material.base_color = flash.original;
            }
            commands.entity(entity).remove::<DamageFlash>();
        }
    }
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

fn spawn_explosion_marker(
    commands: &mut Commands,
    meshes: &mut Assets<Mesh>,
    materials: &mut Assets<StandardMaterial>,
    explosion: Vec3,
    color: Color,
    name: &'static str,
) {
    commands.spawn((
        Name::new(name),
        RocketMarker {
            timer: Timer::from_seconds(EXPLOSION_MARKER_SECONDS, TimerMode::Once),
        },
        Mesh3d(meshes.add(Cuboid::new(0.35, 0.35, 0.35))),
        MeshMaterial3d(rocket_material(materials, color)),
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
        let remaining = marker.timer.remaining_secs() / EXPLOSION_MARKER_SECONDS;
        transform.scale = Vec3::splat(remaining.max(0.15));
        if marker.timer.is_finished() {
            commands.entity(entity).despawn();
        }
    }
}
