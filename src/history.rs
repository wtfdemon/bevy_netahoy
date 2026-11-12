//! One history for everything the netcode samples at a command's seen time:
//! players ([`PlayerSnapshot`]), moving bodies ([`BodySnapshot`]) and a game's own state
//! ([`AppHistoryExt::replicate_with_history`]).
//!
//! [`History<C>`] is a component, the same on both peers, keyed by netahoy's
//! [`ServerTick`](crate::ServerTick), and dense: one entry per tick from the
//! entity's first value to the newest tick this peer knows happened.
//!
//! - **server:** the publisher [`History::record`]s every entity's value
//!   every tick, changed or not, in [`NetAhoyServerSystems::Publish`] (`FixedLast`): the
//!   entry at tick t is the value tick t left;
//! - **client:** replicon hands every received `C` to [`write_history`],
//!   older mutations of a burst included, under netahoy's tick for the
//!   replication message that carried it, sent as replicon userdata
//!   ([`stamp_replication_tick`] / [`ReplicatedTicks`]); replicon's own tick
//!   is a different counter. Ticks the server sent nothing for are the value
//!   holding: [`History::hold_until`] copies it forward once the tick is known
//!   to have happened (the owner snapshot's heartbeat).
//!
//! So both peers hold the same entry at every tick, and a reader never asks
//! "was this a stop or a late packet": a stop is the same value twice, and past
//! the newest entry is only ever data that has not arrived yet. Replicon
//! resends a changed value until the client acks it, under a later message
//! tick each time: that rewrites the value the tick held anyway.
//!
//! A tick can also be lost: over WebTransport a replication message is a datagram, and when one
//! is dropped its tick never arrives while the next one brings a newer value. Held forward, every
//! remote stopped dead for that tick and then stepped twice. A value says when the server wrote
//! the one before it ([`Recorded::previous_write`]): a write after the last value received was
//! lost, and the ticks between the two run on the line between them ([`Recorded::between`])
//! instead of holding.

use bevy::{
    app::RunFixedMainLoop,
    ecs::{component::Mutable, lifecycle::HookContext, world::DeferredWorld},
    prelude::*,
};
use bevy_replicon::{
    bytes::Bytes,
    client::UserdataReceived,
    postcard_utils,
    prelude::*,
    server::ReplicationUserdata,
    shared::{
        replication::{
            deferred_entity::DeferredEntity,
            receive_markers::MarkerConfig,
            registry::ctx::{RemoveCtx, WriteCtx},
        },
        replicon_tick::RepliconTick,
    },
};
use serde::{Deserialize, Serialize};

use crate::{
    client::RemoteClock,
    poses::RemoteRenderTime,
    poses::{REMOTE_INTERPOLATION_TELEPORT_DISTANCE, radians_between},
    protocol::{BodySnapshot, PlayerSnapshot, TickRate},
    server::{NetAhoyServerSystems, ServerTick},
};

/// How far back a [`History`] reaches (s): how far back anyone may ask, and the server clamps a
/// command's seen time to it. Replicon passes on a mutation older than the newest it applied only
/// within 64 of its own ticks.
pub(crate) const HISTORY_SECONDS: f64 = 3.2;

/// The newest ticks of `C`, one entry per tick, in a ring of [`History::capacity`] entries
/// indexed by `tick % capacity`: what the server published, or the client received, of every
/// player's [`PlayerSnapshot`], every moving body's [`BodySnapshot`] and every value a game
/// keeps with [`AppHistoryExt::replicate_with_history`]. netahoy writes it on both peers; a game
/// reads it, at a time with [`History::sample`].
///
/// A default history has no capacity yet: it takes 3.2 s at the world's [`TickRate`] when it is
/// inserted on an entity, and recording into one that has none panics.
#[derive(Component, Clone, Debug)]
#[component(on_insert = size_to_tick_rate::<C>)]
pub struct History<C> {
    /// Empty until the first value, then `capacity` long; the entry for every tick in
    /// `oldest..=newest` is at `tick % capacity`.
    ring: Vec<C>,
    /// Bit `tick % capacity`: that tick's entry is a value that was recorded, not one held
    /// forward from the tick before.
    recorded: Vec<u64>,
    capacity: usize,
    oldest: u64,
    newest: u64,
}

impl<C> Default for History<C> {
    fn default() -> Self {
        Self {
            ring: Vec::new(),
            recorded: Vec::new(),
            capacity: 0,
            oldest: 0,
            newest: 0,
        }
    }
}

/// A history inserted without a capacity takes the world's tick rate's.
fn size_to_tick_rate<C>(mut world: DeferredWorld, context: HookContext)
where
    History<C>: Component<Mutability = Mutable>,
{
    let Some(&rate) = world.get_resource::<TickRate>() else {
        return;
    };
    if let Some(mut history) = world.get_mut::<History<C>>(context.entity)
        && history.capacity == 0
    {
        history.capacity = rate.ticks(HISTORY_SECONDS) as usize;
    }
}

impl<C: Recorded> History<C> {
    /// The value of `tick`, from the server's publish or one replication message. Out of order
    /// is fine: it replaces its slot, and the ticks after it that only held a value forward hold
    /// this one, up to the next recorded entry. A tick past the newest holds the newest value
    /// over the ticks between. Returns whether no later tick has a recorded value: this is the
    /// newest the peer has heard.
    pub(crate) fn record(&mut self, tick: u64, value: C) -> bool {
        if !value.recordable() {
            return false;
        }
        if self.ring.is_empty() {
            assert!(
                self.capacity > 0,
                "a History without a capacity: insert it where there is a TickRate \
                 (NetAhoyProtocolPlugin), or make it with History::new"
            );
            self.ring = vec![value.at(tick); self.capacity];
            self.recorded = vec![0; self.capacity.div_ceil(u64::BITS as usize)];
            (self.oldest, self.newest) = (tick, tick);
            self.mark(tick, true);
            return true;
        }
        if tick > self.newest {
            self.hold_until(tick);
        } else if tick < self.oldest {
            // Older than everything held: only while it still fits the ring.
            if self.newest - tick >= self.capacity as u64 {
                return false;
            }
            self.oldest = tick;
        }
        self.mark(tick, true);
        let slot = self.slot(tick);
        self.ring[slot] = value.at(tick);
        // The ticks since the value before it held that one, or were lost.
        if let Some(before) = (self.oldest..tick).rev().find(|&at| self.is_recorded(at)) {
            self.fill(before, tick);
        }
        match (tick + 1..=self.newest).find(|&at| self.is_recorded(at)) {
            // The ticks after it held this one, or were lost on the way to the next.
            Some(after) => {
                self.fill(tick, after);
                false
            }
            // The ticks after it held their value forward: this one now.
            None => {
                for held in tick + 1..=self.newest {
                    let slot = self.slot(held);
                    self.ring[slot] = value.at(held);
                }
                true
            }
        }
    }

    /// The ticks strictly between two recorded ones: the line between them if the server wrote a
    /// value in between that never arrived, else the earlier value held.
    fn fill(&mut self, from: u64, to: u64) {
        if to <= from + 1 {
            return;
        }
        let start = self.ring[self.slot(from)].clone();
        let end = self.ring[self.slot(to)].clone();
        let lost = end.previous_write(to).is_some_and(|write| write > from);
        let span = (to - from) as f32;
        for at in from + 1..to {
            let value = lost
                .then(|| start.between(&end, (at - from) as f32 / span))
                .flatten()
                .unwrap_or_else(|| start.clone());
            let slot = self.slot(at);
            self.ring[slot] = value.at(at);
        }
    }

    /// Every tick through `tick` happened, and this history heard nothing
    /// for those past its newest entry: the newest value held through them.
    /// Nothing before the first value.
    pub(crate) fn hold_until(&mut self, tick: u64) {
        if self.ring.is_empty() || tick <= self.newest {
            return;
        }
        let held = self.ring[self.slot(self.newest)].clone();
        let capacity = self.capacity as u64;
        let from = (self.newest + 1).max((tick + 1).saturating_sub(capacity));
        for at in from..=tick {
            let slot = self.slot(at);
            self.ring[slot] = held.at(at);
            self.mark(at, false);
        }
        self.newest = tick;
        self.oldest = self.oldest.max((tick + 1).saturating_sub(capacity));
    }
}

impl<C> History<C> {
    /// An empty history of the newest `capacity` ticks. Panics on 0.
    pub(crate) fn with_capacity(capacity: usize) -> Self {
        assert!(capacity > 0, "a History of 0 ticks");
        Self {
            capacity,
            ..default()
        }
    }

    /// An empty history of 3.2 s at `rate`.
    pub(crate) fn new(rate: TickRate) -> Self {
        Self::with_capacity(rate.ticks(HISTORY_SECONDS) as usize)
    }

    /// How many ticks it holds once full; 0 before it is sized.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    fn slot(&self, tick: u64) -> usize {
        (tick % self.capacity as u64) as usize
    }

    fn is_recorded(&self, tick: u64) -> bool {
        let slot = self.slot(tick);
        self.recorded[slot / u64::BITS as usize] >> (slot % u64::BITS as usize) & 1 == 1
    }

    fn mark(&mut self, tick: u64, recorded: bool) {
        let slot = self.slot(tick);
        let (word, bit) = (slot / u64::BITS as usize, 1 << (slot % u64::BITS as usize));
        if recorded {
            self.recorded[word] |= bit;
        } else {
            self.recorded[word] &= !bit;
        }
    }

    fn entry(&self, tick: u64) -> (u64, &C) {
        (tick, &self.ring[self.slot(tick)])
    }

    /// The newest entry and its tick.
    pub fn newest(&self) -> Option<(u64, &C)> {
        (!self.ring.is_empty()).then(|| self.entry(self.newest))
    }

    /// The value in force at `tick`: its own entry, or the newest one before
    /// it.
    pub fn at_or_before(&self, tick: u64) -> Option<(u64, &C)> {
        (!self.ring.is_empty() && tick >= self.oldest).then(|| self.entry(tick.min(self.newest)))
    }

    /// The entries either side of `time`: what was drawn at a render time, or seen at a step's
    /// [`seen_time`](crate::Step::seen_time). `None` until the first value.
    pub fn sample(&self, time: RemoteRenderTime) -> Option<Sample<'_, C>> {
        if self.ring.is_empty() {
            return None;
        }
        let at = |tick: u64| &self.ring[self.slot(tick)];
        // Outside the entries the nearest one holds: no extrapolation.
        let held = |tick: u64| Sample {
            from: at(tick),
            to: at(tick),
            alpha: 0.0,
        };
        Some(if time.tick < self.oldest {
            held(self.oldest)
        } else if time.tick >= self.newest {
            held(self.newest)
        } else {
            Sample {
                from: at(time.tick),
                to: at(time.tick + 1),
                alpha: time.alpha,
            }
        })
    }

    /// The entries around a fractional tick, for interpolation: whole ticks
    /// apart, always.
    pub(crate) fn around(&self, ticks: f64) -> Around<'_, C> {
        if self.ring.is_empty() {
            return Around::Empty;
        }
        let at = |tick: u64| &self.ring[self.slot(tick)];
        let before = |tick: u64| (tick > self.oldest).then(|| at(tick - 1));
        if ticks <= self.oldest as f64 {
            return Around::Before(at(self.oldest));
        }
        if ticks >= self.newest as f64 {
            return Around::After {
                before: before(self.newest),
                last: at(self.newest),
            };
        }
        // Strictly inside: `end` is the first tick at or past `ticks`.
        let end = ticks.ceil() as u64;
        Around::Between {
            before: before(end - 1),
            start: at(end - 1),
            end: at(end),
            after: (end < self.newest).then(|| at(end + 1)),
        }
    }

    /// Every entry, oldest first.
    pub fn iter(&self) -> impl DoubleEndedIterator<Item = (u64, &C)> {
        (self.oldest..self.oldest + self.len() as u64).map(|tick| self.entry(tick))
    }

    /// How many ticks it holds, oldest to newest.
    pub fn len(&self) -> usize {
        if self.ring.is_empty() {
            0
        } else {
            (self.newest - self.oldest + 1) as usize
        }
    }

    /// Whether nothing has been recorded.
    pub fn is_empty(&self) -> bool {
        self.ring.is_empty()
    }
}

/// Two neighbouring entries of a [`History`] and where a time falls between them
/// ([`History::sample`]). The game interpolates: discrete state reads `from`, the value in force;
/// continuous state blends `from` toward `to` by `alpha`, on whatever curve it likes.
#[derive(Clone, Copy, Debug)]
pub struct Sample<'a, C> {
    /// The entry of the time's whole tick: the value in force at the time.
    pub from: &'a C,
    /// The entry of the tick after it; `from` itself outside the entries.
    pub to: &'a C,
    /// How far from `from` toward `to` the time is, `0.0..1.0`; 0 outside the entries.
    pub alpha: f32,
}

/// Where a fractional tick falls in a [`History`]. Neighbouring entries are
/// neighbouring ticks.
pub(crate) enum Around<'a, C> {
    /// Nothing recorded.
    Empty,
    /// At or before the oldest entry.
    Before(&'a C),
    /// Strictly inside `(start, end]`.
    Between {
        /// The tick before `start`, if held.
        before: Option<&'a C>,
        /// The tick before the time.
        start: &'a C,
        /// The tick at or after the time.
        end: &'a C,
        /// The tick after `end`, if held.
        after: Option<&'a C>,
    },
    /// At or past the newest entry: data that has not arrived yet.
    After {
        /// The tick before `last`, if held.
        before: Option<&'a C>,
        /// The newest entry.
        last: &'a C,
    },
}

/// A value a [`History`] can hold. A game's own needs no methods: `impl Recorded for MyState {}`.
pub trait Recorded: Component<Mutability = Mutable> + Clone {
    /// This value as the entry of `tick`. netahoy's snapshots stamp their tick into it; the
    /// default is the value as it is.
    fn at(&self, tick: u64) -> Self {
        let _ = tick;
        self.clone()
    }

    /// False for a placeholder that must not enter a history (a snapshot the
    /// server has not published yet).
    fn recordable(&self) -> bool {
        true
    }

    /// The tick the server wrote the value before this one, recorded under `tick`: the ticks
    /// after that write held it. A write newer than the last value a client received never
    /// arrived. `None`: not known, the last value received held.
    fn previous_write(&self, tick: u64) -> Option<u64> {
        let _ = tick;
        None
    }

    /// The value `alpha` of the way from this one to `next`, for the ticks lost between them.
    /// `None` holds this one instead (a teleport).
    fn between(&self, next: &Self, alpha: f32) -> Option<Self> {
        let _ = (next, alpha);
        None
    }
}

impl Recorded for PlayerSnapshot {
    fn at(&self, tick: u64) -> Self {
        Self {
            server_tick: tick,
            ..*self
        }
    }

    /// The server publishes every player every tick.
    fn previous_write(&self, tick: u64) -> Option<u64> {
        tick.checked_sub(1)
    }

    /// Position and look on the line (yaw the short way); the rest as it was before the loss.
    fn between(&self, next: &Self, alpha: f32) -> Option<Self> {
        if self.position.distance(next.position) > REMOTE_INTERPOLATION_TELEPORT_DISTANCE {
            return None;
        }
        Some(Self {
            position: self.position.lerp(next.position, alpha),
            look: Vec2::new(
                self.look.x + radians_between(self.look.x, next.look.x) * alpha,
                self.look.y.lerp(next.look.y, alpha),
            ),
            ..*self
        })
    }
}

impl Recorded for BodySnapshot {
    fn at(&self, tick: u64) -> Self {
        Self { tick, ..*self }
    }

    fn recordable(&self) -> bool {
        self.tick != 0
    }

    /// The server writes a body only when it moves: `since_previous` says how long the pose
    /// before held.
    fn previous_write(&self, _tick: u64) -> Option<u64> {
        (self.since_previous != 0).then(|| self.tick.saturating_sub(u64::from(self.since_previous)))
    }

    /// Position on the line, rotation slerped.
    fn between(&self, next: &Self, alpha: f32) -> Option<Self> {
        if self.position.distance(next.position) > REMOTE_INTERPOLATION_TELEPORT_DISTANCE {
            return None;
        }
        Some(Self {
            position: self.position.lerp(next.position, alpha),
            rotation: self.rotation.slerp(next.rotation, alpha),
            ..*self
        })
    }
}

/// A game's own state, replicated and kept in a [`History`] on both peers.
pub trait AppHistoryExt {
    /// Replicates `C` and keeps a [`History<C>`] of it on every entity that has one, on both
    /// peers, keyed by the server's tick, as netahoy keeps its moving bodies':
    ///
    /// - the server records each entity's `C` every tick, changed or not, in
    ///   [`NetAhoyServerSystems::Publish`] (`FixedLast`): the entry at tick t is what tick t
    ///   left. Write `C` in the fixed schedule, before it;
    /// - a client records every value replication brings under the tick that sent it, and
    ///   holds the newest through the ticks that brought none.
    ///
    /// So a step reads it at its command's [`seen_time`](crate::Step::seen_time) and both peers
    /// judge the same entry. Call it on both peers, after `RepliconPlugins` and
    /// [`NetAhoyProtocolPlugin`](crate::NetAhoyProtocolPlugin).
    fn replicate_with_history<C>(&mut self) -> &mut Self
    where
        C: Recorded + Serialize + for<'de> Deserialize<'de>;
}

impl AppHistoryExt for App {
    fn replicate_with_history<C>(&mut self) -> &mut Self
    where
        C: Recorded + Serialize + for<'de> Deserialize<'de>,
    {
        replicate_history::<C>(self)
            .register_required_components::<C, History<C>>()
            .add_systems(
                FixedLast,
                record_history::<C>
                    .in_set(NetAhoyServerSystems::Publish)
                    .run_if(resource_exists::<ServerTick>),
            )
    }
}

/// Replicates `C` and keeps every value received, every tick of a burst included, in its
/// entity's [`History`], held to the tick the client knows happened ([`HoldHistories`]).
pub(crate) fn replicate_history<C>(app: &mut App) -> &mut App
where
    C: Recorded + Serialize + for<'de> Deserialize<'de>,
{
    app.replicate::<C>()
        // The history is its own marker: from the first value on, replicon
        // passes older mutations of a burst too, not only the newest.
        .register_marker_with::<History<C>>(MarkerConfig {
            need_history: true,
            ..Default::default()
        })
        .set_marker_fns::<History<C>, C>(write_history::<C>, remove_history::<C>)
        // The first value arrives before the marker exists.
        .set_receive_fns::<C>(write_history::<C>, remove_history::<C>)
        .add_systems(
            RunFixedMainLoop,
            hold_history::<C>
                .in_set(HoldHistories)
                .run_if(resource_exists::<RemoteClock>),
        )
}

/// Server: every entity's `C` into its history under this tick, changed or not.
fn record_history<C: Recorded>(tick: Res<ServerTick>, mut values: Query<(&C, &mut History<C>)>) {
    for (value, mut history) in &mut values {
        history.record(tick.0, value.clone());
    }
}

/// Client: where every [`History`] runs to the tick the client knows happened, once the remote
/// clock has heard this frame's owner snapshot and before anything samples.
#[derive(SystemSet, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct HoldHistories;

/// Client: every [`History<C>`] runs to the owner snapshot's tick: a tick that brought an
/// entity nothing is its value holding ([`History::hold_until`]).
///
/// That leans on an in-order transport that never drops (WebSocket): once tick T's owner snapshot
/// is applied, every message sent before it has arrived. Only the rest of tick T can come later,
/// when replicon splits a tick's mutations across messages; that value replaces the held copy and
/// the held ticks after it follow ([`History::record`]). Over datagrams a tick that brought
/// nothing may have lost something, and holding would turn the loss into a stop: that needs
/// replicon's per-entity confirmation instead.
fn hold_history<C: Recorded>(clock: Res<RemoteClock>, mut histories: Query<&mut History<C>>) {
    let tick = clock.latest_server_tick;
    for mut history in &mut histories {
        // Only a history that runs behind is written: change detection stays quiet for the rest.
        if history.newest().is_some_and(|(newest, _)| newest < tick) {
            history.hold_until(tick);
        }
    }
}

/// Client: every received `C` goes into the history under the netahoy tick
/// of the message that carried it; the component itself keeps the newest.
pub(crate) fn write_history<C: Recorded>(
    ctx: &mut WriteCtx,
    rule_fns: &RuleFns<C>,
    entity: &mut DeferredEntity,
    message: &mut Bytes,
) -> Result<()> {
    let value: C = rule_fns.deserialize(ctx, message)?;
    let tick = entity
        .world()
        .get_resource::<ReplicatedTicks>()
        .and_then(|ticks| ticks.server_tick(ctx.message_tick))
        .ok_or("replication message without netahoy's server tick")?;
    let newest = match entity.get_mut::<History<C>>() {
        Some(mut history) => history.record(tick, value.clone()),
        None => {
            let rate = *entity
                .world()
                .get_resource::<TickRate>()
                .ok_or("a History without NetAhoyProtocolPlugin's TickRate")?;
            let mut history = History::new(rate);
            let newest = history.record(tick, value.clone());
            entity.insert(history);
            newest
        }
    };
    if newest || entity.get_mut::<C>().is_none() {
        entity.insert(value.at(tick));
    }
    Ok(())
}

/// Client: the component left (the server removed it). The history goes
/// too.
pub(crate) fn remove_history<C: Component>(_ctx: &mut RemoveCtx, entity: &mut DeferredEntity) {
    entity.remove::<C>().remove::<History<C>>();
}

/// Client: netahoy's [`ServerTick`](crate::ServerTick) for the replication
/// message being applied, from the userdata the server stamps on every
/// message. The remote clock does not read it: it runs on the owner
/// snapshot's tick, which moves every tick even when nothing else changes.
#[derive(Resource, Default, Debug)]
pub(crate) struct ReplicatedTicks {
    current: Option<(RepliconTick, u64)>,
    newest: u64,
}

impl ReplicatedTicks {
    /// Netahoy's tick for a message replicon stamped `message_tick`.
    pub(crate) fn server_tick(&self, message_tick: RepliconTick) -> Option<u64> {
        self.current
            .filter(|(at, _)| *at == message_tick)
            .map(|(_, tick)| tick)
    }

    /// The newest tick any replication message carried (0 before the
    /// first). A spectator's clock: it has no owner snapshot to run on.
    pub(crate) fn newest(&self) -> u64 {
        self.newest
    }
}

/// Client: read the netahoy tick a replication message carries, right before
/// its contents are applied.
pub(crate) fn receive_replication_tick(
    received: On<UserdataReceived>,
    mut ticks: ResMut<ReplicatedTicks>,
) {
    let mut bytes = received.bytes.clone();
    let Ok(tick) = postcard_utils::from_buf::<u64, _>(&mut bytes) else {
        warn!("replication userdata isn't a server tick");
        return;
    };
    ticks.current = Some((received.message_tick, tick));
    ticks.newest = ticks.newest.max(tick);
}

/// Server: every replication message carries netahoy's tick (written each
/// fixed tick, sent with whatever the frame replicates).
pub fn stamp_replication_tick(
    tick: Res<crate::ServerTick>,
    mut userdata: ResMut<ReplicationUserdata>,
) {
    userdata.0.clear();
    postcard_utils::to_extend_mut(&tick.0, &mut userdata.0).expect("a u64 always serializes");
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: TickRate = TickRate::new(20);

    fn body(x: f32) -> BodySnapshot {
        BodySnapshot {
            tick: 1,
            position: Vec3::X * x,
            ..default()
        }
    }

    fn log(history: &History<BodySnapshot>) -> Vec<(u64, f32)> {
        history
            .iter()
            .map(|(tick, body)| (tick, body.position.x))
            .collect()
    }

    /// A body written at `tick` whose previous write was `since` ticks before.
    fn moved(tick: u64, x: f32, since: u16) -> BodySnapshot {
        BodySnapshot {
            tick,
            position: Vec3::X * x,
            since_previous: since,
            ..default()
        }
    }

    /// A body's write that never arrived (a dropped WebTransport datagram): the next one says
    /// the server wrote one after the last value received, so the ticks between run on the line
    /// between them, not holding and then jumping. A late value refills both sides of it.
    #[test]
    fn ticks_lost_on_the_way_run_on_the_line() {
        let mut history = History::new(RATE);
        history.record(10, moved(10, 0.0, 1));
        // 11 and 12 were written and lost; the heartbeat says they happened.
        history.hold_until(12);
        assert_eq!(log(&history), [(10, 0.0), (11, 0.0), (12, 0.0)]);
        history.record(13, moved(13, 3.0, 1));
        assert_eq!(log(&history), [(10, 0.0), (11, 1.0), (12, 2.0), (13, 3.0)]);
        assert_eq!(history.at_or_before(12).unwrap().1.tick, 12);
        history.record(12, moved(12, 2.5, 1));
        assert_eq!(log(&history), [(10, 0.0), (11, 1.25), (12, 2.5), (13, 3.0)]);
    }

    /// A body the server didn't write held still: the next write says the one before was the
    /// last value received, so the ticks between hold.
    #[test]
    fn a_rest_holds() {
        let mut history = History::new(RATE);
        history.record(10, moved(10, 0.0, 1));
        history.record(13, moved(13, 3.0, 3));
        assert_eq!(log(&history), [(10, 0.0), (11, 0.0), (12, 0.0), (13, 3.0)]);
    }

    /// Lost ticks across a teleport hold: no slide from where it was to where it went.
    #[test]
    fn ticks_lost_across_a_teleport_hold() {
        let mut history = History::new(RATE);
        history.record(10, moved(10, 0.0, 1));
        history.record(12, moved(12, 50.0, 1));
        assert_eq!(log(&history), [(10, 0.0), (11, 0.0), (12, 50.0)]);
    }

    /// The server publishes every player every tick: a tick missing between two was lost.
    #[test]
    fn a_players_missing_tick_is_lost_on_the_way() {
        let player = |x: f32, look: Vec2| PlayerSnapshot {
            position: Vec3::X * x,
            look,
            ..default()
        };
        let mut history = History::new(RATE);
        history.record(10, player(0.0, Vec2::new(3.0, 0.0)));
        history.hold_until(11);
        history.record(12, player(2.0, Vec2::new(-3.0, 0.2)));
        let (_, lost) = history.at_or_before(11).unwrap();
        assert_eq!(lost.position, Vec3::X);
        assert_eq!(lost.server_tick, 11);
        // Yaw the short way round, across ±π.
        assert!(lost.look.x.abs() > 3.0, "{}", lost.look.x);
        assert!((lost.look.y - 0.1).abs() < 1e-6);
    }

    #[test]
    fn records_in_tick_order_and_replaces_a_held_tick() {
        let mut history = History::new(RATE);
        assert!(history.record(10, body(0.0)));
        assert!(history.record(12, body(2.0)));
        assert!(
            !history.record(11, body(1.0)),
            "an older tick is not the newest"
        );
        assert!(!history.record(11, body(1.5)));
        assert_eq!(log(&history), [(10, 0.0), (11, 1.5), (12, 2.0)]);
        let (_, held) = history.at_or_before(11).unwrap();
        assert_eq!(
            held.tick, 11,
            "a value knows the tick it was recorded under"
        );
        assert!(history.at_or_before(9).is_none());
    }

    /// A tick nobody sent a value for holds the one before: a body at rest,
    /// or the ticks between two values of a burst.
    #[test]
    fn every_tick_has_an_entry() {
        let mut history = History::new(RATE);
        history.record(10, body(0.0));
        history.record(13, body(3.0));
        assert_eq!(log(&history), [(10, 0.0), (11, 0.0), (12, 0.0), (13, 3.0)]);
        history.hold_until(15);
        assert_eq!(log(&history)[4..], [(14, 3.0), (15, 3.0)]);
        assert_eq!(history.at_or_before(15).unwrap().1.tick, 15);
        // Holding is "these ticks happened", never "these ticks are undone".
        history.hold_until(12);
        assert_eq!(history.len(), 6);
    }

    #[test]
    fn an_empty_history_holds_nothing() {
        let mut history = History::<BodySnapshot>::new(RATE);
        history.hold_until(20);
        assert!(history.is_empty());
        assert!(log(&history).is_empty());
        history.record(18, body(1.0));
        assert_eq!(log(&history), [(18, 1.0)]);
    }

    /// A value that arrives after its tick was held replaces the held copy,
    /// and the ticks after it that held forward hold the real value, up to
    /// the next value that arrived.
    #[test]
    fn a_late_value_overwrites_what_was_held() {
        let mut history = History::new(RATE);
        history.record(10, body(0.0));
        history.record(14, body(4.0));
        history.hold_until(16);
        // A mutate split across messages: 11's value lands after 14 and 16.
        assert!(!history.record(11, body(1.0)));
        assert_eq!(
            log(&history),
            [
                (10, 0.0),
                (11, 1.0),
                (12, 1.0),
                (13, 1.0),
                (14, 4.0),
                (15, 4.0),
                (16, 4.0)
            ]
        );
        // One at the newest heard: the held ticks after it follow it.
        assert!(history.record(15, body(5.0)));
        assert_eq!(log(&history)[5..], [(15, 5.0), (16, 5.0)]);
    }

    /// Replicon resends a changed value until it's acked, under a later
    /// message tick each time: it rewrites what that tick held anyway.
    #[test]
    fn a_resend_rewrites_the_held_value() {
        let mut server = History::new(RATE);
        let mut client = History::new(RATE);
        server.record(10, body(1.0));
        client.record(10, body(1.0));
        for tick in 11..14 {
            server.record(tick, body(1.0));
        }
        client.record(12, body(1.0));
        client.hold_until(13);
        assert_eq!(log(&client), log(&server));
    }

    #[test]
    fn a_full_ring_drops_the_oldest_and_refuses_older() {
        let mut history = History::new(RATE);
        let capacity = history.capacity();
        assert_eq!(capacity, 64, "3.2 s at 20 Hz");
        for tick in 0..capacity as u64 + 5 {
            history.record(tick + 100, body(tick as f32));
        }
        assert_eq!(history.len(), capacity);
        assert_eq!(history.iter().next().unwrap().0, 105);
        assert!(!history.record(104, body(-1.0)));
        assert_eq!(history.iter().next().unwrap().0, 105);
        // Held far past the ring: only the newest `capacity` ticks stay.
        history.hold_until(1_000);
        assert_eq!(history.len(), capacity);
        let oldest = 1_000 - capacity as u64 + 1;
        let last = body((capacity + 4) as f32);
        assert_eq!(history.iter().next().unwrap(), (oldest, &last.at(oldest)));
    }

    /// At 60 Hz a history holds 192 ticks, three words of recorded bits: a late value still
    /// holds forward over exactly the ticks that held, across the word edges and the wrap.
    #[test]
    fn a_history_of_more_than_64_ticks_keeps_its_ring() {
        let mut history = History::new(TickRate::new(60));
        let capacity = history.capacity();
        assert_eq!(capacity, 192, "3.2 s at 60 Hz");
        // 1000 is in slot 40 and 1190 in slot 38: the ticks between wrap the ring.
        history.record(1000, body(0.0));
        assert!(history.record(1190, body(190.0)));
        // 1063 (slot 103) holds over 1064..=1189, past slot 128 and the wrap, up to 1190.
        assert!(!history.record(1063, body(63.0)));
        let expected = |tick: u64| match tick {
            ..1063 => 0.0,
            1190 => 190.0,
            _ => 63.0,
        };
        let all: Vec<_> = (1000..=1190).map(|tick| (tick, expected(tick))).collect();
        assert_eq!(log(&history), all);
        assert_eq!(history.at_or_before(1189).unwrap().1.position.x, 63.0);

        // Full: a new tick drops the oldest, and a ring or more behind the newest is refused.
        history.record(1191, body(191.0));
        history.record(1192, body(192.0));
        assert_eq!(history.len(), capacity);
        assert_eq!(history.iter().next().unwrap().0, 1001);
        assert!(!history.record(1000, body(-1.0)));

        // Held far past the ring, nothing in it is recorded: a value at the oldest tick holds
        // through every word to the newest.
        history.hold_until(5_000);
        let oldest = 5_000 - capacity as u64 + 1;
        assert_eq!(history.iter().next().unwrap().0, oldest);
        assert!(history.iter().all(|(_, held)| held.position.x == 192.0));
        assert!(
            history.record(oldest, body(7.0)),
            "nothing recorded after it"
        );
        assert!(history.iter().all(|(_, held)| held.position.x == 7.0));
    }

    #[test]
    fn a_default_history_takes_the_tick_rate_of_its_world() {
        let mut world = World::new();
        world.insert_resource(TickRate::new(60));
        let entity = world.spawn(BodySnapshot::default()).id();
        assert_eq!(
            world
                .get::<History<BodySnapshot>>(entity)
                .unwrap()
                .capacity(),
            192
        );
        let sized = world.spawn(History::<BodySnapshot>::with_capacity(5)).id();
        assert_eq!(
            world
                .get::<History<BodySnapshot>>(sized)
                .unwrap()
                .capacity(),
            5
        );
    }

    /// A sample's `from` is the entry of the time's whole tick, `to` the next; outside the
    /// entries the nearest holds, with no extrapolation.
    #[test]
    fn a_sample_brackets_the_time() {
        let mut history = History::new(RATE);
        assert!(history.sample(RemoteRenderTime::new(5, 0.5)).is_none());
        for tick in [10, 11, 13] {
            history.record(tick, body(tick as f32));
        }
        let x = |sample: Sample<BodySnapshot>| (sample.from.tick, sample.to.tick, sample.alpha);
        let at = |tick, alpha| x(history.sample(RemoteRenderTime::new(tick, alpha)).unwrap());
        assert_eq!(at(11, 0.25), (11, 12, 0.25));
        assert_eq!(at(10, 0.0), (10, 11, 0.0), "a whole tick is its own entry");
        assert_eq!(
            history
                .sample(RemoteRenderTime::new(12, 0.5))
                .unwrap()
                .from
                .position
                .x,
            11.0,
            "12 held 11's value"
        );
        assert_eq!(at(9, 0.75), (10, 10, 0.0), "before the oldest: the oldest");
        assert_eq!(
            at(13, 0.5),
            (13, 13, 0.0),
            "past the newest: the newest, held"
        );
        assert_eq!(at(40, 0.0), (13, 13, 0.0));
    }

    #[test]
    fn around_finds_the_segment() {
        let mut history = History::new(RATE);
        for tick in [10, 11, 13] {
            history.record(tick, body(tick as f32));
        }
        assert!(matches!(history.around(9.0), Around::Before(first) if first.tick == 10));
        assert!(matches!(
            history.around(14.0),
            Around::After { before: Some(before), last } if (before.tick, last.tick) == (12, 13)
        ));
        let Around::Between {
            before,
            start,
            end,
            after,
        } = history.around(12.5)
        else {
            panic!("wrong segment");
        };
        assert_eq!(
            (before.map(|b| b.tick), start.tick, end.tick),
            (Some(11), 12, 13)
        );
        assert!(after.is_none(), "13 is the newest");
        assert_eq!(start.position.x, 11.0, "12 held 11's value");
        let Around::Between {
            before,
            start,
            end,
            after,
        } = history.around(10.5)
        else {
            panic!("wrong segment");
        };
        assert_eq!(
            (before.map(|b| b.tick), start.tick, end.tick),
            (None, 10, 11)
        );
        assert_eq!(after.map(|a| a.tick), Some(12));
    }
}
