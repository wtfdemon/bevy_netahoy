use bevy::prelude::*;
use serde::{Deserialize, Serialize};

// Replicated components and events live in this shared lib (not a per-binary
// `mod`) so their `type_name` — which replicon's protocol hash is built from —
// matches on client and server.

/// Marker: this entity is a vehicle. Replicated so clients attach visuals.
#[derive(Component, Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq)]
pub struct Vehicle;

/// Present on a vehicle while someone drives it; replicated so every client
/// knows who. The driver's client is the vehicle's physics authority.
#[derive(Component, Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq)]
pub struct Driver(pub u64);

/// Client → server: board the nearest free vehicle, or exit the current one.
#[derive(Event, Serialize, Deserialize, Clone, Copy, Debug)]
pub struct BoardVehicle;

/// Client → server: the driving client's simulated vehicle pose this tick.
/// Owner-authoritative — the server applies it verbatim and replication fans
/// it out to everyone else. (A sanity clamp would go where it's applied.)
#[derive(Event, Serialize, Deserialize, Clone, Copy, Debug)]
pub struct VehicleState {
    pub position: Vec3,
    pub rotation: Quat,
    pub linear_velocity: Vec3,
    pub angular_velocity: Vec3,
}

/// A rocket fired by a driver from their vehicle. Client → server on fire
/// (the owner already baked the path against its authoritative sim); server →
/// everyone as the relay for visuals and other owners' impulse response.
#[derive(Event, Serialize, Deserialize, Clone, Copy, Debug)]
pub struct VehicleRocketFired {
    pub firer: u64,
    pub start: Vec3,
    pub dir: Vec3,
    pub hit_distance: f32,
    pub fuse_seconds: f32,
}

/// Marker: this entity is a pickupable prop (a crate). Replicated so clients
/// attach colliders and visuals.
#[derive(Component, Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq)]
pub struct Prop;

/// Present on a prop while a player holds it; replicated so the holder's
/// client excludes it from KCC prediction and springs its visual to the
/// camera, and everyone else knows not to expect it to settle.
#[derive(Component, Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq)]
pub struct HeldBy(pub u64);

/// Client → server: drive the server-side avian_pickup actor. `Pull` is
/// held-semantics (sent every frame the grab key is down), `Throw`/`Drop` are
/// edges.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum PickupAction {
    Pull,
    Throw,
    Drop,
}

#[derive(Event, Serialize, Deserialize, Clone, Copy, Debug)]
pub struct PickupInput {
    pub action: PickupAction,
}

#[derive(Event, Serialize, Deserialize, Clone, Copy, Debug)]
pub struct HitScanShot {
    pub shot_id: u32,
    pub client_sample_tick: u64,
    pub client_sample_alpha: f32,
    pub origin: Vec3,
    pub direction: Vec3,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
pub struct HitScanHit {
    pub player_id: u64,
    pub position: Vec3,
    pub distance: f32,
}

#[derive(Event, Serialize, Deserialize, Clone, Copy, Debug)]
pub struct HitScanAck {
    pub shot_id: u32,
    pub server_tick: u64,
    pub client_sample_tick: u64,
    pub client_sample_alpha: f32,
    pub hit: Option<HitScanHit>,
}
