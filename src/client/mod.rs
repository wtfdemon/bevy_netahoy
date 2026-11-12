//! Lives on the player's machine: guesses where they're going (prediction),
//! fixes the guess when the server disagrees (reconciliation), draws everyone.

mod command_clock;
mod input;
mod predict;
mod reconcile;
mod remote_clock;
mod remotes;
#[cfg(test)]
mod snapshot_sim;

use std::marker::PhantomData;

use avian3d::prelude::{
    Collider, CollisionLayers, LinearVelocity, Position, TransformEasingSystems,
};
use bevy::{
    app::{RunFixedMainLoop, RunFixedMainLoopSystems},
    prelude::*,
};
use bevy_ahoy::{CharacterController, CharacterLook};
use bevy_replicon::prelude::*;

use crate::{
    demo::DemoPlayback,
    events::Outbox,
    game::{PlayerGame, PlayerState},
    history::HoldHistories,
    protocol::*,
    reconnect::SessionResets,
    stepper::{PlayerComponents, show},
};

use command_clock::{reset_command_clock, steer_command_clock};
use input::latch_fire_frame;
use predict::{
    apply_link_profile, drive_prediction_and_send_input, record_prediction_state, reset_prediction,
};
use reconcile::reconcile_local_prediction;
use remote_clock::{advance_server_clock_fixed, reset_server_clock, update_server_clock};
use remotes::{ease_remote_players_from_history, interpolate_remote_players};

#[cfg(test)]
pub(crate) use command_clock::CommandClockAction;
pub(crate) use command_clock::INPUT_LEAD_BAND;
#[cfg(test)]
pub(crate) use command_clock::{COMMAND_CLOCK_MAX_SLOWDOWN, COMMAND_CLOCK_MAX_SPEEDUP};
pub use command_clock::{CommandClock, CommandClockStats};
use command_clock::{pace_fixed_clock, settle_fixed_clock};
pub use input::{ClientInput, PresentedFrame};
pub(crate) use predict::ClientInputState;
#[cfg(test)]
pub(crate) use predict::DATAGRAM_USERCMD_BACKUP_SECONDS;
pub use predict::LinkProfile;
pub(crate) use predict::{LocalCommandHistory, PredictionHistory};
#[cfg(test)]
pub(crate) use reconcile::max_replay_commands;
pub use reconcile::{CorrectionMode, PendingCorrection, PredictionStatus};
pub use remote_clock::{RemoteClock, RemoteClockStats};
pub use remotes::{RemotePlaybackStatus, RemoteSampledFrame};

/// The client plugin's system sets, for a game to order its own systems against.
#[derive(SystemSet, Debug, Clone, Copy, Hash, PartialEq, Eq)]
pub enum NetAhoyClientSystems {
    /// Rewind + replay against fresh server snapshots (`FixedPreUpdate`).
    Reconcile,
    /// Build this tick's user command, predict it, send it (`FixedPreUpdate`).
    Predict,
    /// Remote pose sampling (`FixedLast`).
    Interpolate,
    /// Where the game writes [`ClientInput`] (`RunFixedMainLoop`, `BeforeFixedMainLoop`).
    /// It runs before the input clock paces the frame, so `overstep_fraction()` is still the
    /// fraction the last frame drew at, as a [`SubtickFire::frac`] wants; after the pace it
    /// would include this frame's catch-up, past 1 on a catch-up frame.
    GatherInput,
}

/// The client half for the game `G`: predicts the local player, reconciles it with the
/// server, draws the remote ones.
pub struct NetAhoyClientPlugin<G>(PhantomData<G>);

impl<G> Default for NetAhoyClientPlugin<G> {
    fn default() -> Self {
        Self(PhantomData)
    }
}

impl<G: PlayerGame> Plugin for NetAhoyClientPlugin<G> {
    fn build(&self, app: &mut App) {
        app.add_plugins(crate::reconnect::NetAhoyClientSessionPlugin)
            .init_resource::<SessionResets>();
        app.world_mut()
            .resource_mut::<SessionResets>()
            .0
            .push(reset_prediction::<G>);
        app.register_required_components::<PredictedPlayer, PlayerState<G>>()
            .init_resource::<OwnPlayerId>()
            .add_systems(Update, apply_link_profile)
            .init_resource::<ClientInput>()
            .init_resource::<ClientInputState>()
            .init_resource::<PresentedFrame>()
            // The local step's events, for the game to present without waiting on the server.
            .add_message::<G::Event>()
            .init_resource::<Outbox<G::Event>>()
            .add_observer(set_own_player_id)
            .add_observer(admit_prediction::<G>)
            .add_systems(
                OnEnter(ClientState::Connected),
                (reset_server_clock, reset_command_clock).chain(),
            )
            .add_systems(PreUpdate, tag_remote_players.after(ClientSystems::Receive))
            // Freeze one remote-render time before prediction/replay. Every
            // fixed step and every Update presentation samples this same time.
            .add_systems(
                RunFixedMainLoop,
                (
                    update_server_clock::<G>,
                    pace_fixed_clock.run_if(
                        in_state(ClientState::Connected)
                            .and_then(not(resource_exists::<DemoPlayback>)),
                    ),
                )
                    .in_set(RunFixedMainLoopSystems::BeforeFixedMainLoop),
            )
            .configure_sets(
                RunFixedMainLoop,
                HoldHistories
                    .in_set(RunFixedMainLoopSystems::BeforeFixedMainLoop)
                    .after(update_server_clock::<G>),
            )
            .configure_sets(
                RunFixedMainLoop,
                NetAhoyClientSystems::GatherInput
                    .in_set(RunFixedMainLoopSystems::BeforeFixedMainLoop)
                    .before(pace_fixed_clock),
            )
            // Same window avian's transform easing uses, so every Update
            // system reads this frame's final remote pose. The overstep is
            // settled before anything eases by it: the own body, the remotes
            // and the next frame's fire fraction all read one value.
            .add_systems(
                RunFixedMainLoop,
                (
                    settle_fixed_clock
                        .run_if(
                            in_state(ClientState::Connected)
                                .and_then(not(resource_exists::<DemoPlayback>)),
                        )
                        .before(TransformEasingSystems::Ease),
                    latch_fire_frame,
                    ease_remote_players_from_history.after(TransformEasingSystems::Ease),
                )
                    .chain()
                    .in_set(RunFixedMainLoopSystems::AfterFixedMainLoop),
            )
            .configure_sets(
                FixedPreUpdate,
                (
                    NetAhoyClientSystems::Reconcile,
                    NetAhoyClientSystems::Predict,
                )
                    .chain(),
            )
            .add_systems(FixedFirst, advance_server_clock_fixed)
            .add_systems(
                FixedPreUpdate,
                reconcile_local_prediction::<G>
                    .run_if(in_state(ClientState::Connected))
                    .in_set(NetAhoyClientSystems::Reconcile),
            )
            .add_systems(
                FixedPreUpdate,
                steer_command_clock::<G>
                    .run_if(
                        in_state(ClientState::Connected)
                            .and_then(not(resource_exists::<DemoPlayback>)),
                    )
                    .after(NetAhoyClientSystems::Reconcile)
                    .before(NetAhoyClientSystems::Predict),
            )
            .add_systems(
                FixedPreUpdate,
                (
                    drive_prediction_and_send_input::<G>
                        .run_if(in_state(ClientState::Connected))
                        .in_set(NetAhoyClientSystems::Predict),
                    // The camera and presentation read the predicted player by its
                    // components: written from its state once it has stepped.
                    crate::stepper::write_player_components::<G>
                        .after(NetAhoyClientSystems::Predict),
                ),
            )
            .add_systems(
                FixedLast,
                (
                    record_prediction_state::<G>,
                    interpolate_remote_players
                        .in_set(NetAhoyClientSystems::Interpolate)
                        .before(TransformEasingSystems::UpdateEnd),
                ),
            )
            .add_systems(
                Update,
                (
                    mark_own_player,
                    cleanup_predicted_players,
                    cleanup_own_visuals,
                    cleanup_remote_visuals,
                )
                    .chain(),
            );
    }

    // The clocks and histories run at the tick rate, which the protocol plugin may set after
    // this one builds; Ahoy's plugins may come after it too.
    fn finish(&self, app: &mut App) {
        crate::stepper::assert_kcc_unscheduled(app, "NetAhoyClientPlugin");
        let rate = fixed_tick_rate(app.world(), "NetAhoyClientPlugin");
        let world = app.world_mut();
        world.get_resource_or_insert_with(|| RemoteClock::new(rate));
        world.get_resource_or_insert_with(|| CommandClock::new(rate));
        world.get_resource_or_insert_with(|| LinkProfile::new(rate));
        world.get_resource_or_insert_with(|| PredictionHistory::<G>::new(rate));
        world.get_resource_or_insert_with(|| LocalCommandHistory::new(rate));
    }
}

/// The id of this client's own player, the one the server gave it: `None` until the server
/// accepts the join, after a disconnect, and throughout demo playback.
#[derive(Resource, Default, Clone, Copy, Debug)]
pub struct OwnPlayerId(pub Option<PlayerId>);

impl OwnPlayerId {
    /// Whether this client was given `player_id`.
    pub fn is_assigned_to(self, player_id: PlayerId) -> bool {
        self.0 == Some(player_id)
    }
}

/// On a client, the [`ServerPlayer`] that is this client's own: the server's truth about it,
/// which reconciliation judges the [`PredictedPlayer`] against. The plugin marks it once the
/// server says which player this client owns ([`OwnPlayerId`]); every other one is a
/// [`RemotePlayer`].
#[derive(Component)]
pub struct OwnPlayer;

/// This client's player as it predicts it: stepped on each command as the command is sent,
/// ahead of the server. What the camera and gameplay treat as the player.
///
/// The game spawns it when its [`OwnPlayer`] appears, with the game's
/// [`CharacterController`] and collider. The plugin gives it the rest: the player's
/// [`PlayerId`], its [`PlayerState`] from the server's, and the components written from that
/// state. Spawned without a `CharacterController` it is an error: nothing could step it. So is
/// a collider on [`WORLD_COLLISION_LAYER`] (avian's default layers): every step would meet it
/// as level geometry. Players go on [`PLAYER_COLLISION_LAYER`].
#[derive(Component)]
#[require(
    PendingCorrection,
    PredictionStatus,
    Transform,
    Position,
    LinearVelocity,
    CharacterLook
)]
pub struct PredictedPlayer {
    /// The player it predicts: its [`OwnPlayer`].
    pub server_entity: Entity,
}

/// What draws this client's own player, if the game draws it apart from the
/// [`PredictedPlayer`]. netahoy only snaps the prediction and reports each correction in its
/// [`PendingCorrection`]; the game owns this entity's transform (where it eases from
/// and to, and however it chooses to hide corrections). It goes when its prediction goes.
#[derive(Component)]
pub struct OwnVisual {
    /// The [`PredictedPlayer`] it draws.
    pub prediction_entity: Entity,
}

/// What draws a [`RemotePlayer`]: the game spawns it, and netahoy moves its `Transform` every
/// frame to where the player's history has it at the remote render time, easing across
/// corrections. Add no transform interpolation of its own: the two would fight. It goes when its
/// player goes.
#[derive(Component)]
#[require(RemotePlaybackStatus)]
pub struct RemoteVisual {
    /// The player it draws: its [`RemotePlayer`].
    pub server_entity: Entity,
    /// That player's id.
    pub player_id: PlayerId,
}

/// On a client, every [`ServerPlayer`] but its own: drawn from its
/// [`History`](crate::History), which lag compensation samples too. The plugin marks it the
/// frame replication spawns it.
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct RemotePlayer;

fn set_own_player_id(
    accepted: On<JoinAccepted>,
    mut local: ResMut<OwnPlayerId>,
    playback: Option<ResMut<DemoPlayback>>,
) {
    // In playback the recorded JoinAccepted replays, but nobody is local: OwnPlayerId stays
    // unset so no prediction engages.
    if let Some(mut playback) = playback {
        playback.recorded_player = Some(accepted.player_id);
        info!("demo recorded by player {}", accepted.player_id.0);
        return;
    }

    local.0 = Some(accepted.player_id);
    info!("joined as player {}", accepted.player_id.0);
}

/// Makes a new [`PredictedPlayer`] the server's player: its id, its state and the
/// components written from it.
fn admit_prediction<G: PlayerGame>(
    add: On<Add, PredictedPlayer>,
    mut commands: Commands,
    mut predictions: Query<(
        &PredictedPlayer,
        Has<CharacterController>,
        (Has<Collider>, Option<&CollisionLayers>),
        &mut PlayerState<G>,
        PlayerComponents,
    )>,
    server_players: Query<
        (
            &PlayerId,
            Option<&OwnerSnapshot<G>>,
            Option<&PlayerSnapshot>,
        ),
        Without<PredictedPlayer>,
    >,
) -> Result {
    let entity = add.event_target();
    let (prediction, controlled, (collider, layers), mut state, components) =
        predictions.get_mut(entity)?;
    if !controlled {
        return Err(format!(
            "PredictedPlayer {entity} has no CharacterController: spawn it with the game's \
             controller and collider"
        )
        .into());
    }
    if let Some(error) = player_layers_error(&format!("PredictedPlayer {entity}"), collider, layers)
    {
        return Err(error.into());
    }
    let server_entity = prediction.server_entity;
    let (&player_id, snapshot, pose) = server_players.get(server_entity).map_err(|_| {
        format!("PredictedPlayer {entity}: its server entity {server_entity} has no PlayerId")
    })?;
    *state = match (snapshot, pose) {
        (Some(snapshot), _) => PlayerState {
            moves: snapshot.moves,
            game: snapshot.game,
        },
        (None, Some(pose)) => PlayerState::at(pose.position),
        (None, None) => default(),
    };
    show(&state.moves, components);
    commands.entity(entity).insert(player_id);
    Ok(())
}

fn mark_own_player(
    mut commands: Commands,
    local: Res<OwnPlayerId>,
    players: Query<(Entity, &PlayerId), (With<ServerPlayer>, Without<OwnPlayer>)>,
) {
    let Some(local_id) = local.0 else {
        return;
    };

    for (entity, player_id) in &players {
        if *player_id == local_id {
            commands.entity(entity).insert(OwnPlayer);
        }
    }
}

/// Marks each player that isn't this client's a [`RemotePlayer`], in the frame it replicates.
fn tag_remote_players(
    mut commands: Commands,
    local: Res<OwnPlayerId>,
    playback: Option<Res<DemoPlayback>>,
    players: Query<
        (Entity, &PlayerId),
        (
            With<ServerPlayer>,
            Without<OwnPlayer>,
            Without<RemotePlayer>,
        ),
    >,
) {
    // In demo playback nobody is local: every player interpolates.
    let local_id = if playback.is_some() {
        None
    } else {
        let Some(local_id) = local.0 else {
            return;
        };
        Some(local_id)
    };

    for (entity, player_id) in &players {
        if Some(*player_id) != local_id {
            commands.entity(entity).insert(RemotePlayer);
        }
    }
}

fn cleanup_predicted_players(
    mut commands: Commands,
    predictions: Query<(Entity, &PredictedPlayer)>,
    local_players: Query<(), With<OwnPlayer>>,
) {
    for (entity, prediction) in &predictions {
        if local_players.get(prediction.server_entity).is_err() {
            commands.entity(entity).despawn();
        }
    }
}

fn cleanup_own_visuals(
    mut commands: Commands,
    presentations: Query<(Entity, &OwnVisual)>,
    predictions: Query<(), With<PredictedPlayer>>,
) {
    for (entity, presentation) in &presentations {
        if predictions.get(presentation.prediction_entity).is_err() {
            commands.entity(entity).despawn();
        }
    }
}

fn cleanup_remote_visuals(
    mut commands: Commands,
    visuals: Query<(Entity, &RemoteVisual)>,
    remotes: Query<(), With<RemotePlayer>>,
) {
    for (entity, visual) in &visuals {
        if remotes.get(visual.server_entity).is_err() {
            commands.entity(entity).despawn();
        }
    }
}

/// A client app stepping its player at `rate` in an empty level, with only the resources
/// prediction and reconciliation read: for this module's tests, which run those systems by hand.
#[cfg(test)]
fn stepping_app<G: PlayerGame>(rate: TickRate) -> App {
    use avian3d::prelude::PhysicsPlugins;
    use bevy_ahoy::{AhoyPlugins, input::AhoyInputPlugin};

    use crate::KccSchedule;

    let mut app = App::new();
    app.add_plugins((
        MinimalPlugins,
        TransformPlugin,
        bevy::diagnostic::DiagnosticsPlugin,
        PhysicsPlugins::default(),
        AhoyPlugins::new(KccSchedule)
            .build()
            .disable::<AhoyInputPlugin>(),
    ))
    .insert_resource(rate)
    .insert_resource(PlayerShape::default())
    .insert_resource(PredictionHistory::<G>::new(rate))
    .insert_resource(LocalCommandHistory::new(rate))
    .init_resource::<ClientInputState>()
    .add_message::<G::Event>()
    .init_resource::<Outbox<G::Event>>()
    .init_resource::<avian3d::spatial_query::SpatialQueryDiagnostics>()
    .init_resource::<avian3d::collider_tree::ColliderTreeDiagnostics>()
    .init_resource::<avian3d::collision::CollisionDiagnostics>()
    .init_resource::<avian3d::dynamics::solver::SolverDiagnostics>();
    app
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::game::test_game::TestGame;

    /// The client plugin may come before the protocol plugin: its clocks run at the protocol's
    /// rate either way.
    #[test]
    fn the_clocks_run_at_the_protocol_rate_whatever_the_plugin_order() {
        let mut app = App::new();
        app.add_plugins((
            MinimalPlugins,
            bevy::state::app::StatesPlugin,
            RepliconPlugins,
            NetAhoyClientPlugin::<TestGame>::default(),
            NetAhoyProtocolPlugin::<TestGame>::new(TickRate::new(60)),
        ));
        app.finish();
        assert_eq!(app.world().resource::<RemoteClock>().tick_hz, 60.0);
        assert_eq!(app.world().resource::<CommandClock>().lead_window, 60);
    }

    /// A client app with the protocol and the client plugin, for the game `TestGame`.
    fn client_app() -> App {
        let mut app = App::new();
        app.add_plugins((
            MinimalPlugins,
            bevy::state::app::StatesPlugin,
            RepliconPlugins,
            NetAhoyProtocolPlugin::<TestGame>::default(),
            NetAhoyClientPlugin::<TestGame>::default(),
        ));
        app.finish();
        app
    }

    /// The game spawns the prediction with its controller; the plugin gives it the server
    /// player's id and state, and shows it there.
    #[test]
    fn a_prediction_takes_the_server_players_id_and_state() {
        let mut app = client_app();
        let mut moves = PlayerState::<TestGame>::at(Vec3::new(1.0, 2.0, 3.0)).moves;
        moves.velocity = Vec3::X;
        let server_entity = app
            .world_mut()
            .spawn((
                PlayerId(3),
                ServerPlayer,
                OwnerSnapshot::<TestGame> { moves, ..default() },
            ))
            .id();
        let prediction = app
            .world_mut()
            .spawn((
                PredictedPlayer { server_entity },
                CharacterController::default(),
            ))
            .id();
        let prediction = app.world().entity(prediction);
        assert_eq!(prediction.get::<PlayerId>(), Some(&PlayerId(3)));
        assert_eq!(
            prediction.get::<PlayerState<TestGame>>().unwrap().moves,
            moves
        );
        assert_eq!(
            prediction.get::<Transform>().unwrap().translation,
            moves.position
        );
        assert_eq!(prediction.get::<Position>().unwrap().0, moves.position);
        assert_eq!(prediction.get::<LinearVelocity>().unwrap().0, Vec3::X);
    }

    /// Without a controller the stepper couldn't move the prediction.
    #[test]
    #[should_panic(expected = "has no CharacterController")]
    fn a_prediction_without_a_controller_is_an_error_at_spawn() {
        let mut app = client_app();
        let server_entity = app.world_mut().spawn(PlayerId(3)).id();
        app.world_mut().spawn(PredictedPlayer { server_entity });
    }

    /// The prediction steps against the world layer: a collider of its own on it would be in
    /// its way.
    #[test]
    #[should_panic(expected = "is a member of WORLD_COLLISION_LAYER")]
    fn a_prediction_collider_on_the_world_layer_is_an_error_at_spawn() {
        let mut app = client_app();
        let server_entity = app.world_mut().spawn(PlayerId(3)).id();
        app.world_mut().spawn((
            PredictedPlayer { server_entity },
            CharacterController::default(),
            PlayerShape::default().collider(),
            CollisionLayers::new(WORLD_COLLISION_LAYER, avian3d::prelude::LayerMask::ALL),
        ));
    }

    /// At 20 Hz the plugin sizes the client's histories to 256 ticks, the datagram tail to 10 and
    /// the behind resync to 6, and three times as many at 60 Hz, but the datagram tail stops at
    /// its 10.
    #[test]
    fn the_client_windows_hold_their_seconds_up_to_their_caps() {
        for (hz, scale) in [(20, 1), (60, 3)] {
            let mut app = App::new();
            app.add_plugins((
                MinimalPlugins,
                bevy::state::app::StatesPlugin,
                RepliconPlugins,
                NetAhoyProtocolPlugin::<TestGame>::new(TickRate::new(hz)),
                NetAhoyClientPlugin::<TestGame>::default(),
            ));
            app.finish();
            let world = app.world();
            assert_eq!(
                world.resource::<PredictionHistory<TestGame>>().capacity,
                256 * scale
            );
            assert_eq!(
                world.resource::<LocalCommandHistory>().capacity,
                256 * scale
            );
            assert_eq!(world.resource::<LinkProfile>().datagram_backup_cap, 10);
            assert_eq!(
                world.resource::<CommandClock>().resync_behind,
                6 * scale as i32
            );
        }
    }
}
