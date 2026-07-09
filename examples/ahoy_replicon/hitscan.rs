//! Lag-compensated hitscan weapon example.
#![allow(dead_code)]

use bevy::{
    prelude::*,
    window::{CursorGrabMode, CursorOptions},
};
use bevy_netahoy::*;
use bevy_replicon::prelude::*;

use ahoy_replicon::{HitScanAck, HitScanHit, HitScanShot};

use super::shared::FLYING_TARGET_PLAYER_ID;

const HITSCAN_MAX_DISTANCE: f32 = 80.0;
const HIT_MARKER_SECONDS: f32 = 0.75;
const AUTO_FIRE_INTERVAL_SECONDS: f32 = 0.10;
const PREDICTED_HIT_ACK_TIMEOUT_SECONDS: f32 = 1.0;

#[derive(SystemSet, Debug, Clone, Copy, Hash, PartialEq, Eq)]
pub enum ExampleHitscanClientSystems {
    Fire,
}

pub fn add_client_hitscan(app: &mut App) {
    app.init_resource::<ClientShotState>()
        .init_resource::<ShotFeedback>()
        .add_observer(receive_shot_ack)
        .add_systems(Startup, setup_shot_hud)
        .add_systems(
            Update,
            (
                update_hit_markers,
                automatic_fire.in_set(ExampleHitscanClientSystems::Fire),
                warn_unacked_predicted_hits,
                update_shot_text,
            )
                .chain()
                .after(ClientNetAhoySystems::Interpolate),
        );
}

pub fn add_server_hitscan(app: &mut App) {
    app.add_observer(process_shot);
}

#[derive(Resource, Default)]
struct ClientShotState {
    next_shot_id: u32,
    seconds_until_next_shot: f32,
    pending_predicted_hits: Vec<PendingPredictedHit>,
}

struct PendingPredictedHit {
    shot_id: u32,
    seconds_without_ack: f32,
}

#[derive(Resource, Default)]
struct ShotFeedback {
    predicted: String,
    acknowledged: String,
}

#[derive(Component)]
struct ShotText;

#[derive(Component)]
struct HitMarker {
    timer: Timer,
}

type HitscanCameraFilter = (
    With<Camera3d>,
    Without<ClientPredictionKcc>,
    Without<ServerTruthGhost>,
    Without<LocalPresentationPlayer>,
);

fn setup_shot_hud(mut commands: Commands) {
    commands.spawn((
        Node {
            position_type: PositionType::Absolute,
            top: px(64.0),
            left: px(16.0),
            ..default()
        },
        Text::new("shots: predicted none | server none"),
        TextColor(Color::WHITE.with_alpha(0.55)),
        ShotText,
    ));
}

fn process_shot(
    shot: On<FromClient<HitScanShot>>,
    mut commands: Commands,
    tick: Res<ServerTick>,
    history: Res<LagCompensationHistory>,
    players: Query<(&PlayerOwner, &PlayerId)>,
) {
    let Some(client) = shot.client_id.entity() else {
        return;
    };
    let Some((_, shooter_id)) = players.iter().find(|(owner, _)| owner.0 == client) else {
        return;
    };

    let min_rewind_tick = tick.0.saturating_sub(history.max_frames as u64);
    let sample_time = RemoteRenderTime::new(shot.client_sample_tick, shot.client_sample_alpha)
        .clamp_ticks(min_rewind_tick, tick.0);

    let hit = history
        .raycast_hitboxes_at_time(LagCompensatedCast {
            server_time: sample_time,
            origin: shot.origin,
            direction: shot.direction,
            max_distance: HITSCAN_MAX_DISTANCE,
            radius: PLAYER_CAPSULE_RADIUS,
            half_height: PLAYER_CAPSULE_HALF_HEIGHT,
            ignored_player: Some(*shooter_id),
        })
        .map(|hit| HitScanHit {
            player_id: hit.player_id.0,
            position: hit.position,
            distance: hit.distance,
        });

    commands.server_trigger(ToClients {
        mode: SendMode::Direct(ClientId::Client(client)),
        message: HitScanAck {
            shot_id: shot.shot_id,
            server_tick: tick.0,
            client_sample_tick: sample_time.tick,
            client_sample_alpha: sample_time.alpha,
            hit,
        },
    });
}

fn automatic_fire(
    mut commands: Commands,
    cursor: Single<&CursorOptions>,
    mouse_buttons: Res<ButtonInput<MouseButton>>,
    time: Res<Time>,
    clock: Res<ClientServerClock>,
    local: Res<LocalPlayerId>,
    mut shot_state: ResMut<ClientShotState>,
    mut feedback: ResMut<ShotFeedback>,
    camera: Single<&Transform, HitscanCameraFilter>,
    targets: Query<(&RemotePlayerVisual, &Transform)>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    if !mouse_buttons.pressed(MouseButton::Left) {
        shot_state.seconds_until_next_shot = 0.0;
        return;
    }
    if cursor.grab_mode != CursorGrabMode::Locked {
        return;
    }

    shot_state.seconds_until_next_shot -= time.delta_secs();
    if shot_state.seconds_until_next_shot > 0.0 {
        return;
    }
    shot_state.seconds_until_next_shot = AUTO_FIRE_INTERVAL_SECONDS;

    let shot_id = shot_state.next_shot_id.wrapping_add(1);
    shot_state.next_shot_id = shot_id;

    let origin = camera.translation;
    let direction = camera.rotation * Vec3::NEG_Z;
    let predicted_hit = predicted_hit_scan(origin, direction, local.0, &targets);
    let sample_time = clock.target_time().unwrap_or_default();

    feedback.predicted = predicted_hit
        .map(|hit| format_hit("client", hit))
        .unwrap_or_else(|| format!("client #{shot_id}: miss"));

    if let Some(hit) = predicted_hit {
        shot_state.pending_predicted_hits.push(PendingPredictedHit {
            shot_id,
            seconds_without_ack: 0.0,
        });

        spawn_hit_marker(
            &mut commands,
            &mut meshes,
            &mut materials,
            hit.position,
            Color::srgb(0.2, 1.0, 0.35),
            "client predicted hit",
        );
    }

    commands.client_trigger(HitScanShot {
        shot_id,
        client_sample_tick: sample_time.tick,
        client_sample_alpha: sample_time.alpha,
        origin,
        direction,
    });
}

fn predicted_hit_scan(
    origin: Vec3,
    direction: Vec3,
    local_player_id: Option<u64>,
    targets: &Query<(&RemotePlayerVisual, &Transform)>,
) -> Option<HitScanHit> {
    targets
        .iter()
        .filter(|(visual, _)| Some(visual.player_id.0) != local_player_id)
        .filter_map(|(visual, transform)| {
            let distance = ray_hitbox_distance(
                origin,
                direction,
                HITSCAN_MAX_DISTANCE,
                transform.translation,
                PLAYER_CAPSULE_RADIUS,
                PLAYER_CAPSULE_HALF_HEIGHT,
            )?;
            Some(HitScanHit {
                player_id: visual.player_id.0,
                position: origin + direction.normalize_or_zero() * distance,
                distance,
            })
        })
        .min_by(|a, b| a.distance.total_cmp(&b.distance))
}

fn receive_shot_ack(
    ack: On<HitScanAck>,
    mut commands: Commands,
    mut shot_state: ResMut<ClientShotState>,
    mut feedback: ResMut<ShotFeedback>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    shot_state
        .pending_predicted_hits
        .retain(|pending| pending.shot_id != ack.shot_id);

    feedback.acknowledged = ack
        .hit
        .map(|hit| format_hit("server", hit))
        .unwrap_or_else(|| format!("server #{}: miss", ack.shot_id));

    if let Some(hit) = ack.hit {
        spawn_hit_marker(
            &mut commands,
            &mut meshes,
            &mut materials,
            hit.position,
            Color::srgb(1.0, 0.12, 0.1),
            "server ack hit",
        );
    }
}

fn warn_unacked_predicted_hits(
    time: Res<Time>,
    mut shot_state: ResMut<ClientShotState>,
    mut feedback: ResMut<ShotFeedback>,
) {
    let dt = time.delta_secs();

    shot_state.pending_predicted_hits.retain_mut(|pending| {
        pending.seconds_without_ack += dt;
        if pending.seconds_without_ack < PREDICTED_HIT_ACK_TIMEOUT_SECONDS {
            return true;
        }

        feedback.acknowledged = format!("server #{}: no ack after 1000ms", pending.shot_id);
        warn!("{}", feedback.acknowledged);
        false
    });
}

fn spawn_hit_marker(
    commands: &mut Commands,
    meshes: &mut Assets<Mesh>,
    materials: &mut Assets<StandardMaterial>,
    position: Vec3,
    color: Color,
    name: &'static str,
) {
    commands.spawn((
        Name::new(name),
        HitMarker {
            timer: Timer::from_seconds(HIT_MARKER_SECONDS, TimerMode::Once),
        },
        Mesh3d(meshes.add(Cuboid::new(0.18, 0.18, 0.18))),
        MeshMaterial3d(materials.add(StandardMaterial {
            base_color: color,
            ..default()
        })),
        Transform::from_translation(position),
    ));
}

fn format_hit(source: &str, hit: HitScanHit) -> String {
    let target = if hit.player_id == FLYING_TARGET_PLAYER_ID {
        "target".to_string()
    } else {
        format!("player {}", hit.player_id)
    };
    format!("{source}: {target} {:.1}m", hit.distance)
}

fn update_hit_markers(
    mut commands: Commands,
    time: Res<Time>,
    mut markers: Query<(Entity, &mut HitMarker, &mut Transform)>,
) {
    for (entity, mut marker, mut transform) in &mut markers {
        marker.timer.tick(time.delta());
        let remaining = marker.timer.remaining_secs() / HIT_MARKER_SECONDS;
        transform.scale = Vec3::splat(remaining.max(0.15));
        if marker.timer.is_finished() {
            commands.entity(entity).despawn();
        }
    }
}

fn update_shot_text(feedback: Res<ShotFeedback>, mut text: Single<&mut Text, With<ShotText>>) {
    let predicted = if feedback.predicted.is_empty() {
        "client none"
    } else {
        feedback.predicted.as_str()
    };
    let acknowledged = if feedback.acknowledged.is_empty() {
        "server none"
    } else {
        feedback.acknowledged.as_str()
    };

    text.0 = format!("shots: {predicted} | {acknowledged}");
}
