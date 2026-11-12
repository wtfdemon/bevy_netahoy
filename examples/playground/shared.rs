//! What the example's client and server share besides the game and the level: the protocol at
//! the tick rate, and the player: its controller, collider and spawn.

use avian3d::prelude::*;
use bevy::{prelude::*, state::app::StatesPlugin};
use bevy_ahoy::prelude::*;
use bevy_netahoy::{
    AppHistoryExt, NetAhoyProtocolPlugin, PLAYER_COLLISION_LAYER, PlayerId, PlayerOwner,
    PlayerShape, PlayerState, TickRate, WORLD_COLLISION_LAYER,
    debug::{DebugTimeScale, apply_debug_time_scale},
};
use bevy_replicon::prelude::*;

use crate::{Arsenal, Platform, ShieldGate, level::SPAWN_POINT};

/// What the client and the server both add: replicon, the protocol at the tick rate, the
/// platform's and the gate's replication and the debug time scale.
pub struct ExampleSharedPlugin;

impl Plugin for ExampleSharedPlugin {
    fn build(&self, app: &mut App) {
        if !app.is_plugin_added::<StatesPlugin>() {
            app.add_plugins(StatesPlugin);
        }

        let tick_rate = tick_rate();
        info!("tick rate: {} Hz", tick_rate.hz());
        app.init_resource::<DebugTimeScale>()
            .add_plugins((
                RepliconPlugins,
                NetAhoyProtocolPlugin::<Arsenal>::new(tick_rate),
            ))
            .replicate::<Platform>()
            .replicate_with_history::<ShieldGate>()
            .add_systems(Startup, apply_debug_time_scale);
    }
}

/// `TICK_HZ=<n>` (`?tick_hz=<n>` in a browser): the tick rate. Server and clients must agree;
/// unset, netahoy's default.
fn tick_rate() -> TickRate {
    let Some(raw) = tick_hz_setting() else {
        return TickRate::default();
    };
    match raw.trim().parse::<u32>() {
        Ok(hz) if (TickRate::MIN_HZ..=TickRate::MAX_HZ).contains(&hz) => TickRate::new(hz),
        _ => panic!(
            "TICK_HZ={raw:?}: expected a whole number of Hz from {} to {}",
            TickRate::MIN_HZ,
            TickRate::MAX_HZ
        ),
    }
}

#[cfg(not(target_family = "wasm"))]
fn tick_hz_setting() -> Option<String> {
    std::env::var("TICK_HZ").ok()
}

#[cfg(target_family = "wasm")]
fn tick_hz_setting() -> Option<String> {
    let search = web_sys::window()?.location().search().ok()?;
    search
        .trim_start_matches('?')
        .split('&')
        .find_map(|pair| pair.strip_prefix("tick_hz="))
        .map(str::to_owned)
}

/// Every player's controller: the movement tuning.
pub fn player_controller() -> CharacterController {
    CharacterController {
        // World only: `Step::separate` pushes players apart where each saw the other.
        filter: SpatialQueryFilter::from_mask(WORLD_COLLISION_LAYER),
        acceleration_hz: 10.0,
        speed: 6.5,
        gravity: 23.0,
        friction_hz: 4.0,
        ..default()
    }
}

/// A player's mesh: the shape's cylinder.
pub fn player_mesh(shape: &PlayerShape) -> Cylinder {
    Cylinder::new(shape.radius, shape.half_height * 2.0)
}

/// Every player's collision layers.
pub fn player_collision_layers() -> CollisionLayers {
    CollisionLayers::new(PLAYER_COLLISION_LAYER, LayerMask::ALL)
}

/// Spawn the server-side player entity for `client`.
pub fn spawn_player(
    commands: &mut Commands,
    shape: &PlayerShape,
    client: Entity,
    player_id: u64,
    position: Vec3,
    velocity: Vec3,
) {
    commands.spawn((
        Name::new(format!("player {player_id}")),
        PlayerId(player_id),
        PlayerOwner(client),
        {
            let mut state = PlayerState::<Arsenal>::at(position);
            state.moves.velocity = velocity;
            state
        },
        player_controller(),
        shape.collider(),
        player_collision_layers(),
    ));
}

/// Where player `player_id` spawns: on the spawn pad, five abreast 1.4 m apart, the first in the
/// middle at [`SPAWN_POINT`].
pub fn player_spawn_point(player_id: u64) -> Vec3 {
    const ACROSS: [f32; 5] = [0.0, 1.4, -1.4, 2.8, -2.8];
    let slot = player_id.saturating_sub(1) % ACROSS.len() as u64;
    SPAWN_POINT + Vec3::X * ACROSS[slot as usize]
}
