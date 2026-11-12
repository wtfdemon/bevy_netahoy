//! The third-person camera.

use bevy::prelude::*;
use bevy_netahoy::*;

use playground::{Arsenal, level::SPAWN_POINT};

use crate::ClientLook;

const CAMERA_DISTANCE: f32 = 5.2;
const CAMERA_HEIGHT: f32 = 0.85;
const CAMERA_SHOULDER_OFFSET: f32 = 1.25;
const CAMERA_AIM_RIGHT_OFFSET: f32 = 0.85;

type CameraRigFilter = (
    With<Camera3d>,
    Without<PredictedPlayer>,
    Without<OwnPlayer>,
    Without<OwnVisual>,
);

/// Over the shoulder of the local player (or, in a demo, the recorded one), along the look.
pub fn update_camera_from_local_presentation(
    look: Res<ClientLook>,
    playback: Option<Res<DemoPlayback>>,
    presentations: Query<&Transform, (With<OwnVisual>, Without<Camera3d>)>,
    server_players: Query<&OwnerSnapshot<Arsenal>, With<OwnPlayer>>,
    remote_visuals: Query<(&RemoteVisual, &Transform), Without<Camera3d>>,
    mut camera: Single<&mut Transform, CameraRigFilter>,
) {
    let target = presentations
        .single()
        .ok()
        .map(|transform| transform.translation + Vec3::Y * 0.6)
        .or_else(|| {
            server_players
                .single()
                .ok()
                .map(|snapshot| snapshot.position() + Vec3::Y * 0.6)
        })
        .or_else(|| {
            // Demo playback: chase the recorded player's interpolated visual.
            let recorded = playback.as_ref()?.recorded_player?;
            remote_visuals
                .iter()
                .find(|(visual, _)| visual.player_id == recorded)
                .map(|(_, transform)| transform.translation + Vec3::Y * 0.6)
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
