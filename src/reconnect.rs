//! Staying connected. Nothing below netahoy notices a dead link: aeronet has no keepalive or
//! idle timeout (a stalled but open socket stays connected), replicon keeps every replicated
//! entity on disconnect (a reconnect then duplicates the world), and a reconnecting client is a
//! new client to both.
//!
//! - Client: a disconnect resets the session (every [`Remote`] entity and all netahoy client
//!   state), so a reconnect converges like a first connect. With [`Reconnect`] inserted, a
//!   silent server is dropped and the game's connect system re-runs with backoff.
//! - Server: a silent client is dropped, and a rejoin carrying its [`RejoinToken`] drops the
//!   stale connection it replaces, so a client back before the server noticed leaves no ghost.
//!
//! Kicks go through aeronet's `Disconnect`, never replicon's `DisconnectRequest`: the latter
//! despawns the session silently, skipping the `Disconnected` observers, and the client never
//! learns why. Replicon refuses a client whose protocol differs that way, so the server session
//! refuses it first, through `Disconnect` with [`PROTOCOL_MISMATCH`] as the reason: the client
//! stops reconnecting on it, since every try would be refused the same. The server plugin despawns a
//! player when its client's `ConnectedClient` goes, however the connection ends.

use std::{
    hash::{BuildHasher, RandomState},
    time::Duration,
};

use aeronet::io::{
    Session, SessionEndpoint,
    connection::{Disconnect, DisconnectReason, Disconnected},
};
use aeronet_replicon::client::AeronetRepliconClient;
use bevy::{ecs::system::SystemId, platform::collections::HashMap, prelude::*};
use bevy_replicon::{client::Remote, prelude::*};

use crate::{
    client::{ClientInputState, CommandClock, LocalCommandHistory, OwnPlayerId, RemoteClock},
    demo::DemoPlayback,
    history::ReplicatedTicks,
    protocol::{JoinRequest, RejoinToken},
};

/// The client half: join on connect with the rejoin token, reset the session on disconnect, and,
/// with [`Reconnect`], the watchdog and reconnect loop. [`crate::NetAhoyClientPlugin`] adds it;
/// alone it needs only replicon, aeronet and [`crate::NetAhoyProtocolPlugin`].
pub struct NetAhoyClientSessionPlugin;

impl Plugin for NetAhoyClientSessionPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<HeldRejoinToken>()
            .init_resource::<JoinCredential>()
            .init_resource::<SessionResets>()
            .add_observer(hold_rejoin_token)
            .add_observer(stop_on_protocol_mismatch)
            .add_systems(OnEnter(ClientState::Connected), announce_join)
            .add_systems(
                OnExit(ClientState::Connected),
                reset_client_session.run_if(not(resource_exists::<DemoPlayback>)),
            )
            .add_systems(Update, drive_reconnect.run_if(resource_exists::<Reconnect>));
    }
}

/// What [`reset_client_session`] also resets that this module can't name: the client plugin
/// registers its game-typed state here.
#[derive(Resource, Default)]
pub(crate) struct SessionResets(pub(crate) Vec<fn(&mut World)>);

/// The server's last [`RejoinToken`], sent back in the next [`JoinRequest`]. Survives the
/// session reset on purpose.
#[derive(Resource, Default, Clone, Copy, Debug)]
pub struct HeldRejoinToken(pub Option<RejoinToken>);

/// What the next [`JoinRequest`] presents to the game's server, empty if nothing. The game keeps
/// it current; survives a reset.
#[derive(Resource, Default, Clone, Debug)]
pub struct JoinCredential(pub String);

fn hold_rejoin_token(token: On<RejoinToken>, mut held: ResMut<HeldRejoinToken>) {
    held.0 = Some(*token);
}

/// Why a server drops a client whose protocol differs from its own: the replicated types, the
/// wire or the tick rate. The client stops reconnecting when it is dropped for it.
pub const PROTOCOL_MISMATCH: &str =
    "protocol mismatch: the server runs another build or another tick rate";

/// The server refused this client's protocol. Reconnecting can't help, so the loop stops.
fn stop_on_protocol_mismatch(
    disconnected: On<Disconnected>,
    mut commands: Commands,
    reconnect: Option<Res<Reconnect>>,
) {
    let DisconnectReason::ByPeer(reason) = &disconnected.reason else {
        return;
    };
    if reason != PROTOCOL_MISMATCH || reconnect.is_none() {
        return;
    }
    error!("the server refused this client ({PROTOCOL_MISMATCH}): not reconnecting");
    commands.remove_resource::<Reconnect>();
}

fn announce_join(
    mut commands: Commands,
    token: Res<HeldRejoinToken>,
    credential: Res<JoinCredential>,
) {
    commands.client_trigger(JoinRequest {
        rejoin: token.0,
        ticket: credential.0.clone(),
    });
}

/// Clears the old session: despawns every [`Remote`] entity and resets netahoy's client state.
///
/// Replicon has already forgotten which server entity each one mirrors, so the next connect
/// would spawn the whole world again beside them. The client-only player entities
/// ([`PredictedPlayer`](crate::PredictedPlayer) and the visuals) follow their server entities
/// out on the next `Update`.
pub(crate) fn reset_client_session(mut commands: Commands, remote: Query<Entity, With<Remote>>) {
    let count = remote.iter().count();
    for entity in &remote {
        commands.entity(entity).try_despawn();
    }
    commands.queue(|world: &mut World| {
        fn fresh<T: Resource + Default>(world: &mut World) {
            if world.contains_resource::<T>() {
                world.insert_resource(T::default());
            }
        }
        fresh::<OwnPlayerId>(world);
        fresh::<ClientInputState>(world);
        if let Some(mut commands) = world.get_resource_mut::<LocalCommandHistory>() {
            commands.clear();
        }
        if let Some(mut clock) = world.get_resource_mut::<RemoteClock>() {
            clock.reset();
        }
        if let Some(mut clock) = world.get_resource_mut::<CommandClock>() {
            clock.reset();
        }
        // Pose and body histories are components: they left with their entities.
        fresh::<ReplicatedTicks>(world);
        let resets = world
            .get_resource::<SessionResets>()
            .map(|resets| resets.0.clone());
        for reset in resets.into_iter().flatten() {
            reset(world);
        }
    });
    info!("session reset: despawned {count} replicated entities");
}

/// Keeps this client connected: insert it with the game's connect system, and netahoy runs that
/// system with backoff whenever the client is disconnected, and drops connections that go quiet
/// or never finish connecting. The game's own first connect still works: netahoy steps in only
/// after [`Self::backoff_min`] disconnected.
#[derive(Resource, Debug)]
pub struct Reconnect {
    /// The game's connect system: it spawns the session entity, transport and all.
    pub connect: SystemId,
    /// No packet from the server for this long while connected: the link is dead, whatever the
    /// socket says. The server sends every tick, so seconds of nothing are never a quiet moment.
    pub server_silence: Duration,
    /// A connect that hasn't finished by now is abandoned and retried.
    pub connect_timeout: Duration,
    /// The wait before the first retry; each failed one doubles it.
    pub backoff_min: Duration,
    /// The longest wait between retries.
    pub backoff_max: Duration,
    /// Connected and hearing the server this long resets the backoff, so a server that accepts
    /// then drops the client at once still backs off.
    pub stable_after: Duration,
    backoff: Duration,
    waited: Duration,
    connecting_for: Duration,
    connected_for: Duration,
    silent_for: Duration,
    last_packets: usize,
    /// Reconnects started so far.
    pub attempts: u32,
}

impl Reconnect {
    /// Reconnects with `connect`: a link is dead after 5 s of silence or a 10 s connect, and
    /// retries wait 0.5 s to 8 s.
    pub fn new(connect: SystemId) -> Self {
        let backoff_min = Duration::from_millis(500);
        Self {
            connect,
            server_silence: Duration::from_secs(5),
            connect_timeout: Duration::from_secs(10),
            backoff_min,
            backoff_max: Duration::from_secs(8),
            stable_after: Duration::from_secs(5),
            backoff: backoff_min,
            waited: Duration::ZERO,
            connecting_for: Duration::ZERO,
            connected_for: Duration::ZERO,
            silent_for: Duration::ZERO,
            last_packets: 0,
            attempts: 0,
        }
    }
}

fn drive_reconnect(
    mut commands: Commands,
    time: Res<Time<Real>>,
    state: Res<State<ClientState>>,
    mut reconnect: ResMut<Reconnect>,
    sessions: Query<
        (Entity, Option<&Session>),
        (With<SessionEndpoint>, With<AeronetRepliconClient>),
    >,
) {
    let dt = time.delta();
    let drop_sessions = |commands: &mut Commands, reason: &str| {
        for (session, _) in &sessions {
            commands.trigger(Disconnect::new(session, reason));
        }
    };

    match state.get() {
        ClientState::Connected => {
            reconnect.waited = Duration::ZERO;
            reconnect.connecting_for = Duration::ZERO;
            let packets: usize = sessions
                .iter()
                .filter_map(|(_, session)| session)
                .map(|session| session.stats.packets_recv.0)
                .sum();
            // Judge progress before charging this frame's time: the first frame back from a
            // hidden tab carries a huge delta, but also everything the browser buffered.
            if packets != reconnect.last_packets {
                reconnect.last_packets = packets;
                reconnect.silent_for = Duration::ZERO;
                reconnect.connected_for += dt;
                if reconnect.connected_for >= reconnect.stable_after {
                    reconnect.backoff = reconnect.backoff_min;
                }
                return;
            }
            reconnect.silent_for += dt;
            if reconnect.silent_for >= reconnect.server_silence {
                warn!(
                    "server silent for {:.1?}, dropping the connection",
                    reconnect.silent_for
                );
                drop_sessions(&mut commands, "server went silent");
                reconnect.silent_for = Duration::ZERO;
            }
        }
        ClientState::Connecting => {
            reconnect.connecting_for += dt;
            if reconnect.connecting_for >= reconnect.connect_timeout {
                warn!("connect timed out after {:.1?}", reconnect.connecting_for);
                drop_sessions(&mut commands, "connect timed out");
                reconnect.connecting_for = Duration::ZERO;
            }
        }
        ClientState::Disconnected => {
            reconnect.connected_for = Duration::ZERO;
            reconnect.silent_for = Duration::ZERO;
            reconnect.last_packets = 0;
            reconnect.waited += dt;
            if reconnect.waited >= reconnect.backoff {
                reconnect.attempts += 1;
                info!(
                    "reconnecting (attempt {}, next backoff {:.1?})",
                    reconnect.attempts,
                    (reconnect.backoff * 2).min(reconnect.backoff_max)
                );
                commands.run_system(reconnect.connect);
                reconnect.waited = Duration::ZERO;
                reconnect.backoff = (reconnect.backoff * 2).min(reconnect.backoff_max);
            }
        }
    }
}

/// The server half: rejoin tokens and the silent-client timeout. [`crate::NetAhoyServerPlugin`]
/// adds it; alone it needs only replicon, aeronet and [`crate::NetAhoyProtocolPlugin`].
pub struct NetAhoyServerSessionPlugin;

impl Plugin for NetAhoyServerSessionPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<RejoinTokens>()
            .init_resource::<ClientTimeout>()
            .add_observer(refuse_other_protocols)
            .add_observer(issue_rejoin_token)
            .add_observer(forget_rejoin_token)
            .add_systems(Update, drop_silent_clients);
    }
}

/// A client heard nothing from for this long is dropped. Long, because a hidden browser tab
/// sends nothing; the client reconnects and rejoins on its own when it comes back.
#[derive(Resource, Clone, Copy, Debug)]
pub struct ClientTimeout(pub Duration);

impl Default for ClientTimeout {
    fn default() -> Self {
        Self(Duration::from_secs(30))
    }
}

/// Live rejoin tokens, each to the client connection it was issued to. SipHash under random
/// per-process keys: not a security boundary, but another player can't predict yours to kick you.
#[derive(Resource, Default)]
pub(crate) struct RejoinTokens {
    clients: HashMap<u64, Entity>,
    seed: RandomState,
    issued: u64,
}

/// Drops a client whose protocol hash differs, with [`PROTOCOL_MISMATCH`] as the reason, before
/// replicon's own check despawns it unheard.
fn refuse_other_protocols(
    hash: On<FromClient<ProtocolHash>>,
    protocol: Res<ProtocolHash>,
    mut commands: Commands,
) {
    let Some(client) = hash.client_id.entity() else {
        return;
    };
    if **hash != *protocol {
        info!("refusing {client}: {PROTOCOL_MISMATCH}");
        commands.trigger(Disconnect::new(client, PROTOCOL_MISMATCH));
    }
}

fn issue_rejoin_token(
    join: On<FromClient<JoinRequest>>,
    mut commands: Commands,
    mut tokens: ResMut<RejoinTokens>,
    authorized: Query<(), With<AuthorizedClient>>,
) {
    let Some(client) = join.client_id.entity() else {
        return;
    };
    // A refused client gets no token, and can't drop the connection one names.
    if !authorized.contains(client) {
        return;
    }
    if let Some(RejoinToken(token)) = join.rejoin
        && let Some(stale) = tokens.clients.remove(&token)
        && stale != client
    {
        info!("{client} rejoined: dropping its stale connection {stale}");
        commands.trigger(Disconnect::new(stale, "rejoined from a new connection"));
    }
    tokens.issued += 1;
    let token = tokens.seed.hash_one(tokens.issued);
    tokens.clients.retain(|_, owner| *owner != client);
    tokens.clients.insert(token, client);
    commands.server_trigger(ToClients {
        targets: SendTargets::Single(ClientId::Client(client)),
        message: RejoinToken(token),
    });
}

fn forget_rejoin_token(removed: On<Remove, ConnectedClient>, mut tokens: ResMut<RejoinTokens>) {
    let client = removed.event_target();
    tokens.clients.retain(|_, owner| *owner != client);
}

/// Packets heard per connected client, and for how long they've stopped.
#[derive(Default)]
struct Hearing {
    packets: usize,
    silent_for: Duration,
}

fn drop_silent_clients(
    mut commands: Commands,
    time: Res<Time<Real>>,
    timeout: Res<ClientTimeout>,
    clients: Query<(Entity, &Session), With<ConnectedClient>>,
    mut heard: Local<HashMap<Entity, Hearing>>,
) {
    heard.retain(|client, _| clients.contains(*client));
    for (client, session) in &clients {
        let hearing = heard.entry(client).or_default();
        let packets = session.stats.packets_recv.0;
        if packets != hearing.packets {
            hearing.packets = packets;
            hearing.silent_for = Duration::ZERO;
            continue;
        }
        hearing.silent_for += time.delta();
        if hearing.silent_for >= timeout.0 {
            info!(
                "{client} silent for {:.1?}, dropping it",
                hearing.silent_for
            );
            commands.trigger(Disconnect::new(client, "timed out"));
            hearing.silent_for = Duration::ZERO;
        }
    }
}
