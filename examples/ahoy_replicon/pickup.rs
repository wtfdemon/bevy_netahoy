//! Server-authoritative crate pickup, HL2 gravity-gun style, on avian_pickup.
//!
//! The prop is a plain server-dynamic body its whole life — [`BodySnapshot`]
//! down, [`RigidBody::Static`] + damped visual on clients, exactly the vehicle
//! pattern minus ownership transfer. avian_pickup runs *only on the server*,
//! driven by replicated [`PickupInput`]; a per-player actor entity mirrors the
//! player's eye pose so the pull raycast and hold spring see what the player
//! sees. `Holding` is mirrored into the replicated [`HeldBy`], which both
//! peers use to exclude the held prop from that player's KCC filter (server
//! KCC and client prediction KCC alike — otherwise the crate in your face is
//! a wall you carry with you).
//!
//! Feel is handled at the presentation layer, no authority ever moves:
//! - grab: on the key press the client raycasts for the prop it's probably
//!   grabbing and springs its visual to the hold point immediately; the
//!   replicated `HeldBy` confirms (or the guess times out and reverts).
//! - hold: while `HeldBy(us)`, the visual springs to our *predicted* camera
//!   instead of damping toward the interpolated (delayed) snapshot.
//! - throw: the client launches a ballistic ghost with the same fixed throw
//!   speed the server actor is configured with, then blends back into the
//!   replicated stream — a thrown crate is a rocket with gravity, and for the
//!   first RTT the guess and the truth agree.

// This mod is compiled into both example binaries; the server half is "dead"
// in the client build and vice versa.
#![allow(dead_code)]

use avian3d::prelude::*;
use avian_pickup::prelude::*;
use avian_pickup::Holding;
use bevy::prelude::*;
use bevy_ahoy::{prelude::*, CharacterLook};
use bevy_netahoy::{
    BodySnapshot, ClientInput, ClientNetAhoySystems, LocalPlayerId, PlayerId, PlayerOwner,
    QueuedUserCmds, ServerNetAhoySystems, WORLD_COLLISION_LAYER,
};
use bevy_replicon::prelude::*;

pub use ahoy_replicon::{HeldBy, PickupAction, PickupInput, Prop};

const PROP_SIZE: f32 = 0.7;
const PROP_MASS: f32 = 8.0;
/// Fixed on both ends: the server actor throws with exactly this speed and
/// the client's ballistic ghost assumes it, so the predicted arc matches.
const THROW_SPEED: f32 = 11.0;
/// Matches the library's rocket eye height, where the actor and the client
/// hold point both anchor.
const EYE_HEIGHT: f32 = 0.6;
/// How far in front of the eye the held visual sits (actor hold preferred
/// distance + half a crate).
const HOLD_DISTANCE: f32 = 1.3;
/// Avian's default gravity, which is what the server prop falls under.
const PROP_GRAVITY: f32 = 9.81;
/// Snap rate of the held visual toward the hold point.
const HOLD_FOLLOW_RATE: f32 = 18.0;
/// Damping rate of free prop visuals chasing the replicated pose.
const VISUAL_FOLLOW_RATE: f32 = 14.0;
/// How long a thrown ghost takes to defer fully back to the snapshot stream.
const THROW_BLEND_SECONDS: f32 = 0.35;
/// An optimistic grab beyond this range would look wrong (the server pulls
/// distant props in gradually), so the instant spring only covers close grabs.
const OPTIMISTIC_GRAB_RANGE: f32 = 2.5;
/// Give up an unconfirmed optimistic grab after this long (denied or lost race).
const GRAB_GUESS_TIMEOUT: f32 = 0.6;
const VOID_Y: f32 = -12.0;

const GRAB_KEY: KeyCode = KeyCode::KeyQ;
const THROW_KEY: KeyCode = KeyCode::KeyT;

fn prop_collider() -> Collider {
    Collider::cuboid(PROP_SIZE, PROP_SIZE, PROP_SIZE)
}

/// Same layers as world geometry: the KCC climbs it, wheel rays drive over
/// it, rockets detonate on it, and blasts shove it (it's a dynamic body).
fn prop_collision_layers() -> CollisionLayers {
    CollisionLayers::new(WORLD_COLLISION_LAYER, LayerMask::ALL)
}

// --- Both peers ----------------------------------------------------------

/// Keyed off the replicated `HeldBy`, so the identical observer pair runs on
/// the server (real KCC) and the client (prediction KCC): a held prop must
/// not be solid to its holder, or it's a wall glued to their face.
fn exclude_held_prop_from_kcc(
    insert: On<Insert, HeldBy>,
    held: Query<&HeldBy>,
    mut controllers: Query<(&PlayerId, &mut CharacterController)>,
) {
    let prop = insert.entity;
    let Ok(held_by) = held.get(prop) else {
        return;
    };
    for (player_id, mut controller) in &mut controllers {
        if player_id.0 == held_by.0 {
            controller.filter.excluded_entities.insert(prop);
        }
    }
}

fn readmit_dropped_prop_to_kcc(
    replace: On<Discard, HeldBy>,
    held: Query<&HeldBy>,
    mut controllers: Query<(&PlayerId, &mut CharacterController)>,
) {
    let prop = replace.entity;
    let Ok(held_by) = held.get(prop) else {
        return;
    };
    for (player_id, mut controller) in &mut controllers {
        if player_id.0 == held_by.0 {
            controller.filter.excluded_entities.remove(&prop);
        }
    }
}

// --- Server ----------------------------------------------------------------

/// Server-only: the avian_pickup actor entity mirroring this player's eye.
#[derive(Component, Clone, Copy, Debug)]
struct PickupActorOf {
    player: Entity,
    player_id: u64,
}

/// Server-only: where a fallen prop respawns.
#[derive(Component, Clone, Copy, Debug)]
struct PropHome(Vec3);

pub fn add_server_pickup(app: &mut App) {
    app.add_plugins(AvianPickupPlugin::default())
        .add_observer(queue_pickup_input)
        .add_observer(mark_held_prop)
        .add_observer(unmark_released_prop)
        .add_observer(exclude_held_prop_from_kcc)
        .add_observer(readmit_dropped_prop_to_kcc)
        .add_systems(
            FixedPreUpdate,
            (sync_pickup_actors, reset_fallen_props)
                .after(ServerNetAhoySystems::ApplyCommands),
        );
}

/// Server: spawn a pickupable crate at `translation`.
pub fn spawn_prop(commands: &mut Commands, translation: Vec3) {
    commands.spawn((
        Name::new("prop crate"),
        Prop,
        PropHome(translation),
        Replicated,
        // The only thing that crosses the wire; the library keeps it fresh.
        BodySnapshot::default(),
        RigidBody::Dynamic,
        // Blast impulses write velocity out-of-band; never sleep through one.
        SleepingDisabled,
        prop_collider(),
        prop_collision_layers(),
        Mass(PROP_MASS),
        Position::new(translation),
        Transform::from_translation(translation),
    ));
}

/// Keep one actor entity per commanding player (bots have no
/// [`QueuedUserCmds`] and get none), posed at the player's eye each tick
/// before physics runs — avian_pickup raycasts and holds from its
/// `GlobalTransform`. Orphans (player despawned by boarding a vehicle or
/// disconnecting) are despawned, which also releases anything held.
fn sync_pickup_actors(
    mut commands: Commands,
    players: Query<(Entity, &PlayerId, &Position, &CharacterLook), With<QueuedUserCmds>>,
    mut actors: Query<(Entity, &PickupActorOf, &mut Transform, &mut GlobalTransform)>,
) {
    for (actor, of, ..) in &actors {
        if players.get(of.player).is_err() {
            commands.entity(actor).despawn();
        }
    }

    for (player, player_id, ..) in &players {
        if !actors.iter().any(|(_, of, ..)| of.player == player) {
            commands.spawn((
                Name::new(format!("pickup actor for player {}", player_id.0)),
                PickupActorOf {
                    player,
                    player_id: player_id.0,
                },
                AvianPickupActor {
                    // Props are world-layer members; only dynamic bodies are
                    // considered anyway, so static world geometry never matches.
                    prop_filter: SpatialQueryFilter::from_mask(WORLD_COLLISION_LAYER),
                    obstacle_filter: SpatialQueryFilter::from_mask(WORLD_COLLISION_LAYER),
                    // The actor entity carries no colliders of its own; match
                    // nothing and let the hold math use its min-distance fallback.
                    actor_filter: SpatialQueryFilter::from_mask(LayerMask::NONE),
                    throw: AvianPickupActorThrowConfig {
                        // Deterministic on purpose: the client's thrown ghost
                        // assumes exactly this launch velocity.
                        linear_speed_range: THROW_SPEED..=THROW_SPEED,
                        angular_speed_range: 0.0..=0.0,
                        ..default()
                    },
                    ..default()
                },
                Transform::default(),
                GlobalTransform::default(),
            ));
        }
    }

    for (_, of, mut transform, mut global) in &mut actors {
        let Ok((_, _, position, look)) = players.get(of.player) else {
            continue;
        };
        *transform = Transform::from_translation(position.0 + Vec3::Y * EYE_HEIGHT)
            .with_rotation(Quat::from_euler(EulerRot::YXZ, look.yaw, look.pitch, 0.0));
        // Written directly: avian_pickup reads it inside the physics schedule,
        // before bevy's render-rate propagation would get around to it.
        *global = GlobalTransform::from(*transform);
    }
}

/// Feed a client's pickup input to its actor. avian_pickup validates the rest
/// (range, cone, line of sight, mass, cooldowns) — this is only routing.
fn queue_pickup_input(
    input: On<FromClient<PickupInput>>,
    players: Query<(Entity, &PlayerOwner)>,
    actors: Query<(Entity, &PickupActorOf)>,
    mut messages: MessageWriter<AvianPickupInput>,
) {
    let Some(client) = input.client_id.entity() else {
        return;
    };
    let Some(player) = players
        .iter()
        .find_map(|(player, owner)| (owner.0 == client).then_some(player))
    else {
        return;
    };
    let Some(actor) = actors
        .iter()
        .find_map(|(actor, of)| (of.player == player).then_some(actor))
    else {
        return;
    };

    let action = match input.action {
        PickupAction::Pull => AvianPickupAction::Pull,
        PickupAction::Throw => AvianPickupAction::Throw,
        PickupAction::Drop => AvianPickupAction::Drop,
    };
    messages.write(AvianPickupInput { actor, action });
}

/// Mirror avian_pickup's `Holding` (on the actor) into the replicated
/// `HeldBy` (on the prop).
fn mark_held_prop(
    insert: On<Insert, Holding>,
    actors: Query<(&Holding, &PickupActorOf)>,
    mut commands: Commands,
) {
    let Ok((holding, of)) = actors.get(insert.entity) else {
        return;
    };
    commands.entity(holding.0).insert(HeldBy(of.player_id));
}

fn unmark_released_prop(
    replace: On<Discard, Holding>,
    actors: Query<(&Holding, &PickupActorOf)>,
    mut commands: Commands,
) {
    let Ok((holding, _)) = actors.get(replace.entity) else {
        return;
    };
    if let Ok(mut prop) = commands.get_entity(holding.0) {
        prop.remove::<HeldBy>();
    }
}

fn reset_fallen_props(
    mut props: Query<
        (&PropHome, &mut Position, &mut Rotation, &mut LinearVelocity, &mut AngularVelocity),
        (With<Prop>, Without<HeldBy>),
    >,
) {
    for (home, mut position, mut rotation, mut linvel, mut angvel) in &mut props {
        if position.y < VOID_Y {
            position.0 = home.0;
            rotation.0 = Quat::IDENTITY;
            linvel.0 = Vec3::ZERO;
            angvel.0 = Vec3::ZERO;
        }
    }
}

// --- Client ------------------------------------------------------------

/// Rendered stand-in for a replicated prop: damps toward the replicated pose
/// normally, springs to the hold point while held by us, flies ballistically
/// for a beat after we throw.
#[derive(Component)]
struct PropVisual {
    server_entity: Entity,
}

/// An optimistic grab: we asked, sprang the visual immediately, and are
/// waiting for `HeldBy` to confirm. Times out back to snapshot-following.
#[derive(Component)]
struct GrabGuess {
    timer: Timer,
}

/// Client-predicted throw: fly the visual on the same launch the server will
/// perform, blending into the replicated stream as it arrives.
#[derive(Component)]
struct ThrowGhost {
    velocity: Vec3,
    age: f32,
}

pub fn add_client_pickup(app: &mut App) {
    app.add_observer(exclude_held_prop_from_kcc)
        .add_observer(readmit_dropped_prop_to_kcc)
        // Same reasoning as vehicles: collider + seeded pose must exist before
        // avian's first look at the entity.
        .add_systems(PreUpdate, attach_prop_bodies.after(ClientSystems::Receive))
        .add_systems(
            Update,
            (apply_prop_snapshots, send_pickup_input, update_prop_visuals)
                .chain()
                .after(ClientNetAhoySystems::Interpolate),
        );
}

/// New replicated prop: client-side collider (static — the KCC treats it as
/// world geometry, mirroring the server; `apply_prop_snapshots` stays the sole
/// author of its pose) and a visual.
fn attach_prop_bodies(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    added: Query<(Entity, &BodySnapshot), (With<Prop>, Without<Collider>)>,
) {
    for (server_entity, snapshot) in &added {
        if snapshot.tick == 0 {
            continue;
        }
        commands.entity(server_entity).insert((
            prop_collider(),
            prop_collision_layers(),
            RigidBody::Static,
            Position::new(snapshot.position),
            Rotation(snapshot.rotation),
            Transform::from_translation(snapshot.position).with_rotation(snapshot.rotation),
        ));

        commands.spawn((
            Name::new("prop crate visual"),
            PropVisual { server_entity },
            Mesh3d(meshes.add(Cuboid::new(PROP_SIZE, PROP_SIZE, PROP_SIZE))),
            MeshMaterial3d(materials.add(StandardMaterial {
                base_color: Color::srgb(0.72, 0.52, 0.28),
                perceptual_roughness: 0.9,
                ..default()
            })),
            Transform::from_translation(snapshot.position).with_rotation(snapshot.rotation),
            Visibility::Visible,
        ));
    }
}

/// The airlock: the only writer of the replicated copy's client-side physics
/// pose (same contract as the vehicle copy).
fn apply_prop_snapshots(
    mut props: Query<(&BodySnapshot, &mut Position, &mut Rotation), With<Prop>>,
) {
    for (snapshot, mut position, mut rotation) in &mut props {
        if snapshot.tick == 0 {
            continue;
        }
        position.0 = snapshot.position;
        rotation.0 = snapshot.rotation;
    }
}

/// Q held = pull/carry (release to drop), T = throw. Pull repeats every frame
/// (avian_pickup's contract — an actor with no input this update stops
/// pulling); throw/drop are edges. The optimistic grab and predicted throw
/// spring/launch the visual on the same frame the input leaves.
fn send_pickup_input(
    keys: Res<ButtonInput<KeyCode>>,
    input: Res<ClientInput>,
    local: Res<LocalPlayerId>,
    time: Res<Time>,
    spatial: SpatialQuery,
    mut commands: Commands,
    props: Query<(Entity, Option<&HeldBy>), With<Prop>>,
    presentations: Query<&Transform, With<bevy_netahoy::LocalPresentationPlayer>>,
    mut visuals: Query<(Entity, &PropVisual, &Transform, Option<&mut GrabGuess>), Without<bevy_netahoy::LocalPresentationPlayer>>,
) {
    // Tick and expire optimistic guesses; a confirmed `HeldBy` (any holder —
    // if someone else won the race our spring was wrong too) also clears them.
    for (visual_entity, visual, _, guess) in &mut visuals {
        let Some(mut guess) = guess else {
            continue;
        };
        let confirmed = props
            .get(visual.server_entity)
            .is_ok_and(|(_, held)| held.is_some());
        guess.timer.tick(time.delta());
        if confirmed || guess.timer.is_finished() {
            commands.entity(visual_entity).remove::<GrabGuess>();
        }
    }

    let held_prop = props.iter().find_map(|(prop, held)| {
        held.filter(|held| local.is_assigned_to(held.0)).map(|_| prop)
    });
    let guessed_prop = visuals
        .iter()
        .find_map(|(_, visual, _, guess)| guess.is_some().then_some(visual.server_entity));

    if keys.pressed(GRAB_KEY) {
        commands.client_trigger(PickupInput { action: PickupAction::Pull });
    }
    if keys.just_released(GRAB_KEY) && held_prop.is_some() {
        commands.client_trigger(PickupInput { action: PickupAction::Drop });
    }

    // Optimistic grab: spring the prop we're probably about to hold. Only
    // point-blank — distant props get pulled in over time by the server, and
    // an instant spring would lie about that.
    if keys.just_pressed(GRAB_KEY)
        && held_prop.is_none()
        && let Ok(presentation) = presentations.single()
    {
        let eye = presentation.translation + Vec3::Y * EYE_HEIGHT;
        let rotation = Quat::from_euler(EulerRot::YXZ, input.look.x, input.look.y, 0.0);
        let direction = Dir3::new(rotation * Vec3::NEG_Z).unwrap_or(Dir3::NEG_Z);
        let filter = SpatialQueryFilter::from_mask(WORLD_COLLISION_LAYER);
        if let Some(hit) = spatial.cast_ray(eye, direction, OPTIMISTIC_GRAB_RANGE, true, &filter)
            && props.contains(hit.entity)
        {
            for (visual_entity, visual, ..) in &visuals {
                if visual.server_entity == hit.entity {
                    commands.entity(visual_entity).insert(GrabGuess {
                        timer: Timer::from_seconds(GRAB_GUESS_TIMEOUT, TimerMode::Once),
                    });
                }
            }
        }
    }

    // Predicted throw: launch the ghost with the exact speed the server actor
    // is configured for, along our own look — for the first RTT a thrown
    // crate is open-air ballistics, so guess and truth agree.
    if keys.just_pressed(THROW_KEY)
        && let Some(prop) = held_prop.or(guessed_prop)
    {
        commands.client_trigger(PickupInput { action: PickupAction::Throw });
        let rotation = Quat::from_euler(EulerRot::YXZ, input.look.x, input.look.y, 0.0);
        for (visual_entity, visual, ..) in &visuals {
            if visual.server_entity == prop {
                commands.entity(visual_entity).remove::<GrabGuess>().insert(ThrowGhost {
                    velocity: rotation * Vec3::NEG_Z * THROW_SPEED,
                    age: 0.0,
                });
            }
        }
    }
}

/// One system, three regimes per visual: thrown ghosts fly and blend back to
/// the stream, our held (or optimistically grabbed) crate springs to the
/// predicted camera's hold point, everything else damps toward the snapshot.
fn update_prop_visuals(
    time: Res<Time>,
    input: Res<ClientInput>,
    local: Res<LocalPlayerId>,
    mut commands: Commands,
    props: Query<(&BodySnapshot, Option<&HeldBy>), With<Prop>>,
    presentations: Query<
        &Transform,
        (With<bevy_netahoy::LocalPresentationPlayer>, Without<PropVisual>),
    >,
    mut visuals: Query<
        (Entity, &PropVisual, &mut Transform, Option<&mut ThrowGhost>, Has<GrabGuess>),
        With<PropVisual>,
    >,
) {
    let dt = time.delta_secs();
    let follow_alpha = 1.0 - (-VISUAL_FOLLOW_RATE * dt).exp();
    let hold_alpha = 1.0 - (-HOLD_FOLLOW_RATE * dt).exp();

    for (visual_entity, visual, mut transform, ghost, guessed) in &mut visuals {
        let Ok((snapshot, held_by)) = props.get(visual.server_entity) else {
            commands.entity(visual_entity).despawn();
            continue;
        };
        if snapshot.tick == 0 {
            continue;
        }

        if let Some(mut ghost) = ghost {
            // Ballistic guess, deferring to the replicated stream as it lands.
            ghost.age += dt;
            ghost.velocity.y -= PROP_GRAVITY * dt;
            let ballistic = transform.translation + ghost.velocity * dt;
            let blend = (ghost.age / THROW_BLEND_SECONDS).clamp(0.0, 1.0);
            transform.translation = ballistic.lerp(snapshot.position, blend);
            transform.rotation = transform.rotation.slerp(snapshot.rotation, blend);
            if blend >= 1.0 {
                commands.entity(visual_entity).remove::<ThrowGhost>();
            }
            continue;
        }

        let held_by_us = held_by.is_some_and(|held| local.is_assigned_to(held.0));
        if (held_by_us || guessed)
            && let Ok(presentation) = presentations.single()
        {
            // Spring to the predicted camera's hold point: our camera is
            // predicted, so the crate feels instant even though the server
            // owns it — and corrections never yank it, it tracks the camera.
            let rotation = Quat::from_euler(EulerRot::YXZ, input.look.x, input.look.y, 0.0);
            let hold = presentation.translation
                + Vec3::Y * EYE_HEIGHT
                + rotation * Vec3::NEG_Z * HOLD_DISTANCE;
            transform.translation = transform.translation.lerp(hold, hold_alpha);
            transform.rotation = transform
                .rotation
                .slerp(Quat::from_rotation_y(input.look.x), hold_alpha);
            continue;
        }

        transform.translation = transform.translation.lerp(snapshot.position, follow_alpha);
        transform.rotation = transform.rotation.slerp(snapshot.rotation, follow_alpha);
    }
}
