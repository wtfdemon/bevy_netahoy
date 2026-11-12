//! Lag compensation: rays against every player's hitbox as it stood at the time the shooter
//! saw.

use avian3d::prelude::{Collider, Position, Rotation};
use bevy::prelude::*;

use crate::{
    history::History,
    poses::{RemotePose, RemoteRenderTime, pose_at},
    protocol::{PlayerId, PlayerSnapshot, TickRate},
};

/// How far along the ray it meets the player hitbox at `center`: the movement collider's
/// upright cylinder, so what you hit is what blocks you.
pub(crate) fn ray_hitbox_distance(
    origin: Vec3,
    direction: Vec3,
    max_distance: f32,
    center: Vec3,
    radius: f32,
    half_height: f32,
) -> Option<f32> {
    let direction = direction.try_normalize()?;
    let cylinder = Collider::cylinder(radius, half_height * 2.0);
    let (distance, _) = cylinder.cast_ray(
        Position::new(center),
        Rotation::IDENTITY,
        origin,
        direction,
        max_distance,
        false,
    )?;
    Some(distance)
}

/// Every player's pose history, sorted by [`PlayerId`] so any f32 sum over players runs in the
/// same order on both peers.
pub struct PlayerPoses<'a> {
    players: Vec<(PlayerId, &'a History<PlayerSnapshot>)>,
    rate: TickRate,
}

impl<'a> PlayerPoses<'a> {
    /// The players' histories, a tick of `rate` between entries.
    pub fn new(
        players: impl IntoIterator<Item = (&'a PlayerId, &'a History<PlayerSnapshot>)>,
        rate: TickRate,
    ) -> Self {
        let mut players: Vec<_> = players
            .into_iter()
            .map(|(id, history)| (*id, history))
            .collect();
        players.sort_by_key(|(id, _)| id.0);
        players.dedup_by_key(|(id, _)| id.0);
        Self { players, rate }
    }

    /// Every player's id, in order.
    pub fn ids(&self) -> impl Iterator<Item = PlayerId> + '_ {
        self.players.iter().map(|(id, _)| *id)
    }

    /// `player_id`'s pose history.
    pub fn history(&self, player_id: PlayerId) -> Option<&'a History<PlayerSnapshot>> {
        self.players
            .iter()
            .find(|(id, _)| *id == player_id)
            .map(|(_, history)| *history)
    }

    /// `player_id`'s pose at `server_time`: the curve through its history that remote rendering,
    /// lag-compensated hits and the player push all sample, on both peers. Past the newest entry
    /// it is dead-reckoned for at most 0.3 s, then held; across a teleport it jumps at the
    /// teleport's tick.
    pub fn pose_at_time(
        &self,
        player_id: PlayerId,
        server_time: RemoteRenderTime,
    ) -> Option<RemotePose> {
        pose_at(self.history(player_id)?, server_time, self.rate)
    }

    /// The nearest player the ray meets, each as it stood at the cast's seen time.
    pub fn raycast_hitboxes_at_time(&self, cast: LagCompensatedCast) -> Option<LagCompensatedHit> {
        let direction = cast.direction.try_normalize()?;

        self.players
            .iter()
            .filter(|(player_id, _)| cast.ignored_player != Some(*player_id))
            .filter_map(|(player_id, history)| {
                let pose = pose_at(history, cast.server_time, self.rate)?;
                let distance = ray_hitbox_distance(
                    cast.origin,
                    direction,
                    cast.max_distance,
                    pose.position,
                    cast.radius,
                    cast.half_height,
                )?;
                Some(LagCompensatedHit {
                    player_id: *player_id,
                    position: cast.origin + direction * distance,
                    distance,
                })
            })
            .min_by(|a, b| a.distance.total_cmp(&b.distance))
    }
}

/// A ray against every player as they stood at one time: [`PlayerPoses::raycast_hitboxes_at_time`].
#[derive(Clone, Copy, Debug)]
pub struct LagCompensatedCast {
    /// When the players are met: the shooter's seen time.
    pub server_time: RemoteRenderTime,
    /// Where the ray starts.
    pub origin: Vec3,
    /// Where it goes; normalized by the cast.
    pub direction: Vec3,
    /// How far it reaches (m).
    pub max_distance: f32,
    /// The players' cylinder radius (m), plus the shot's own width if it has one.
    pub radius: f32,
    /// Half the players' cylinder height (m), plus the shot's own width if it has one.
    pub half_height: f32,
    /// A player the ray passes through: the shooter.
    pub ignored_player: Option<PlayerId>,
}

/// The nearest player a [`LagCompensatedCast`] met.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LagCompensatedHit {
    /// Who it hit.
    pub player_id: PlayerId,
    /// Where on its cylinder.
    pub position: Vec3,
    /// How far along the ray (m).
    pub distance: f32,
}

#[cfg(test)]
mod tests;
