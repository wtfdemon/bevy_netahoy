//! The world as one player saw it: what their step collides with.
//!
//! Two things, each the same on both peers:
//!
//! - The level: avian's live world ([`LiveWorld`]). It never moves, so now and then are the same.
//! - Every moving body within [`RELEVANT`] of the player: where its snapshot history had it at
//!   the seen tick and one tick on ([`mover_times`]), as a bevy_ahoy [`Mover`]. The command's
//!   rays meet the bodies at the seen time itself, sub-tick: where the player saw them.
//!
//! So anything on the step's filter that moves must have a [`BodySnapshot`] history; a
//! live-only body (a client's own effect) stays off [`WORLD_COLLISION_LAYER`].
//!
//! Moving among them (the ride, the casts, the slide) is bevy_ahoy's
//! [`AmongMovers`](bevy_ahoy::movers::AmongMovers).

use avian3d::prelude::*;
use bevy::{ecs::system::SystemParam, prelude::*};
use bevy_ahoy::{LiveWorld, Mover};
use bevy_replicon::shared::server_entity_map::ServerEntityMap;

use crate::{
    history::History,
    poses::{BodyPose, RemoteRenderTime, body_at},
    protocol::{BodySnapshot, SubtickFire, TickRate, WORLD_COLLISION_LAYER},
};

/// Past this, a body is not a mover for a player. Metres.
pub(crate) const RELEVANT: f32 = 60.0;

/// The two times a command that saw `seen` moves among the bodies at: the seen tick (`current`)
/// and one tick on (`next`). Whole ticks are exact history entries, so no curve between
/// snapshots enters the move. A rider ends the step on the body's `next` pose, so a client that
/// draws both at the end of the tick and eases them the same way keeps the rider on it.
pub(crate) fn mover_times(seen: RemoteRenderTime) -> (RemoteRenderTime, RemoteRenderTime) {
    (
        RemoteRenderTime::new(seen.tick, 0.0),
        RemoteRenderTime::new(seen.tick + 1, 0.0),
    )
}

/// Everything a step reads of the world, read-only.
#[derive(SystemParam)]
pub(crate) struct SeenWorld<'w, 's> {
    level: LiveWorld<'w, 's>,
    bodies: Query<'w, 's, (Entity, &'static History<BodySnapshot>)>,
    body_colliders: Query<'w, 's, &'static RigidBodyColliders>,
    colliders: Query<
        'w,
        's,
        (
            &'static Collider,
            Option<&'static ColliderTransform>,
            Option<&'static CollisionLayers>,
        ),
        (Without<ColliderDisabled>, Without<Sensor>),
    >,
    rate: Res<'w, TickRate>,
    /// On a client, the server entity of each replicated body: what both peers order them by.
    entity_map: Option<Res<'w, ServerEntityMap>>,
}

struct SeenCollider {
    entity: Entity,
    collider: Collider,
    /// In the body's frame.
    local: Isometry3d,
}

impl SeenCollider {
    /// What both peers order a body's parts by: where each sits on the body, then its box.
    /// Bits, not values: only that every peer gets one order matters.
    fn order(&self) -> [u32; 13] {
        let (at, turned) = (self.local.translation, self.local.rotation);
        let aabb = self.collider.aabb(Vec3::ZERO, Quat::IDENTITY);
        let (min, max) = (aabb.min, aabb.max);
        [
            at.x, at.y, at.z, turned.x, turned.y, turned.z, turned.w, min.x, min.y, min.z, max.x,
            max.y, max.z,
        ]
        .map(f32::to_bits)
    }
}

struct SeenBody {
    /// What both peers order bodies by: the body's entity on the server.
    order: u64,
    /// At the seen time, sub-tick: what the command aims at.
    seen: Isometry3d,
    /// At [`mover_times`]: what the move collides with.
    current: Isometry3d,
    next: Isometry3d,
    colliders: Vec<SeenCollider>,
}

/// The world one command collides with.
pub(crate) struct Seen<'a, 'w, 's> {
    world: &'a SeenWorld<'w, 's>,
    bodies: Vec<SeenBody>,
}

impl<'w, 's> SeenWorld<'w, 's> {
    /// The world a command that saw `time` collides with.
    pub(crate) fn at(&self, time: RemoteRenderTime) -> Seen<'_, 'w, 's> {
        let (current_time, next_time) = mover_times(time);
        let mut bodies = self
            .bodies
            .iter()
            .filter_map(|(entity, history)| {
                let pose = |body: BodyPose| Isometry3d::new(body.position, body.rotation);
                let seen = pose(self.body_at_in(history, time)?);
                let colliders = self.colliders_of(entity);
                if colliders.is_empty() {
                    return None;
                }
                let current = self.body_at_in(history, current_time).map_or(seen, pose);
                let next = self.body_at_in(history, next_time).map_or(current, pose);
                Some(SeenBody {
                    order: self.server_entity(entity).to_bits(),
                    seen,
                    current,
                    next,
                    colliders,
                })
            })
            .collect::<Vec<_>>();
        // The order movers meet the step in can change its result (of two at one distance, the
        // first is the ground ridden and the hit reported), and entities differ between peers:
        // sort by the server's entity, which they share.
        bodies.sort_unstable_by_key(|body| body.order);
        Seen {
            world: self,
            bodies,
        }
    }

    /// `entity`'s entity on the server: replicon's map on a client; the entity itself on the
    /// server, or for a body no server sent, which no other peer has to agree on.
    fn server_entity(&self, entity: Entity) -> Entity {
        self.entity_map
            .as_ref()
            .and_then(|map| map.to_server().get(&entity).copied())
            .unwrap_or(entity)
    }

    fn body_at_in(
        &self,
        history: &History<BodySnapshot>,
        time: RemoteRenderTime,
    ) -> Option<BodyPose> {
        body_at(history, time, *self.rate)
    }

    /// The colliders a body with snapshot history moves, in its frame. Only the ones on
    /// [`WORLD_COLLISION_LAYER`], in an order every peer gives them ([`SeenCollider::order`]),
    /// whatever order each attached them in.
    fn colliders_of(&self, entity: Entity) -> Vec<SeenCollider> {
        let parts: Vec<Entity> = match self.body_colliders.get(entity) {
            Ok(colliders) => colliders.iter().collect(),
            Err(_) => vec![entity],
        };
        let mut colliders: Vec<SeenCollider> = parts
            .into_iter()
            .filter_map(|part| {
                let (collider, local, layers) = self.colliders.get(part).ok()?;
                let layers = layers.copied().unwrap_or_default();
                if (layers.memberships & WORLD_COLLISION_LAYER) == LayerMask::NONE {
                    return None;
                }
                let local = local.map_or(Isometry3d::IDENTITY, |local| {
                    Isometry3d::new(local.translation, local.rotation.0)
                });
                Some(SeenCollider {
                    entity: part,
                    collider: collider.clone(),
                    local,
                })
            })
            .collect();
        if colliders.len() > 1 {
            colliders.sort_by_cached_key(SeenCollider::order);
        }
        colliders
    }
}

impl<'w, 's> Seen<'_, 'w, 's> {
    /// The live level, for the move.
    pub(crate) fn level(&self) -> &LiveWorld<'w, 's> {
        &self.world.level
    }

    /// Every moving collider within `RELEVANT` of `viewer`, from its body's pose at the seen
    /// tick to its pose a tick on (`mover_times`).
    pub(crate) fn movers(&self, viewer: Vec3) -> Vec<Mover> {
        let near = |aabb: ColliderAabb| {
            viewer.distance_squared(viewer.clamp(aabb.min, aabb.max)) <= RELEVANT * RELEVANT
        };
        let mut movers = Vec::new();
        for body in &self.bodies {
            for part in &body.colliders {
                let current = body.current * part.local;
                if near(
                    part.collider
                        .aabb(current.translation.into(), current.rotation),
                ) {
                    movers.push(Mover {
                        entity: part.entity,
                        collider: part.collider.clone(),
                        current,
                        next: body.next * part.local,
                    });
                }
            }
        }
        movers
    }

    /// Every collider the command saw that the live level doesn't have right: each body where
    /// `pose` puts it. With its own entity.
    fn for_each(
        &self,
        pose: impl Fn(&SeenBody) -> Isometry3d,
        mut f: impl FnMut(Entity, &Collider, Isometry3d),
    ) {
        for body in &self.bodies {
            for part in &body.colliders {
                f(part.entity, &part.collider, pose(body) * part.local);
            }
        }
    }

    /// A live collider the command decides some other way: one a body with history moves.
    fn replaced(&self, entity: Entity) -> bool {
        self.bodies
            .iter()
            .any(|body| body.colliders.iter().any(|part| part.entity == entity))
    }

    /// A ray through the world at the seen time: bodies where the player saw them, sub-tick.
    pub(crate) fn cast_ray(
        &self,
        origin: Vec3,
        direction: Dir3,
        max_distance: f32,
        solid: bool,
        filter: &SpatialQueryFilter,
    ) -> Option<RayHitData> {
        let mut closest = self
            .world
            .level
            .move_and_slide
            .spatial_query
            .cast_ray_predicate(origin, direction, max_distance, solid, filter, &|entity| {
                !self.replaced(entity)
            });
        self.for_each(
            |body| body.seen,
            |entity, collider, pose| {
                if filter.excluded_entities.contains(&entity) {
                    return;
                }
                let reach = closest.map_or(max_distance, |hit| hit.distance);
                if let Some((distance, normal)) = collider.cast_ray(
                    Vec3::from(pose.translation),
                    pose.rotation,
                    origin,
                    *direction,
                    reach,
                    solid,
                ) && distance < reach
                {
                    closest = Some(RayHitData {
                        entity,
                        distance,
                        normal,
                    });
                }
            },
        );
        closest
    }
}

/// The world a step's rays meet ([`Step::world`](crate::Step::world)): the world the move
/// stepped in, at the command's seen time, so a shot meets moving bodies at the exact pose both
/// peers sampled. Or, for presentation helpers (cursor aim traces), the live world.
pub struct StepWorld<'a, 'b, 'w, 's>(Rays<'a, 'b, 'w, 's>);

enum Rays<'a, 'b, 'w, 's> {
    Seen(&'a Seen<'b, 'w, 's>),
    Live(&'a SpatialQuery<'w, 's>),
}

impl<'a, 'b, 'w, 's> StepWorld<'a, 'b, 'w, 's> {
    pub(crate) fn seen(seen: &'a Seen<'b, 'w, 's>) -> Self {
        Self(Rays::Seen(seen))
    }

    /// The live world, for presentation (cursor aim traces) with the step's ray rules.
    pub fn live(live: &'a SpatialQuery<'w, 's>) -> Self {
        Self(Rays::Live(live))
    }

    /// The nearest hit on [`WORLD_COLLISION_LAYER`] along the ray, within `max_distance`;
    /// `solid` as avian's.
    pub fn cast_ray(
        &self,
        origin: Vec3,
        direction: Dir3,
        max_distance: f32,
        solid: bool,
    ) -> Option<RayHitData> {
        let filter = SpatialQueryFilter::from_mask(WORLD_COLLISION_LAYER);
        match self.0 {
            Rays::Seen(seen) => seen.cast_ray(origin, direction, max_distance, solid, &filter),
            Rays::Live(live) => live.cast_ray(origin, direction, max_distance, solid, &filter),
        }
    }
}

/// A fire's origin may sit this far (m) from the point derived from the move,
/// plus [`FIRE_ORIGIN_SEGMENTS`] of the move itself: the drawn body runs a tick
/// behind that point at speed, and this slack covers a sudden stop or a correction.
const FIRE_ORIGIN_SLACK: f32 = 1.0;
const FIRE_ORIGIN_SEGMENTS: f32 = 2.0;
/// Never further than this, whatever the move (a respawn's is a whole map).
const FIRE_ORIGIN_MAX: f32 = 4.0;

/// The body position a fire shoots from: where the player says they were drawn, if within
/// reach of where the move puts them and no wall stands between; bounded to that reach if
/// further, the move's own point if walled off. Called in the step, so both peers get the same
/// bits and a client can't shoot from somewhere it couldn't have been. The wall test runs at
/// [`eye_height`](crate::PlayerShape::eye_height) above the body.
pub fn fire_position(
    fire: &SubtickFire,
    previous_position: Vec3,
    position: Vec3,
    eye_height: f32,
    world: &StepWorld,
) -> Vec3 {
    let derived = previous_position.lerp(position, fire.frac);
    let Some(origin) = fire.origin else {
        return derived;
    };
    let reach = (FIRE_ORIGIN_SLACK + FIRE_ORIGIN_SEGMENTS * previous_position.distance(position))
        .min(FIRE_ORIGIN_MAX);
    let offset = (origin - derived).clamp_length_max(reach);
    let walled = Dir3::new(offset).is_ok_and(|direction| {
        world
            .cast_ray(
                derived + Vec3::Y * eye_height,
                direction,
                offset.length(),
                true,
            )
            .is_some()
    });
    if walled { derived } else { derived + offset }
}

#[cfg(test)]
mod tests {
    use bevy::{ecs::system::RunSystemOnce, state::app::StatesPlugin};
    use bevy_ahoy::{CharacterController, CharacterLook, input::AhoyInputPlugin, moves::Ground};
    use bevy_replicon::{prelude::*, test_app::ServerTestAppExt};

    use super::*;
    use crate::{
        KccSchedule, NetAhoyProtocolPlugin, PLAYER_COLLISION_LAYER,
        events::Outbox,
        game::{
            PlayerState,
            test_game::{TestEvent, TestGame},
        },
        protocol::{PlayerId, PlayerShape, UserCmd},
        stepper::Stepper,
    };

    /// A peer that can step a player, with replicon's server or client.
    fn peer(server: bool) -> App {
        use bevy_ahoy::AhoyPlugins;

        let mut app = App::new();
        let replicon = if server {
            RepliconPlugins.set(bevy_replicon::server::ServerPlugin::new(PostUpdate))
        } else {
            RepliconPlugins.build()
        };
        app.add_plugins((
            MinimalPlugins,
            StatesPlugin,
            TransformPlugin,
            bevy::diagnostic::DiagnosticsPlugin,
            PhysicsPlugins::default(),
            AhoyPlugins::new(KccSchedule)
                .build()
                .disable::<AhoyInputPlugin>(),
            replicon,
            NetAhoyProtocolPlugin::<TestGame>::default(),
        ))
        .add_message::<TestEvent>()
        .init_resource::<avian3d::spatial_query::SpatialQueryDiagnostics>()
        .init_resource::<avian3d::collider_tree::ColliderTreeDiagnostics>()
        .init_resource::<avian3d::collision::CollisionDiagnostics>()
        .init_resource::<avian3d::dynamics::solver::SolverDiagnostics>();
        app.finish();
        app
    }

    /// Platform `side`'s pose at `tick`: two 2 m boxes meeting at x = 0, their tops at y = 0,
    /// the one at -x sliding +z and the one at +x sliding -z.
    fn platform_at(side: f32, tick: u64) -> Vec3 {
        Vec3::new(side, -0.1, -side * 0.2 * tick as f32)
    }

    /// Makes `body` platform `side` on this peer: its collider, and a history it has been
    /// moving through since tick 1.
    fn make_platform(app: &mut App, body: Entity, side: f32) {
        let rate = *app.world().resource::<TickRate>();
        let mut history = History::new(rate);
        for tick in 1..=4 {
            history.record(
                tick,
                BodySnapshot {
                    tick,
                    position: platform_at(side, tick),
                    rotation: Quat::IDENTITY,
                    ..default()
                },
            );
        }
        app.world_mut().entity_mut(body).insert((
            Transform::from_translation(platform_at(side, 1)),
            RigidBody::Kinematic,
            Collider::cuboid(2.0, 0.2, 2.0),
            CollisionLayers::new(WORLD_COLLISION_LAYER, LayerMask::ALL),
            history,
        ));
    }

    /// A player standing on the seam between the two platforms, one command stepped; where it
    /// ends up.
    fn step_on_the_seam(app: &mut App) -> PlayerState<TestGame> {
        let shape = *app.world().resource::<PlayerShape>();
        let mut state = PlayerState::<TestGame>::at(Vec3::new(0.0, shape.half_height + 0.01, 0.0));
        state.moves.ground = Some(Ground {
            normal: Vec3::Y,
            friction: 0.5,
        });
        let player = app
            .world_mut()
            .spawn((
                PlayerId(1),
                state,
                CharacterLook::default(),
                CharacterController {
                    filter: SpatialQueryFilter::from_mask(WORLD_COLLISION_LAYER),
                    ..default()
                },
                shape.collider(),
                CollisionLayers::new(PLAYER_COLLISION_LAYER, LayerMask::ALL),
                Transform::from_translation(state.moves.position),
            ))
            .id();
        // Let avian and Ahoy set up the colliders and the hulls.
        for _ in 0..4 {
            app.update();
        }
        let command = UserCmd {
            sequence: 1,
            seen_server_tick: 1,
            ..default()
        };
        app.world_mut()
            .run_system_once(move |mut stepper: Stepper<TestGame>| {
                stepper
                    .player_move(player, &mut Outbox::default(), command, 0, default())
                    .unwrap();
            })
            .unwrap();
        *app.world().get::<PlayerState<TestGame>>(player).unwrap()
    }

    /// A player on the seam of two movers stands on whichever the step meets first: the two
    /// are the same distance below it. The server spawned the -x one first, and its client got
    /// them the other way round; both peers step the player onto the same one, the server's
    /// first.
    #[test]
    fn every_peer_meets_the_movers_in_the_servers_order() {
        let mut server = peer(true);
        let mut client = peer(false);
        server.connect_client(&mut client);

        // The -x one is the server's first. The client spawns them as they come, one
        // replication after the other, and the -x one comes first: a fresh world numbers its
        // entities the other way round from their order.
        let mut bodies = [(); 2].map(|()| server.world_mut().spawn(BodySnapshot::default()).id());
        bodies.sort_by_key(|body| body.to_bits());
        let [minus, plus] = bodies;
        let exchange = |server: &mut App, client: &mut App| {
            server.update();
            server.exchange_with_client(client);
            client.update();
        };
        for body in [minus, plus] {
            server.world_mut().entity_mut(body).insert(Replicated);
            exchange(&mut server, &mut client);
        }
        let map = client.world().resource::<ServerEntityMap>().to_client();
        let (minus_there, plus_there) = (map[&minus], map[&plus]);
        assert!(
            minus_there.to_bits() > plus_there.to_bits(),
            "the client's entities are in the other order"
        );

        for (app, minus, plus) in [
            (&mut server, minus, plus),
            (&mut client, minus_there, plus_there),
        ] {
            make_platform(app, minus, -1.0);
            make_platform(app, plus, 1.0);
        }
        let on_server = step_on_the_seam(&mut server);
        let on_client = step_on_the_seam(&mut client);
        assert!(
            on_server.moves.position.z > 0.01,
            "the server's first, the -x one, carried it +z: {:?}",
            on_server.moves
        );
        assert_eq!(on_client, on_server);
    }
}
