//! Rocket visuals, fire-and-forget: one spawns on a fire, flies on its own clock and dies on
//! its blast, never reading netcode state in between. The local player's fires and blasts are
//! its predicted [`Shot`]s, each once however often prediction replays it; everyone else's, and
//! any of its own the prediction missed, are the server's [`RemoteEvent`]s.

use std::collections::VecDeque;

use bevy::prelude::*;
use bevy_netahoy::*;

use playground::{Fired, Hit, ROCKET_SPEED, SPLASH_RADIUS, Shot, Weapon};

use crate::demon::DemonFlinch;

const EXPLOSION_MARKER_SECONDS: f32 = 0.85;
/// Backstop despawn margin past the flight time, for rockets whose hit edge
/// never comes (revoked by a rewind, or a lost remote event).
const ROCKET_TIMEOUT_GRACE: f32 = 0.35;

/// Rocket visuals, blast markers, and the flinch of everyone caught in a blast.
pub fn add_client_rockets(app: &mut App) {
    app.init_resource::<PredictedRockets>()
        .add_observer(receive_remote_rocket_fired)
        .add_observer(receive_rocket_hit)
        .add_systems(
            Update,
            (
                consume_predicted_rocket_events,
                update_rocket_visuals,
                update_rocket_markers,
            )
                .chain()
                .after(NetAhoyClientSystems::Interpolate),
        );
}

/// The local player's newest fires and blasts its prediction made, by id. The server's copy of
/// one of them confirms it; the server's copy of an own event missing here is the only one.
#[derive(Resource, Default)]
struct PredictedRockets {
    fired: VecDeque<EventId>,
    hits: VecDeque<EventId>,
}

impl PredictedRockets {
    /// Twelve seconds of fires at the cooldown: past any fuse plus round trip.
    const KEPT: usize = 64;

    fn note(ids: &mut VecDeque<EventId>, id: EventId) {
        if ids.len() == Self::KEPT {
            ids.pop_front();
        }
        ids.push_back(id);
    }
}

/// A rocket in flight, placed by its age along the baked path.
#[derive(Component)]
struct RocketVisual {
    id: EventId,
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

/// The local player's predicted fire/blast edges.
fn consume_predicted_rocket_events(
    mut events: MessageReader<Shot>,
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    own_id: Res<OwnPlayerId>,
    mut predicted: ResMut<PredictedRockets>,
    visuals: Query<(Entity, &RocketVisual)>,
    mut flinches: Flinches,
) {
    // Visuals spawn after the frame's blasts, so a point-blank rocket (fired and blasted in one
    // frame) just blasts instead of flying on to its timeout.
    let mut pending: Vec<Fired> = Vec::new();
    for &event in events.read() {
        let hit = match event {
            Shot::Fired(fired) if fired.weapon == Weapon::Rockets => {
                PredictedRockets::note(&mut predicted.fired, fired.id);
                pending.push(fired);
                continue;
            }
            Shot::Hit(hit) if hit.weapon == Weapon::Rockets => hit,
            _ => continue,
        };
        PredictedRockets::note(&mut predicted.hits, hit.id);
        // The flying visual may be gone already, timed out: the blast still shows.
        if let Some(index) = pending.iter().position(|p| p.id == hit.id) {
            pending.remove(index);
        } else if let Some((entity, _)) = visuals.iter().find(|(_, v)| v.id == hit.id) {
            commands.entity(entity).despawn();
        }

        spawn_explosion_marker(
            &mut commands,
            &mut meshes,
            &mut materials,
            hit.point,
            Color::srgba(1.0, 0.55, 0.15, 0.8),
            "predicted rocket explosion",
        );
        flinch_blast_victims(&hit, &own_id, &mut flinches);
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

/// The server's fire edges: remote players', and the local player's that its prediction
/// missed (the predicted ones already fly).
fn receive_remote_rocket_fired(
    event: On<RemoteEvent<Shot>>,
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    own_id: Res<OwnPlayerId>,
    predicted: Res<PredictedRockets>,
) {
    let RemoteEvent(Shot::Fired(fired)) = *event else {
        return;
    };
    if fired.weapon != Weapon::Rockets
        || (own_id.is_assigned_to(fired.id.owner) && predicted.fired.contains(&fired.id))
    {
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
/// rocket, plus the despawn and the flinch for every blast the prediction didn't make.
fn receive_rocket_hit(
    event: On<RemoteEvent<Shot>>,
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    own_id: Res<OwnPlayerId>,
    predicted: Res<PredictedRockets>,
    visuals: Query<(Entity, &RocketVisual)>,
    mut flinches: Flinches,
) {
    let RemoteEvent(Shot::Hit(hit)) = *event else {
        return;
    };
    if hit.weapon != Weapon::Rockets {
        return;
    }
    let (color, name) = if hit.direct_hit.is_some() {
        (Color::srgba(1.0, 0.85, 0.15, 0.9), "rocket direct hit")
    } else {
        (Color::srgba(1.0, 0.15, 0.08, 0.75), "rocket hit")
    };
    spawn_explosion_marker(
        &mut commands,
        &mut meshes,
        &mut materials,
        hit.point,
        color,
        name,
    );

    // A blast the prediction made already despawned and flinched.
    if own_id.is_assigned_to(hit.id.owner) && predicted.hits.contains(&hit.id) {
        return;
    }
    for (entity, visual) in &visuals {
        if visual.id == hit.id {
            commands.entity(entity).despawn();
        }
    }
    flinch_blast_victims(&hit, &own_id, &mut flinches);
}

fn spawn_rocket_visual(
    commands: &mut Commands,
    meshes: &mut Assets<Mesh>,
    materials: &mut Assets<StandardMaterial>,
    fired: &Fired,
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
            commands.entity(entity).despawn();
        }
    }
}

type Flinches<'w, 's> = Query<
    'w,
    's,
    (
        &'static Transform,
        Option<&'static RemoteVisual>,
        &'static mut DemonFlinch,
    ),
    Or<(With<RemoteVisual>, With<OwnVisual>)>,
>;

/// Every player caught in a blast flinches: the direct victim, and anyone inside the splash
/// radius.
fn flinch_blast_victims(hit: &Hit, own_id: &OwnPlayerId, flinches: &mut Flinches) {
    for (transform, remote, mut flinch) in flinches.iter_mut() {
        // A remote visual carries its player id; the other is this player's own.
        let player_id = remote.map(|visual| visual.player_id).or(own_id.0);
        let direct = player_id.is_some() && hit.direct_hit == player_id;
        if direct || transform.translation.distance(hit.point) < SPLASH_RADIUS {
            flinch.start();
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
