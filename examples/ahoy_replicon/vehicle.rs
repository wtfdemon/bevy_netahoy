//! Owner-authoritative buggy: the driving client simulates the vehicle as a
//! real Avian dynamic body against its local world colliders and streams the
//! resulting pose to the server ([`VehicleState`]), which applies it verbatim
//! and lets ordinary replication fan it out. Zero input latency for the
//! driver, real collisions and bounces, and the netcode library never learns
//! vehicles exist.
//!
//! Boarding despawns the player's server entity — while driving there is no
//! capsule to predict, reconcile, or hide, and the client's prediction loop
//! dismantles itself through the existing player-despawn cleanup. Exiting
//! respawns the player above the buggy with its momentum.
//!
//! The suspension/friction math is a trimmed port of Rapier's
//! `DynamicRayCastVehicleController` (itself Bullet's `btRaycastVehicle`),
//! applied to the local sim body as velocity impulses before Avian integrates.
//!
//! The honest trade: the driver's client is trusted with the vehicle pose
//! (Roblox-style). A speed/teleport clamp belongs in `apply_vehicle_state`
//! if that ever matters.

// This mod is compiled into both example binaries; the server half is "dead"
// in the client build and vice versa.
#![allow(dead_code)]

use std::f32::consts::FRAC_PI_2;

use aeronet::io::connection::Disconnected;
use avian3d::prelude::*;
use bevy::prelude::*;
use bevy_netahoy::{
    rocket_impulse, ClientInput, LocalPlayerId, PlayerId, PlayerOwner, Rocket, RocketHit,
    BLAST_REFERENCE_MASS, FIXED_TIMESTEP_HZ, ROCKET_FIRE, WORLD_COLLISION_LAYER,
};
use bevy_replicon::prelude::*;

use crate::shared::spawn_player;

pub use ahoy_replicon::{BoardVehicle, Driver, Vehicle, VehicleRocketFired, VehicleState};

// --- Layers -------------------------------------------------------------

/// Wheel rays must not hit the casting chassis itself, so vehicles get their
/// own bit — but the chassis is also a member of the world layer, so player
/// capsules treat it as climbable geometry and wheel rays can drive onto
/// other buggies.
pub const VEHICLE_COLLISION_LAYER: LayerMask = LayerMask(1 << 2);

// --- Tuning (20 Hz fixed step, Avian gravity 9.81) ------------------------

const CHASSIS_HALF: Vec3 = Vec3::new(1.0, 0.35, 0.7);
const CHASSIS_MASS: f32 = 100.0;
const UP_LOCAL: Vec3 = Vec3::Y;

const SUSPENSION_REST: f32 = 0.45;
const WHEEL_RADIUS: f32 = 0.3;
const WHEEL_WIDTH: f32 = 0.2;
/// Wheel mounting points on the chassis, in chassis-local space. Front = +X.
/// Mounted at chassis-center height (y=0) and wide (z=±0.65) to keep the CG low
/// and the track wide — a raycast buggy rolls/wheelies easily otherwise.
const WHEEL_CONNECTIONS: [Vec3; 4] = [
    Vec3::new(0.8, 0.0, 0.65),
    Vec3::new(0.8, 0.0, -0.65),
    Vec3::new(-0.8, 0.0, 0.65),
    Vec3::new(-0.8, 0.0, -0.65),
];

// Drive force per wheel. At mass 100 / dt 0.05, ~180 gives ~0.7 g total thrust,
// under the wheelie threshold. 500 gave 2 g and backflipped at full throttle.
const ENGINE_FORCE: f32 = 180.0;
const MAX_STEER: f32 = 0.5;
const BRAKE_IMPULSE: f32 = 60.0;

/// How close you must be (to the chassis center) to board.
const BOARD_RADIUS: f32 = 3.0;
/// Where the player reappears on exit, chassis-local-ish (straight up so a
/// rolled buggy still ejects you clear of the ground).
const EXIT_OFFSET: Vec3 = Vec3::new(0.0, 1.6, 0.0);
/// Below this the world has ended; put the thing back.
const VOID_Y: f32 = -12.0;

/// Damping rate for non-driven vehicle visuals chasing the replicated pose.
const VISUAL_FOLLOW_RATE: f32 = 14.0;

/// Chassis collider, extended below the visual box so a parked (unsimulated)
/// buggy rests near wheel height instead of sinking to its floor pan. The
/// bottom stays 0.1 m above the wheels' rest plane so the driving sim doesn't
/// scrape the ground at rest — bottoming out under compression is allowed
/// (and fun).
fn chassis_collider() -> Collider {
    let bottom_drop = SUSPENSION_REST + WHEEL_RADIUS - 0.1 - CHASSIS_HALF.y;
    Collider::compound(vec![(
        Vec3::new(0.0, -bottom_drop / 2.0, 0.0),
        Quat::IDENTITY,
        Collider::cuboid(
            CHASSIS_HALF.x * 2.0,
            CHASSIS_HALF.y * 2.0 + bottom_drop,
            CHASSIS_HALF.z * 2.0,
        ),
    )])
}

fn vehicle_collision_layers() -> CollisionLayers {
    CollisionLayers::new(
        VEHICLE_COLLISION_LAYER | WORLD_COLLISION_LAYER,
        WORLD_COLLISION_LAYER | VEHICLE_COLLISION_LAYER,
    )
}

// --- Rigid-body proxy the ported controller operates on -------------------

/// The subset of chassis state the vehicle math reads and writes. Impulses hit
/// `linvel`/`angvel` immediately (matching Rapier's semantics), then the caller
/// writes them back to the Avian body for the solver to integrate.
struct Chassis {
    /// World center of mass. The collider is centered, so this is also `Position`.
    position: Vec3,
    rotation: Quat,
    linvel: Vec3,
    angvel: Vec3,
    inv_mass: f32,
    mass: f32,
    inv_inertia_world: Mat3,
}

impl Chassis {
    /// Mass and inertia come from Avian's computed properties for the body, so
    /// changing `Mass` or the collider can't silently desync this proxy.
    fn new(
        position: Vec3,
        rotation: Quat,
        linvel: Vec3,
        angvel: Vec3,
        mass: &ComputedMass,
        inertia: &ComputedAngularInertia,
    ) -> Self {
        Self {
            position,
            rotation,
            linvel,
            angvel,
            inv_mass: mass.inverse(),
            mass: mass.value(),
            // Local tensor rotated into world: I⁻¹_world = R I⁻¹ Rᵀ.
            inv_inertia_world: inertia.rotated(rotation).inverse().to_mat3(),
        }
    }

    fn velocity_at_point(&self, point: Vec3) -> Vec3 {
        self.linvel + self.angvel.cross(point - self.position)
    }

    fn apply_impulse_at_point(&mut self, impulse: Vec3, point: Vec3) {
        self.linvel += impulse * self.inv_mass;
        self.angvel += self.inv_inertia_world * (point - self.position).cross(impulse);
    }
}

fn inv(x: f32) -> f32 {
    if x != 0.0 { 1.0 / x } else { 0.0 }
}

// --- The ported controller -------------------------------------------------

#[derive(Clone, Copy, Debug)]
struct WheelTuning {
    suspension_stiffness: f32,
    damping_compression: f32,
    damping_relaxation: f32,
    max_suspension_travel: f32,
    side_friction_stiffness: f32,
    friction_slip: f32,
    max_suspension_force: f32,
}

impl Default for WheelTuning {
    fn default() -> Self {
        Self {
            suspension_stiffness: 25.0,
            damping_compression: 4.0,
            damping_relaxation: 4.5,
            max_suspension_travel: 0.5,
            side_friction_stiffness: 1.0,
            // Eased off so a hard corner slides before it rolls.
            friction_slip: 3.5,
            max_suspension_force: 20_000.0,
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct RayCastInfo {
    contact_normal_ws: Vec3,
    contact_point_ws: Vec3,
    suspension_length: f32,
    hard_point_ws: Vec3,
    is_in_contact: bool,
}

#[derive(Clone, Copy, Debug)]
struct Wheel {
    raycast_info: RayCastInfo,
    wheel_direction_ws: Vec3,
    wheel_axle_ws: Vec3,

    chassis_connection_point_cs: Vec3,
    direction_cs: Vec3,
    axle_cs: Vec3,
    suspension_rest_length: f32,
    max_suspension_travel: f32,
    radius: f32,
    suspension_stiffness: f32,
    damping_compression: f32,
    damping_relaxation: f32,
    friction_slip: f32,
    side_friction_stiffness: f32,
    roll_influence: f32,
    max_suspension_force: f32,

    forward_impulse: f32,
    side_impulse: f32,

    steering: f32,
    engine_force: f32,
    brake: f32,

    clipped_inv_contact_dot_suspension: f32,
    suspension_relative_velocity: f32,
    wheel_suspension_force: f32,
    skid_info: f32,
}

/// Per-vehicle controller state. Lives on the driving client's sim entity.
#[derive(Component)]
pub struct VehicleController {
    wheels: [Wheel; 4],
    forward_ws: [Vec3; 4],
    axle: [Vec3; 4],
}

impl VehicleController {
    /// A 4-wheel AWD buggy: front wheels (indices 0,1) steer, all four drive.
    fn new_buggy() -> Self {
        let tuning = WheelTuning::default();
        let wheels = WHEEL_CONNECTIONS.map(|connection| Wheel {
            raycast_info: RayCastInfo::default(),
            wheel_direction_ws: Vec3::NEG_Y,
            wheel_axle_ws: Vec3::Z,
            chassis_connection_point_cs: connection,
            direction_cs: Vec3::NEG_Y,
            axle_cs: Vec3::Z,
            suspension_rest_length: SUSPENSION_REST,
            max_suspension_travel: tuning.max_suspension_travel,
            radius: WHEEL_RADIUS,
            suspension_stiffness: tuning.suspension_stiffness,
            damping_compression: tuning.damping_compression,
            damping_relaxation: tuning.damping_relaxation,
            friction_slip: tuning.friction_slip,
            side_friction_stiffness: tuning.side_friction_stiffness,
            roll_influence: 0.1,
            max_suspension_force: tuning.max_suspension_force,
            forward_impulse: 0.0,
            side_impulse: 0.0,
            steering: 0.0,
            engine_force: 0.0,
            brake: 0.0,
            clipped_inv_contact_dot_suspension: 0.0,
            suspension_relative_velocity: 0.0,
            wheel_suspension_force: 0.0,
            skid_info: 0.0,
        });
        Self {
            wheels,
            forward_ws: [Vec3::ZERO; 4],
            axle: [Vec3::ZERO; 4],
        }
    }

    /// Set this tick's inputs. Front wheels steer; all wheels drive/brake.
    fn set_inputs(&mut self, throttle: f32, steer: f32, brake: f32) {
        for wheel in &mut self.wheels {
            wheel.engine_force = throttle * ENGINE_FORCE;
            wheel.brake = brake;
            wheel.steering = if wheel.chassis_connection_point_cs.x > 0.0 {
                steer * MAX_STEER
            } else {
                0.0
            };
        }
    }

    fn update_wheel_transform(&mut self, chassis: &Chassis, i: usize) {
        let wheel = &mut self.wheels[i];
        wheel.raycast_info.is_in_contact = false;
        wheel.raycast_info.hard_point_ws =
            chassis.position + chassis.rotation * wheel.chassis_connection_point_cs;
        wheel.wheel_direction_ws = chassis.rotation * wheel.direction_cs;
        // Steering rotates the axle around the (world) suspension direction.
        let steering_orn = Quat::from_scaled_axis(-wheel.wheel_direction_ws * wheel.steering);
        wheel.wheel_axle_ws = steering_orn * (chassis.rotation * wheel.axle_cs);
    }

    fn ray_cast(&mut self, spatial: &SpatialQuery, body: Entity, chassis: &Chassis, i: usize) {
        let wheel = &mut self.wheels[i];
        let raylen = wheel.suspension_rest_length + wheel.radius;
        let source = wheel.raycast_info.hard_point_ws;
        let dir = Dir3::new(wheel.wheel_direction_ws).unwrap_or(Dir3::NEG_Y);
        wheel.raycast_info.contact_point_ws = source + wheel.wheel_direction_ws * raylen;

        // World and other vehicles are drivable ground; the casting body is not.
        let filter =
            SpatialQueryFilter::from_mask(WORLD_COLLISION_LAYER | VEHICLE_COLLISION_LAYER)
                .with_excluded_entities([body]);
        if let Some(hit) = spatial.cast_ray(source, dir, raylen, true, &filter) {
            let mut normal = hit.normal;
            if hit.distance <= 0.0 || normal == Vec3::ZERO {
                normal = -wheel.wheel_direction_ws;
            }
            wheel.raycast_info.contact_normal_ws = normal;
            wheel.raycast_info.is_in_contact = true;

            wheel.raycast_info.suspension_length = (hit.distance - wheel.radius).clamp(
                wheel.suspension_rest_length - wheel.max_suspension_travel,
                wheel.suspension_rest_length + wheel.max_suspension_travel,
            );
            wheel.raycast_info.contact_point_ws = source + *dir * hit.distance;

            let denominator = wheel.raycast_info.contact_normal_ws.dot(wheel.wheel_direction_ws);
            let proj_vel = wheel
                .raycast_info
                .contact_normal_ws
                .dot(chassis.velocity_at_point(wheel.raycast_info.contact_point_ws));
            if denominator >= -0.1 {
                wheel.suspension_relative_velocity = 0.0;
                wheel.clipped_inv_contact_dot_suspension = 1.0 / 0.1;
            } else {
                let inv = -1.0 / denominator;
                wheel.suspension_relative_velocity = proj_vel * inv;
                wheel.clipped_inv_contact_dot_suspension = inv;
            }
        } else {
            wheel.raycast_info.suspension_length = wheel.suspension_rest_length;
            wheel.suspension_relative_velocity = 0.0;
            wheel.raycast_info.contact_normal_ws = -wheel.wheel_direction_ws;
            wheel.clipped_inv_contact_dot_suspension = 1.0;
        }
    }

    fn update_suspension(&mut self, chassis_mass: f32) {
        for wheel in &mut self.wheels {
            if wheel.raycast_info.is_in_contact {
                let length_diff = wheel.suspension_rest_length - wheel.raycast_info.suspension_length;
                let mut force =
                    wheel.suspension_stiffness * length_diff * wheel.clipped_inv_contact_dot_suspension;

                let projected_rel_vel = wheel.suspension_relative_velocity;
                let susp_damping = if projected_rel_vel < 0.0 {
                    wheel.damping_compression
                } else {
                    wheel.damping_relaxation
                };
                force -= susp_damping * projected_rel_vel;

                wheel.wheel_suspension_force = (force * chassis_mass).max(0.0);
            } else {
                wheel.wheel_suspension_force = 0.0;
            }
        }
    }

    fn update_friction(&mut self, chassis: &mut Chassis, dt: f32) {
        let n = self.wheels.len();
        let mut num_on_ground = 0;
        for wheel in &mut self.wheels {
            if wheel.raycast_info.is_in_contact {
                num_on_ground += 1;
            }
            wheel.side_impulse = 0.0;
            wheel.forward_impulse = 0.0;
        }

        // Lateral (side) friction: keep the tire from sliding along its axle.
        for i in 0..n {
            let wheel = self.wheels[i];
            if !wheel.raycast_info.is_in_contact {
                continue;
            }
            let surf = wheel.raycast_info.contact_normal_ws;
            let mut axle = wheel.wheel_axle_ws;
            axle -= surf * axle.dot(surf);
            axle = axle.normalize_or_zero();
            self.axle[i] = axle;
            self.forward_ws[i] = surf.cross(axle).normalize_or_zero();
            let side = resolve_single_unilateral(chassis, wheel.raycast_info.contact_point_ws, axle);
            self.wheels[i].side_impulse = side * wheel.side_friction_stiffness;
        }

        // Forward (drive / brake / rolling) friction.
        let side_factor = 1.0;
        let fwd_factor = 0.5;
        let mut sliding = false;
        for i in 0..n {
            let wheel = self.wheels[i];
            let grounded = wheel.raycast_info.is_in_contact;
            let mut rolling_friction = 0.0;
            if grounded {
                if wheel.engine_force != 0.0 {
                    rolling_friction = wheel.engine_force * dt;
                } else {
                    let max_impulse = wheel.brake;
                    rolling_friction = calc_rolling_friction(
                        chassis,
                        wheel.raycast_info.contact_point_ws,
                        self.forward_ws[i],
                        max_impulse,
                        num_on_ground.max(1),
                    );
                }
            }

            self.wheels[i].forward_impulse = 0.0;
            self.wheels[i].skid_info = 1.0;
            if grounded {
                let max_imp = wheel.wheel_suspension_force * dt * wheel.friction_slip;
                let x = rolling_friction * fwd_factor;
                let y = wheel.side_impulse * side_factor;
                self.wheels[i].forward_impulse = rolling_friction;
                if x * x + y * y > max_imp * max_imp {
                    sliding = true;
                    self.wheels[i].skid_info = max_imp * inv((x * x + y * y).sqrt());
                }
            }
        }

        if sliding {
            for wheel in &mut self.wheels {
                if wheel.side_impulse != 0.0 && wheel.skid_info < 1.0 {
                    wheel.forward_impulse *= wheel.skid_info;
                    wheel.side_impulse *= wheel.skid_info;
                }
            }
        }

        // Apply the accumulated wheel impulses to the chassis.
        for i in 0..n {
            let wheel = self.wheels[i];
            if wheel.forward_impulse != 0.0 {
                chassis.apply_impulse_at_point(
                    self.forward_ws[i] * wheel.forward_impulse,
                    wheel.raycast_info.contact_point_ws,
                );
            }
            if wheel.side_impulse != 0.0 {
                let side_impulse = self.axle[i] * wheel.side_impulse;
                let up = chassis.rotation * UP_LOCAL;
                let mut point = wheel.raycast_info.contact_point_ws;
                point -= up * (up.dot(point - chassis.position) * (1.0 - wheel.roll_influence));
                chassis.apply_impulse_at_point(side_impulse, point);
            }
        }
    }

    /// One vehicle step: suspension and friction, applied to `chassis`.
    fn update_vehicle(&mut self, dt: f32, body: Entity, chassis: &mut Chassis, spatial: &SpatialQuery) {
        let n = self.wheels.len();
        for i in 0..n {
            self.update_wheel_transform(chassis, i);
        }

        for i in 0..n {
            self.ray_cast(spatial, body, chassis, i);
        }

        self.update_suspension(chassis.mass);
        for wheel in &self.wheels {
            let force = wheel.wheel_suspension_force.min(wheel.max_suspension_force);
            let impulse = wheel.raycast_info.contact_normal_ws * force * dt;
            chassis.apply_impulse_at_point(impulse, wheel.raycast_info.contact_point_ws);
        }

        self.update_friction(chassis, dt);
    }
}

fn resolve_single_unilateral(chassis: &Chassis, point: Vec3, normal: Vec3) -> f32 {
    let vel = chassis.velocity_at_point(point);
    let arm = point - chassis.position;
    let iaj = chassis.inv_inertia_world * arm.cross(normal);
    let jac = chassis.inv_mass + iaj.dot(iaj);
    -0.2 * normal.dot(vel) * inv(jac)
}

fn calc_rolling_friction(
    chassis: &Chassis,
    point: Vec3,
    forward: Vec3,
    max_impulse: f32,
    num_on_ground: usize,
) -> f32 {
    let arm = point - chassis.position;
    let gcross = arm.cross(forward);
    let denom = chassis.inv_mass + forward.dot((chassis.inv_inertia_world * gcross).cross(arm));
    let vrel = forward.dot(chassis.velocity_at_point(point));
    (-vrel * inv(denom) / num_on_ground as f32).clamp(-max_impulse, max_impulse)
}

// --- Server ------------------------------------------------------------

/// Server-only: which client connection currently drives this vehicle, and as
/// which player id (needed to respawn them on exit).
#[derive(Component, Clone, Copy, Debug)]
struct DrivenBy {
    client: Entity,
    player_id: u64,
}

/// Server-only: where a fallen (unowned) vehicle respawns.
#[derive(Component, Clone, Copy, Debug)]
struct Home(Vec3);

/// Server: spawn a driveable buggy at `translation`. Unowned it's a plain
/// dynamic body (rockets shove it, it settles on its chassis); while driven
/// it flips to kinematic and follows the owner's streamed pose.
pub fn spawn_buggy(commands: &mut Commands, translation: Vec3) {
    commands.spawn((
        Name::new("buggy"),
        Vehicle,
        Home(translation),
        Replicated,
        RigidBody::Dynamic,
        // Blast impulses write velocity out-of-band; never let it sleep through one.
        SleepingDisabled,
        chassis_collider(),
        vehicle_collision_layers(),
        Mass(CHASSIS_MASS),
        Position::new(translation),
        Transform::from_translation(translation),
    ));
}

/// Server-only: a vehicle rocket in flight, counting down to its splash.
#[derive(Component)]
struct ServerVehicleRocket {
    point: Vec3,
    timer: Timer,
}

pub fn add_server_vehicles(app: &mut App) {
    app.add_observer(board_or_exit)
        .add_observer(apply_vehicle_state)
        .add_observer(relay_vehicle_rockets)
        .add_observer(release_vehicle_on_disconnect)
        .add_systems(FixedPreUpdate, (reset_fallen_vehicles, detonate_vehicle_rockets));
}

/// Board the nearest free buggy (despawning the player entity — the driver's
/// client takes over the physics), or exit the one being driven (respawning
/// the player above it, keeping its momentum).
fn board_or_exit(
    request: On<FromClient<BoardVehicle>>,
    mut commands: Commands,
    players: Query<(Entity, &PlayerId, &PlayerOwner, &Position)>,
    vehicles: Query<(Entity, &Position, &LinearVelocity, Option<&DrivenBy>), With<Vehicle>>,
) {
    let Some(client) = request.client_id.entity() else {
        return;
    };

    // Already driving? Exit.
    if let Some((vehicle, position, velocity, driven)) = vehicles
        .iter()
        .find(|(.., driven)| driven.is_some_and(|driven| driven.client == client))
    {
        let player_id = driven.unwrap().player_id;
        spawn_player(
            &mut commands,
            client,
            player_id,
            position.0 + EXIT_OFFSET,
            velocity.0,
        );
        commands
            .entity(vehicle)
            .remove::<(Driver, DrivenBy)>()
            .insert(RigidBody::Dynamic);
        info!("player {player_id} exited a vehicle");
        return;
    }

    let Some((player, player_id, _, player_position)) = players
        .iter()
        .find(|(_, _, owner, _)| owner.0 == client)
    else {
        return;
    };

    let nearest = vehicles
        .iter()
        .filter(|(.., driven)| driven.is_none())
        .map(|(vehicle, position, ..)| (vehicle, position.0.distance(player_position.0)))
        .filter(|(_, distance)| *distance <= BOARD_RADIUS)
        .min_by(|a, b| a.1.total_cmp(&b.1));

    if let Some((vehicle, _)) = nearest {
        commands.entity(player).despawn();
        commands
            .entity(vehicle)
            .insert((
                Driver(player_id.0),
                DrivenBy {
                    client,
                    player_id: player_id.0,
                },
                RigidBody::Kinematic,
            ));
        info!("player {} boarded a vehicle", player_id.0);
    }
}

/// Apply the owner's streamed pose. This is the trust boundary — a speed or
/// teleport clamp goes here if strangers ever join.
fn apply_vehicle_state(
    state: On<FromClient<VehicleState>>,
    mut vehicles: Query<
        (
            &DrivenBy,
            &mut Position,
            &mut Rotation,
            &mut LinearVelocity,
            &mut AngularVelocity,
            &mut Transform,
        ),
        With<Vehicle>,
    >,
) {
    let Some(client) = state.client_id.entity() else {
        return;
    };

    for (driven, mut position, mut rotation, mut linvel, mut angvel, mut transform) in &mut vehicles
    {
        if driven.client != client {
            continue;
        }
        position.0 = state.position;
        rotation.0 = state.rotation;
        linvel.0 = state.linear_velocity;
        angvel.0 = state.angular_velocity;
        transform.translation = state.position;
        transform.rotation = state.rotation;
    }
}

/// A driver's connection died: free the buggy where it stands. (Their player
/// entity doesn't exist while driving, so the normal disconnect cleanup has
/// nothing to do.)
fn release_vehicle_on_disconnect(
    disconnected: On<Disconnected>,
    mut commands: Commands,
    vehicles: Query<(Entity, &DrivenBy), With<Vehicle>>,
) {
    let client = disconnected.event_target();
    for (vehicle, driven) in &vehicles {
        if driven.client == client {
            commands
                .entity(vehicle)
                .remove::<(Driver, DrivenBy)>()
                .insert(RigidBody::Dynamic);
            info!("player {} disconnected while driving", driven.player_id);
        }
    }
}

fn reset_fallen_vehicles(
    mut vehicles: Query<
        (&Home, &mut Position, &mut Rotation, &mut LinearVelocity, &mut AngularVelocity),
        (With<Vehicle>, Without<DrivenBy>),
    >,
) {
    for (home, mut position, mut rotation, mut linvel, mut angvel) in &mut vehicles {
        if position.y < VOID_Y {
            position.0 = home.0;
            rotation.0 = Quat::IDENTITY;
            linvel.0 = Vec3::ZERO;
            angvel.0 = Vec3::ZERO;
        }
    }
}

/// A driver fired: schedule the authoritative splash and relay the shot to
/// every client (they animate it and, if driving, respond to the blast).
/// The path is owner-baked; the server only stamps the firer id.
fn relay_vehicle_rockets(
    fired: On<FromClient<VehicleRocketFired>>,
    mut commands: Commands,
    vehicles: Query<&DrivenBy, With<Vehicle>>,
) {
    let Some(client) = fired.client_id.entity() else {
        return;
    };
    // Only actual drivers get the vehicle-rocket path; on foot you have the
    // regular predicted launcher.
    let Some(driven) = vehicles.iter().find(|driven| driven.client == client) else {
        return;
    };

    let message = VehicleRocketFired {
        firer: driven.player_id,
        ..**fired
    };
    commands.spawn((
        Name::new("vehicle rocket"),
        ServerVehicleRocket {
            point: message.start + message.dir * message.hit_distance,
            timer: Timer::from_seconds(message.fuse_seconds, TimerMode::Once),
        },
    ));
    commands.server_trigger(ToClients {
        mode: SendMode::Broadcast,
        message,
    });
}

/// Splash on-foot players and unowned dynamic bodies when a vehicle rocket's
/// fuse runs out. Players are kinematic rigid bodies (the KCC requires one),
/// so they're selected by PlayerId; bodies are checked for `Dynamic`
/// explicitly. Owned vehicles are kinematic and skipped — their drivers apply
/// the blast locally from the relayed event.
fn detonate_vehicle_rockets(
    time: Res<Time>,
    mut commands: Commands,
    mut rockets: Query<(Entity, &mut ServerVehicleRocket)>,
    mut players: Query<(&Position, &mut LinearVelocity), With<PlayerId>>,
    mut bodies: Query<
        (&RigidBody, &Position, &mut LinearVelocity, &ComputedMass),
        Without<PlayerId>,
    >,
) {
    for (entity, mut rocket) in &mut rockets {
        rocket.timer.tick(time.delta());
        if !rocket.timer.is_finished() {
            continue;
        }

        for (position, mut velocity) in &mut players {
            velocity.0 += rocket_impulse(rocket.point, position.0);
        }
        for (body, position, mut velocity, mass) in &mut bodies {
            let inverse_mass = mass.inverse();
            if !matches!(body, RigidBody::Dynamic) || inverse_mass <= 0.0 {
                continue;
            }
            velocity.0 +=
                rocket_impulse(rocket.point, position.0) * (BLAST_REFERENCE_MASS * inverse_mass);
        }
        commands.entity(entity).despawn();
    }
}

// --- Client ------------------------------------------------------------

/// Rendered stand-in for a replicated vehicle: damps toward the replicated
/// pose normally, snaps to the local sim while we drive it.
#[derive(Component)]
struct VehicleVisual {
    server_entity: Entity,
}

/// The locally simulated physics body for the vehicle we're driving. The
/// camera follows it; its pose streams to the server every fixed tick.
#[derive(Component)]
pub struct LocalVehicleSim {
    pub server_entity: Entity,
}

/// Client-side vehicle rocket in flight (ours immediately, everyone else's
/// via the server relay). Purely visual until the fuse ends — then it shoves
/// our sim if we're driving nearby.
#[derive(Component)]
struct VehicleRocketVisual {
    start: Vec3,
    dir: Vec3,
    hit_distance: f32,
    timer: Timer,
}

/// Short-lived explosion flash.
#[derive(Component)]
struct VehicleRocketBoom {
    timer: Timer,
}

pub fn add_client_vehicles(app: &mut App) {
    app.add_observer(blast_local_sim_on_rocket_hit)
        .add_observer(receive_vehicle_rockets)
        .add_systems(
            Update,
            (
                attach_vehicle_bodies,
                send_board_requests,
                manage_local_sim,
                fire_vehicle_rockets,
                update_vehicle_rockets,
                update_vehicle_visuals,
            )
                .chain(),
        )
        .add_systems(FixedUpdate, (drive_local_sim, rescue_local_sim))
        .add_systems(FixedLast, stream_local_sim);
}

/// New replicated vehicle: give it a client-side collider (so KCC prediction
/// treats it as world geometry, mirroring the server) and spawn its visual.
fn attach_vehicle_bodies(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    added: Query<(Entity, &Transform), Added<Vehicle>>,
) {
    for (server_entity, transform) in &added {
        commands
            .entity(server_entity)
            .insert((chassis_collider(), vehicle_collision_layers()));

        let body = materials.add(StandardMaterial {
            base_color: Color::srgb(0.9, 0.55, 0.15),
            perceptual_roughness: 0.6,
            ..default()
        });
        let rubber = materials.add(StandardMaterial {
            base_color: Color::srgb(0.08, 0.08, 0.09),
            perceptual_roughness: 0.9,
            ..default()
        });
        let wheel_mesh = meshes.add(Cylinder::new(WHEEL_RADIUS, WHEEL_WIDTH));

        commands
            .spawn((
                Name::new("buggy visual"),
                VehicleVisual { server_entity },
                Mesh3d(meshes.add(Cuboid::new(
                    CHASSIS_HALF.x * 2.0,
                    CHASSIS_HALF.y * 2.0,
                    CHASSIS_HALF.z * 2.0,
                ))),
                MeshMaterial3d(body),
                *transform,
                Visibility::Visible,
            ))
            .with_children(|parent| {
                for connection in WHEEL_CONNECTIONS {
                    // Resting wheel center; cylinder axis is Y, wheel axle is Z.
                    parent.spawn((
                        Mesh3d(wheel_mesh.clone()),
                        MeshMaterial3d(rubber.clone()),
                        Transform::from_translation(connection + Vec3::NEG_Y * SUSPENSION_REST)
                            .with_rotation(Quat::from_rotation_x(FRAC_PI_2)),
                    ));
                }
            });
    }
}

fn send_board_requests(keys: Res<ButtonInput<KeyCode>>, mut commands: Commands) {
    if keys.just_pressed(KeyCode::KeyG) {
        commands.client_trigger(BoardVehicle);
    }
}

/// Spawn/despawn the local physics sim as `Driver(us)` comes and goes on a
/// replicated vehicle. The replicated copy's collider is disabled while we
/// drive it — the sim body replaces it, and our own state round-tripping
/// through the server must not become an obstacle to ourselves.
fn manage_local_sim(
    mut commands: Commands,
    local: Res<LocalPlayerId>,
    vehicles: Query<(Entity, &Transform, Option<&LinearVelocity>, Option<&Driver>), With<Vehicle>>,
    sims: Query<(Entity, &LocalVehicleSim)>,
) {
    let driven = vehicles
        .iter()
        .find(|(.., driver)| driver.is_some_and(|driver| local.is_assigned_to(driver.0)));

    for (sim_entity, sim) in &sims {
        if driven.is_none_or(|(vehicle, ..)| vehicle != sim.server_entity) {
            commands.entity(sim_entity).despawn();
            if let Ok(mut server_entity) = commands.get_entity(sim.server_entity) {
                server_entity.remove::<ColliderDisabled>();
            }
        }
    }

    if let Some((vehicle, transform, velocity, _)) = driven
        && !sims.iter().any(|(_, sim)| sim.server_entity == vehicle)
    {
        commands.entity(vehicle).insert(ColliderDisabled);
        commands.spawn((
            Name::new("local vehicle sim"),
            LocalVehicleSim { server_entity: vehicle },
            VehicleController::new_buggy(),
            RigidBody::Dynamic,
            SleepingDisabled,
            chassis_collider(),
            vehicle_collision_layers(),
            Mass(CHASSIS_MASS),
            Position::new(transform.translation),
            Rotation(transform.rotation),
            LinearVelocity(velocity.map(|velocity| velocity.0).unwrap_or_default()),
            AngularVelocity::default(),
            *transform,
        ));
    }
}

/// Step the suspension/friction controller on the sim body each fixed tick,
/// before Avian integrates. Input reads the keyboard directly — no usercmd,
/// no prediction, the sim IS the authority.
fn drive_local_sim(
    fixed: Res<Time<Fixed>>,
    keys: Res<ButtonInput<KeyCode>>,
    spatial: SpatialQuery,
    mut sims: Query<
        (
            Entity,
            &mut VehicleController,
            &Position,
            &Rotation,
            &mut LinearVelocity,
            &mut AngularVelocity,
            &ComputedMass,
            &ComputedAngularInertia,
        ),
        With<LocalVehicleSim>,
    >,
) {
    let dt = fixed.timestep().as_secs_f32();
    for (entity, mut controller, position, rotation, mut linvel, mut angvel, mass, inertia) in
        &mut sims
    {
        let mut throttle = 0.0;
        if keys.pressed(KeyCode::KeyW) {
            throttle += 1.0;
        }
        if keys.pressed(KeyCode::KeyS) {
            throttle -= 1.0;
        }
        let mut steer = 0.0;
        if keys.pressed(KeyCode::KeyA) {
            steer += 1.0;
        }
        if keys.pressed(KeyCode::KeyD) {
            steer -= 1.0;
        }
        let braking = keys.pressed(KeyCode::Space) || keys.pressed(KeyCode::ControlLeft);
        controller.set_inputs(throttle, steer, if braking { BRAKE_IMPULSE } else { 0.0 });

        let mut chassis = Chassis::new(position.0, rotation.0, linvel.0, angvel.0, mass, inertia);
        controller.update_vehicle(dt, entity, &mut chassis, &spatial);
        linvel.0 = chassis.linvel;
        angvel.0 = chassis.angvel;
    }
}

/// R flips the buggy upright in place; falling into the void hoists it back
/// up automatically. Separate from `drive_local_sim` because writing
/// `Position` can't coexist with its `SpatialQuery`.
fn rescue_local_sim(
    keys: Res<ButtonInput<KeyCode>>,
    mut sims: Query<
        (&mut Position, &mut Rotation, &mut LinearVelocity, &mut AngularVelocity),
        With<LocalVehicleSim>,
    >,
) {
    for (mut position, mut rotation, mut linvel, mut angvel) in &mut sims {
        let fell_out = position.y < VOID_Y;
        if !keys.just_pressed(KeyCode::KeyR) && !fell_out {
            continue;
        }
        if fell_out {
            position.0 = Vec3::new(0.0, 3.0, 0.0);
        } else {
            position.y += 1.5;
        }
        rotation.0 = Quat::IDENTITY;
        linvel.0 = Vec3::ZERO;
        angvel.0 = Vec3::ZERO;
    }
}

/// Stream the sim's post-physics pose to the server, once per fixed tick.
fn stream_local_sim(
    mut commands: Commands,
    sims: Query<(&Position, &Rotation, &LinearVelocity, &AngularVelocity), With<LocalVehicleSim>>,
) {
    for (position, rotation, linvel, angvel) in &sims {
        commands.client_trigger(VehicleState {
            position: position.0,
            rotation: rotation.0,
            linear_velocity: linvel.0,
            angular_velocity: angvel.0,
        });
    }
}

/// Visuals: our own sim renders raw (zero latency); everything else damps
/// toward the replicated pose to hide the 20 Hz staircase.
fn update_vehicle_visuals(
    time: Res<Time>,
    mut visuals: Query<(&VehicleVisual, &mut Transform)>,
    sims: Query<(&LocalVehicleSim, &Transform), Without<VehicleVisual>>,
    vehicles: Query<&Transform, (With<Vehicle>, Without<VehicleVisual>, Without<LocalVehicleSim>)>,
) {
    let alpha = 1.0 - (-VISUAL_FOLLOW_RATE * time.delta_secs()).exp();
    for (visual, mut transform) in &mut visuals {
        if let Some((_, sim_transform)) = sims
            .iter()
            .find(|(sim, _)| sim.server_entity == visual.server_entity)
        {
            *transform = *sim_transform;
        } else if let Ok(target) = vehicles.get(visual.server_entity) {
            transform.translation = transform.translation.lerp(target.translation, alpha);
            transform.rotation = transform.rotation.slerp(target.rotation, alpha);
        }
    }
}

/// Other players' (on-foot) rockets blast our sim too. The server can't shove
/// it — the server copy is kinematic while owned — so the owner applies the
/// impulse. That's what being the physics authority means.
fn blast_local_sim_on_rocket_hit(
    hit: On<RocketHit>,
    mut sims: Query<(&Position, &mut LinearVelocity), With<LocalVehicleSim>>,
) {
    for (position, mut velocity) in &mut sims {
        velocity.0 += rocket_impulse(hit.point, position.0) * (BLAST_REFERENCE_MASS / CHASSIS_MASS);
    }
}

/// Fire from the driver's seat, along the camera look. The path is baked
/// against our own sim (we're the authority), the visual spawns immediately,
/// and the server relays the shot to everyone else. The ray happily hits our
/// own chassis — shooting your own floor IS the rocket jump.
fn fire_vehicle_rockets(
    input: Res<ClientInput>,
    local: Res<LocalPlayerId>,
    mut was_pressed: Local<bool>,
    spatial: SpatialQuery,
    sims: Query<&Position, With<LocalVehicleSim>>,
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    let pressed = input.buttons.contains(ROCKET_FIRE);
    let edge = pressed && !*was_pressed;
    *was_pressed = pressed;

    let Ok(position) = sims.single() else {
        return;
    };
    if !edge {
        return;
    }

    // Reuse the library's closed-form rocket math as a pure value: one raycast
    // fixes the whole path and fuse.
    let rocket = Rocket::fire(
        PlayerId(local.0.unwrap_or_default()),
        0,
        position.0,
        input.look,
        &spatial,
    );
    let fired = VehicleRocketFired {
        firer: local.0.unwrap_or_default(),
        start: rocket.start,
        dir: rocket.dir,
        hit_distance: rocket.hit_distance,
        fuse_seconds: rocket.fuse_ticks as f32 / FIXED_TIMESTEP_HZ as f32,
    };
    spawn_vehicle_rocket_visual(&mut commands, &mut meshes, &mut materials, fired);
    commands.client_trigger(fired);
}

/// Someone else's vehicle rocket, via the server relay. Ours already spawned
/// on fire.
fn receive_vehicle_rockets(
    fired: On<VehicleRocketFired>,
    local: Res<LocalPlayerId>,
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    if local.is_assigned_to(fired.firer) {
        return;
    }
    spawn_vehicle_rocket_visual(&mut commands, &mut meshes, &mut materials, *fired);
}

fn spawn_vehicle_rocket_visual(
    commands: &mut Commands,
    meshes: &mut Assets<Mesh>,
    materials: &mut Assets<StandardMaterial>,
    fired: VehicleRocketFired,
) {
    let mut transform = Transform::from_translation(fired.start);
    transform.look_to(fired.dir, Vec3::Y);
    commands.spawn((
        Name::new("vehicle rocket visual"),
        VehicleRocketVisual {
            start: fired.start,
            dir: fired.dir,
            hit_distance: fired.hit_distance,
            timer: Timer::from_seconds(fired.fuse_seconds.max(0.01), TimerMode::Once),
        },
        Mesh3d(meshes.add(Cuboid::new(0.12, 0.12, 0.5))),
        MeshMaterial3d(materials.add(StandardMaterial {
            base_color: Color::srgb(1.0, 0.55, 0.1),
            emissive: LinearRgba::new(6.0, 2.0, 0.3, 1.0),
            ..default()
        })),
        transform,
    ));
}

/// Fly each rocket along its baked path; at the fuse, boom: flash + shove our
/// own sim if we're in the splash (self rocket-jumps included).
fn update_vehicle_rockets(
    time: Res<Time>,
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    mut rockets: Query<(Entity, &mut VehicleRocketVisual, &mut Transform)>,
    mut sims: Query<(&Position, &mut LinearVelocity), With<LocalVehicleSim>>,
    mut booms: Query<(Entity, &mut VehicleRocketBoom, &mut Transform), Without<VehicleRocketVisual>>,
) {
    for (entity, mut rocket, mut transform) in &mut rockets {
        rocket.timer.tick(time.delta());
        let point = rocket.start + rocket.dir * rocket.hit_distance * rocket.timer.fraction();
        transform.translation = point;

        if rocket.timer.is_finished() {
            for (position, mut velocity) in &mut sims {
                velocity.0 +=
                    rocket_impulse(point, position.0) * (BLAST_REFERENCE_MASS / CHASSIS_MASS);
            }
            commands.entity(entity).despawn();
            commands.spawn((
                Name::new("vehicle rocket boom"),
                VehicleRocketBoom {
                    timer: Timer::from_seconds(0.4, TimerMode::Once),
                },
                Mesh3d(meshes.add(Sphere::new(1.2))),
                MeshMaterial3d(materials.add(StandardMaterial {
                    base_color: Color::srgba(1.0, 0.4, 0.1, 0.5),
                    emissive: LinearRgba::new(8.0, 2.5, 0.4, 1.0),
                    alpha_mode: AlphaMode::Blend,
                    ..default()
                })),
                Transform::from_translation(point),
            ));
        }
    }

    for (entity, mut boom, mut transform) in &mut booms {
        boom.timer.tick(time.delta());
        transform.scale = Vec3::splat(boom.timer.remaining_secs().max(0.05) / 0.4);
        if boom.timer.is_finished() {
            commands.entity(entity).despawn();
        }
    }
}
