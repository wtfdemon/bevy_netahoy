//! Who owns a player, and the filter that sends its reconcile snapshot to that client only.

use std::marker::PhantomData;

use bevy::prelude::*;
use bevy_replicon::prelude::*;

use crate::{game::PlayerGame, protocol::*};

/// The client connection that owns this player: the only one sent its [`OwnerSnapshot`].
///
/// The player lives as long as that connection: the server despawns it when the client's
/// `ConnectedClient` goes, by a disconnect, a timeout or a refusal, and at once if it is spawned
/// for anything but an authorized client.
///
/// Immutable, since it decides who gets the snapshot. A player without one sends it to
/// everyone, so a bot carries `PlayerOwner(Entity::PLACEHOLDER)`: owned by nobody, its snapshot
/// sent to nobody.
///
/// It brings replicon's `Replicated` and [`ServerPlayer`], which every peer finds players by.
/// With [`NetAhoyServerPlugin`](crate::NetAhoyServerPlugin) it brings the rest of a server
/// player: its input timeline, and each at its default unless spawned with it,
/// [`QueuedUserCmds`](crate::QueuedUserCmds), [`OwnerSnapshot`],
/// [`PlayerState`](crate::PlayerState), and the components written from that
/// state (`Transform`, `Position`, `LinearVelocity`, `CharacterLook`); and it tells the owning
/// client which player is its own ([`OwnPlayerId`](crate::OwnPlayerId)).
///
/// Spawning it is an error unless the player has a [`PlayerId`]; a `CharacterController` if a
/// client owns it (a bot needs one only if the game queues commands for it); and any collider
/// off [`WORLD_COLLISION_LAYER`], where every step would meet it as level geometry (avian's
/// default layers are on it): players go on [`PLAYER_COLLISION_LAYER`].
#[derive(Component, Clone, Copy, Debug, PartialEq, Eq)]
#[component(immutable)]
#[require(Replicated, ServerPlayer)]
pub struct PlayerOwner(pub Entity);

/// Sends each player's [`OwnerSnapshot`] to its [`PlayerOwner`] only; everyone gets the pose in
/// [`PlayerSnapshot`]. A player without a `PlayerOwner` sends its snapshot to everyone.
pub struct OwnerSnapshotPlugin<G>(PhantomData<G>);

impl<G> Default for OwnerSnapshotPlugin<G> {
    fn default() -> Self {
        Self(PhantomData)
    }
}

impl<G: PlayerGame> Plugin for OwnerSnapshotPlugin<G> {
    fn build(&self, app: &mut App) {
        app.add_visibility_filter::<SnapshotOwner<G>>()
            .add_observer(mirror_player_owner::<G>)
            .add_observer(unmirror_player_owner::<G>);
    }
}

/// The filter itself: a copy of [`PlayerOwner`] typed by the snapshot it hides.
#[derive(Component)]
#[component(immutable)]
struct SnapshotOwner<G> {
    owner: Entity,
    game: PhantomData<G>,
}

impl<G: PlayerGame> VisibilityFilter for SnapshotOwner<G> {
    /// Never on a client entity: visibility compares the owner with the client entity itself.
    type ClientComponent = PlayerOwner;
    type Scope = SingleComponent<OwnerSnapshot<G>>;

    fn is_visible(&self, client: Entity, _: Option<&PlayerOwner>) -> bool {
        self.owner == client
    }
}

fn mirror_player_owner<G: PlayerGame>(
    insert: On<Insert, PlayerOwner>,
    owners: Query<&PlayerOwner>,
    mut commands: Commands,
) {
    let player = insert.event_target();
    if let Ok(owner) = owners.get(player) {
        commands.entity(player).insert(SnapshotOwner::<G> {
            owner: owner.0,
            game: PhantomData,
        });
    }
}

/// A player that loses its [`PlayerOwner`] is unfiltered again: everyone gets its snapshot.
fn unmirror_player_owner<G: PlayerGame>(remove: On<Remove, PlayerOwner>, mut commands: Commands) {
    // A despawn removes the owner too, and leaves nothing to remove the filter from.
    commands
        .entity(remove.event_target())
        .try_remove::<SnapshotOwner<G>>();
}
