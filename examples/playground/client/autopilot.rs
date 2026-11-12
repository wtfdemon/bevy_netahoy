//! `AUTOPILOT=1` (`?autopilot` in a browser): the client plays by itself, over the wire, so
//! prediction, the server's input guesses, rollback, rockets and the pistol all run. Scripted
//! from the tick and the player id, so every run plays the same.

use std::f32::consts::PI;

use bevy::prelude::*;
use bevy_netahoy::*;

use playground::{
    Arsenal, EQUIP_PISTOL, EQUIP_ROCKETS,
    bots::{AD_STRAFER_ID, STRAFER_ID},
};

/// Seconds on each weapon before the autopilot switches to the other.
const WEAPON_SECONDS: f64 = 8.0;

/// Plays the local player by script when the autopilot is on.
pub struct AutopilotPlugin;

impl Plugin for AutopilotPlugin {
    fn build(&self, app: &mut App) {
        if !enabled() {
            return;
        }
        info!("autopilot: playing by script");
        app.add_systems(
            FixedPreUpdate,
            pilot
                .after(NetAhoyClientSystems::Reconcile)
                .before(NetAhoyClientSystems::Predict),
        );
    }
}

/// `AUTOPILOT=1` or `AUTOPILOT=true`.
#[cfg(not(target_family = "wasm"))]
fn enabled() -> bool {
    std::env::var("AUTOPILOT").is_ok_and(|value| value == "1" || value == "true")
}

/// `?autopilot`, `?autopilot=1` or `?autopilot=true` among the page's query parameters.
#[cfg(target_family = "wasm")]
fn enabled() -> bool {
    let Some(search) = web_sys::window().and_then(|window| window.location().search().ok()) else {
        return false;
    };
    search
        .trim_start_matches('?')
        .split('&')
        .any(|pair| matches!(pair, "autopilot" | "autopilot=1" | "autopilot=true"))
}

/// Writes this tick's input over the keyboard's. Runs in wide circles around the spawn, the
/// players half a turn apart so they keep meeting; bunnyhops for a stretch, crouches, jumps now
/// and then. Weapons take turns of [`WEAPON_SECONDS`]: a rocket every two seconds, then a
/// pistol shot every half second at the nearer bot, aimed where this command will see it.
fn pilot(
    mut tick: Local<u32>,
    own: Res<OwnPlayerId>,
    tick_rate: Res<TickRate>,
    shape: Res<PlayerShape>,
    presented: Res<PresentedFrame>,
    predicted: Query<&PlayerState<Arsenal>, With<PredictedPlayer>>,
    players: Query<(&PlayerId, &History<PlayerSnapshot>)>,
    mut input: ResMut<ClientInput>,
) {
    let Ok(player) = predicted.single() else {
        return;
    };
    *tick += 1;
    let tick = *tick;
    let id = own.0.map_or(0, |id| id.0);
    let t = tick as f32 / tick_rate.hz() as f32;
    let ticks = |seconds| tick_rate.ticks(seconds) as u32;

    let circling = Vec2::new(0.6 * t + id as f32 * PI, -0.15);
    let pistol = tick % ticks(2.0 * WEAPON_SECONDS) >= ticks(WEAPON_SECONDS);
    let eye = player.moves.position + Vec3::Y * shape.eye_height;
    // A fire set in the tick that sends it is seen at what the last frame drew.
    let aim = presented
        .remote
        .filter(|_| pistol)
        .and_then(|seen| nearer_bot(&PlayerPoses::new(players.iter(), *tick_rate), seen, eye))
        .map(|bot| look_at(eye, bot));
    let look = aim.unwrap_or(circling);
    input.look = look;
    // Turned against the look, so the walk keeps its circle whatever it aims at.
    input.movement = Vec2::from_angle(circling.x - look.x).rotate(Vec2::new(0.35, 1.0));
    input.tap_movement = input.movement;

    let phase = tick % ticks(10.0);
    let hopping = phase < ticks(4.0);
    let mut buttons = if pistol { EQUIP_PISTOL } else { EQUIP_ROCKETS };
    buttons.set(Buttons::JUMP, hopping);
    buttons.set(Buttons::CROUCH, (ticks(8.0)..ticks(9.0)).contains(&phase));
    let mut pressed = Buttons::empty();
    pressed.set(Buttons::JUMP, !hopping && tick.is_multiple_of(ticks(1.5)));
    let fire = match aim {
        Some(_) => tick.is_multiple_of(ticks(0.5)),
        None => !pistol && tick % ticks(2.0) == ticks(1.0),
    };
    if fire {
        input.fire = Some(SubtickFire {
            frac: 0.0,
            look,
            origin: None,
        });
    }
    buttons |= pressed;
    input.buttons = buttons;
    // OR, not assign: a re-press the last command held back waits in `pressed` for this one.
    input.pressed |= pressed;
}

/// The centre of whichever bot is nearer `eye` at `seen`.
fn nearer_bot(poses: &PlayerPoses, seen: RemoteRenderTime, eye: Vec3) -> Option<Vec3> {
    [STRAFER_ID, AD_STRAFER_ID]
        .into_iter()
        .filter_map(|bot| poses.pose_at_time(bot, seen))
        .map(|pose| pose.position)
        .min_by(|a, b| a.distance_squared(eye).total_cmp(&b.distance_squared(eye)))
}

/// The look (yaw, pitch) from `eye` at `at`.
fn look_at(eye: Vec3, at: Vec3) -> Vec2 {
    let d = at - eye;
    Vec2::new((-d.x).atan2(-d.z), d.y.atan2(d.xz().length()))
}
