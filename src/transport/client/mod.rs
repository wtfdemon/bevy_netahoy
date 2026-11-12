//! The client's side: which transport each connect uses by [`rules`](super::rules), the sessions
//! over it, and what decides the next one: a handshake that fails or hangs, a session whose UDP
//! alone died.

mod endpoints;
pub(super) mod fetch;

use aeronet::io::{
    Session, SessionEndpoint,
    connection::{Disconnect, DisconnectReason, Disconnected},
};
use aeronet_replicon::client::{AeronetRepliconClient, AeronetRepliconClientPlugin};
use aeronet_websocket::client::{WebSocketClient, WebSocketClientPlugin};
use aeronet_webtransport::{
    cert::{self, CertificateHash},
    client::{WebTransportClient, WebTransportClientPlugin},
};
use bevy::{
    ecs::system::SystemId,
    prelude::*,
    tasks::{IoTaskPool, Task, futures::check_ready},
};

pub use endpoints::ServerEndpoints;

use super::{
    Offer, Transport,
    rules::{
        ASK_WAIT_SECS, Answer, GIVE_UP_SECS, GaveUp, OfferState, Plan, QUIET_SECS, Silence,
        TransportChoice, plan,
    },
};
use crate::{LinkLossModel, Reconnect, TickRate, client::RemoteClock};

/// Connects over WebTransport first and falls back to WebSocket, and keeps the client connected
/// through [`Reconnect`]. It adds aeronet's WebSocket and WebTransport client plugins and
/// replicon's aeronet backend; every session it opens carries replicon and its [`Transport`].
pub struct NetAhoyTransportClientPlugin {
    /// Where the server is.
    pub endpoints: ServerEndpoints,
    /// Which transports to use.
    pub choice: TransportChoice,
    /// Connect at startup and keep connecting: on by default. Off, the game inserts
    /// `Reconnect::new(connect.0)` with the [`TransportConnect`] system when it wants to play.
    pub connect_at_startup: bool,
}

impl NetAhoyTransportClientPlugin {
    /// To the server at `endpoints`, WebTransport first, connecting at startup.
    pub fn new(endpoints: ServerEndpoints) -> Self {
        Self {
            endpoints,
            choice: TransportChoice::Auto,
            connect_at_startup: true,
        }
    }

    /// To the server and over the transports the settings name: natively `SERVER_URL`,
    /// `WT_HASH_URL` and `TRANSPORT` ([`ServerEndpoints::from_env`],
    /// [`TransportChoice::from_env`]), in a browser the page's address and its `?transport=`
    /// (`ServerEndpoints::from_page`, `TransportChoice::from_page`).
    pub fn from_settings() -> Self {
        #[cfg(not(target_family = "wasm"))]
        let (endpoints, choice) = (ServerEndpoints::from_env(), TransportChoice::from_env());
        #[cfg(target_family = "wasm")]
        let (endpoints, choice) = (ServerEndpoints::from_page(), TransportChoice::from_page());
        Self::new(endpoints).with_choice(choice)
    }

    /// The same plugin over the transports `choice` allows.
    pub fn with_choice(self, choice: TransportChoice) -> Self {
        Self { choice, ..self }
    }

    /// The same plugin connecting at startup or not.
    pub fn with_connect_at_startup(self, on: bool) -> Self {
        Self {
            connect_at_startup: on,
            ..self
        }
    }
}

/// The connect system: it opens a session over the transport the plan picks, or asks the server
/// for its offer first. [`Reconnect`] runs it; run it once more for the first connect.
#[derive(Resource, Clone, Copy, Debug)]
pub struct TransportConnect(pub SystemId);

impl Plugin for NetAhoyTransportClientPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins((
            WebSocketClientPlugin,
            WebTransportClientPlugin,
            AeronetRepliconClientPlugin,
        ))
        .insert_resource(self.endpoints.clone())
        .insert_resource(Transports::new(self.choice))
        .add_observer(use_replicon_for_session)
        .add_observer(session_started)
        .add_observer(session_ended)
        .add_systems(
            Update,
            (
                // Gone once the server refuses this client: nothing left to connect.
                finish_ask.run_if(resource_exists::<Reconnect>),
                time_out_attempts,
                listen,
            ),
        );
        let connect = app.register_system(connect_to_server);
        app.insert_resource(TransportConnect(connect));
        if self.connect_at_startup {
            app.insert_resource(Reconnect::new(connect)).add_systems(
                Startup,
                move |mut commands: Commands| {
                    commands.run_system(connect);
                },
            );
        }
    }
}

/// A handshake that hasn't finished by now (s) has failed: blocked UDP mostly hangs instead.
const CONNECT_TIMEOUT_SECS: f32 = 5.0;

/// The most of one frame (s) that counts toward a session's silence: a frozen loop (a hidden tab)
/// hears nothing, which says nothing about the link.
const MAX_FRAME_SECS: f32 = 0.25;

/// The browser holds at most this much unsent; the rest waits in aeronet's queue, visible as
/// `SendBacklog`.
const CLIENT_SEND_BUFFER_LIMIT: usize = 4 * 1024;

/// Which transport the next connect uses, and what decides it.
#[derive(Resource)]
struct Transports {
    choice: TransportChoice,
    offer: Ask,
    gave_up: Option<GaveUp>,
    /// When the last WebTransport session ended (s), if none has come up since.
    lost: Option<f64>,
    /// A WebTransport session has come up in this run, so a later failed handshake isn't
    /// blocked UDP.
    worked: bool,
}

enum Ask {
    Unasked,
    Asking {
        since: f64,
        task: Task<Result<Offer, String>>,
    },
    Answered(Option<Offer>),
}

impl Transports {
    fn new(choice: TransportChoice) -> Self {
        Self {
            choice,
            offer: Ask::Unasked,
            gave_up: None,
            lost: None,
            worked: false,
        }
    }

    /// What the next connect does at `now` (s).
    fn plan(&self, now: f64) -> Plan {
        let offer = match &self.offer {
            Ask::Unasked => OfferState::Unasked,
            Ask::Asking { since, .. } => OfferState::Asking(*since),
            Ask::Answered(Some(_)) => OfferState::Offered,
            Ask::Answered(None) => OfferState::NoOffer,
        };
        plan(now, self.choice, self.gave_up, self.lost, offer)
    }
}

/// Asks the server for its offer, on an IO thread.
fn ask_offer(endpoints: &ServerEndpoints) -> Task<Result<Offer, String>> {
    IoTaskPool::get().spawn(fetch::fetch_offer(endpoints.offer.clone()))
}

/// A WebTransport connect that isn't a session yet, and its timeout.
#[derive(Component)]
struct Attempt(Timer);

/// A WebTransport session's server (the run its offer came from), its silence and the ask for
/// the offer the silence started: that server answering it, running, while the session hears
/// nothing is UDP alone dying.
#[derive(Component)]
struct Heard {
    boot: u64,
    silence: Silence,
    ask: Option<Task<Result<Offer, String>>>,
}

/// Opens a session over the transport the plan picks, or none while the server's offer is on its
/// way (its answer runs this again).
fn connect_to_server(
    mut commands: Commands,
    mut transports: ResMut<Transports>,
    endpoints: Res<ServerEndpoints>,
    time: Res<Time<Real>>,
    sessions: Query<(), With<Transport>>,
) {
    // The answer to an ask and the reconnect loop can both run this in one frame.
    if !sessions.is_empty() {
        return;
    }
    let now = time.elapsed_secs_f64();
    match transports.plan(now) {
        Plan::Ask => {
            transports.offer = Ask::Asking {
                since: now,
                task: ask_offer(&endpoints),
            };
        }
        Plan::Wait => {}
        Plan::WebTransport => connect_webtransport(&mut commands, &mut transports, &endpoints),
        Plan::WebSocket => connect_websocket(&mut commands, &endpoints),
    }
}

/// Opens the WebSocket session. TCP redelivers, so only the newest command needs sending.
fn connect_websocket(commands: &mut Commands, endpoints: &ServerEndpoints) {
    commands
        .spawn((
            Name::new("Client"),
            Transport::WebSocket,
            LinkLossModel::Stream,
        ))
        .queue(WebSocketClient::connect(
            websocket_config(),
            endpoints.websocket.clone(),
        ));
    info!("websocket client connecting to {}", endpoints.websocket);
}

/// Opens a WebTransport session to the offer the server answered.
fn connect_webtransport(
    commands: &mut Commands,
    transports: &mut Transports,
    endpoints: &ServerEndpoints,
) {
    let Ask::Answered(Some(offer)) = &transports.offer else {
        return;
    };
    let hashes = match offer
        .hashes
        .iter()
        .map(|hash| cert::hash_from_b64(hash).map_err(|err| format!("{hash:?}: {err}")))
        .collect::<Result<Vec<_>, _>>()
    {
        Ok(hashes) if !hashes.is_empty() => hashes,
        Ok(_) => {
            warn!("webtransport offer: no certificate hash");
            transports.offer = Ask::Answered(None);
            return;
        }
        Err(err) => {
            warn!("webtransport offer: bad certificate hash {err}");
            transports.offer = Ask::Answered(None);
            return;
        }
    };
    let url = format!("https://{}:{}", endpoints.webtransport_host, offer.port);
    commands
        .spawn((
            Name::new("Client"),
            Transport::WebTransport,
            LinkLossModel::Datagram,
            Attempt(Timer::from_seconds(CONNECT_TIMEOUT_SECS, TimerMode::Once)),
            // Its silence counts once the session is up: `listen` reads sessions only.
            Heard {
                boot: offer.boot,
                silence: Silence::default(),
                ask: None,
            },
        ))
        .queue(WebTransportClient::connect(
            webtransport_config(hashes),
            url.clone(),
        ));
    info!("webtransport client connecting to {url}");
}

/// Takes the server's answer. An offer is what the connect waited for, so it runs the connect;
/// a failed ask waits for the reconnect loop, or a refused one would ask again every frame. An
/// ask that outlasts its wait is no offer, and runs the connect then: the reconnect loop's next
/// attempt can be seconds later.
fn finish_ask(
    mut transports: ResMut<Transports>,
    reconnect: Res<Reconnect>,
    time: Res<Time<Real>>,
    mut commands: Commands,
) {
    let Ask::Asking { task, .. } = &mut transports.offer else {
        return;
    };
    match check_ready(task) {
        Some(Ok(offer)) => {
            info!("webtransport offer: udp port {}", offer.port);
            transports.offer = Ask::Answered(Some(offer));
            commands.run_system(reconnect.connect);
        }
        Some(Err(why)) => {
            warn!("no webtransport offer: {why}");
            transports.offer = Ask::Answered(None);
        }
        None if transports.plan(time.elapsed_secs_f64()) != Plan::Wait => {
            warn!("no webtransport offer after {ASK_WAIT_SECS} s");
            transports.offer = Ask::Answered(None);
            commands.run_system(reconnect.connect);
        }
        None => {}
    }
}

fn time_out_attempts(
    time: Res<Time<Real>>,
    mut attempts: Query<(Entity, &mut Attempt)>,
    mut commands: Commands,
) {
    for (entity, mut attempt) in &mut attempts {
        if attempt.0.tick(time.delta()).just_finished() {
            // `session_ended` takes it as a failed handshake.
            commands.trigger(Disconnect::new(entity, "webtransport connect timed out"));
        }
    }
}

/// Follows each session's silence, and asks for the offer when it goes quiet.
fn listen(
    time: Res<Time<Real>>,
    clock: Option<Res<RemoteClock>>,
    rate: Res<TickRate>,
    endpoints: Res<ServerEndpoints>,
    mut sessions: Query<(&Session, &mut Heard)>,
) {
    let dt = f64::from(time.delta_secs().min(MAX_FRAME_SECS));
    // Nothing comes in while the session is silent, so this is the newest tick it heard.
    let heard_tick = clock.map_or(0, |clock| clock.latest_server_tick());
    for (session, mut heard) in &mut sessions {
        let heard = &mut *heard;
        if heard.silence.hear(session.stats.packets_recv.0, dt) {
            info!("webtransport quiet for {QUIET_SECS} s: asking the server over http");
            heard.ask = Some(ask_offer(&endpoints));
        } else if !heard.silence.asking() {
            // Heard again: an ask still out is about a silence that's over.
            heard.ask = None;
        }
        let Some(answer) = heard.ask.as_mut().and_then(check_ready) else {
            continue;
        };
        heard.ask = None;
        let hz = f64::from(rate.hz());
        let answer = Answer::read(answer.as_ref().ok(), heard.boot, heard_tick, hz);
        heard.silence.answered(answer);
        match answer {
            Answer::Running => {
                warn!("webtransport hears nothing while the server runs and answers over http")
            }
            Answer::Frozen => {
                warn!("webtransport hears nothing, and the server's tick stands still: it froze")
            }
            Answer::Restarted => info!("webtransport hears nothing: the server restarted"),
            Answer::Unanswered => info!("webtransport hears nothing, and http gets no answer"),
        }
    }
}

/// Every session this plugin opens carries replicon.
fn use_replicon_for_session(
    session: On<Add, SessionEndpoint>,
    ours: Query<(), With<Transport>>,
    mut commands: Commands,
) {
    let session = session.event_target();
    if ours.contains(session) {
        commands.entity(session).insert(AeronetRepliconClient);
    }
}

/// Logs the session coming up, and over what; a WebTransport one is no longer an attempt.
fn session_started(
    session: On<Add, Session>,
    transports_of: Query<&Transport>,
    mut transports: ResMut<Transports>,
    mut commands: Commands,
) {
    let session = session.event_target();
    let Ok(transport) = transports_of.get(session) else {
        return;
    };
    info!("connected over {}: {session}", transport.name());
    transports.lost = None;
    if *transport == Transport::WebTransport {
        transports.worked = true;
        commands.entity(session).remove::<Attempt>();
    }
}

/// A failed handshake falls back to WebSocket at once (unless WebTransport is pinned), and a
/// session whose UDP alone died gives WebTransport up for now; a WebTransport session that ended
/// leaves what the next connect's plan needs to know.
fn session_ended(
    disconnected: On<Disconnected>,
    sessions: Query<(&Transport, Has<Attempt>, Option<&Heard>)>,
    mut transports: ResMut<Transports>,
    endpoints: Res<ServerEndpoints>,
    time: Res<Time<Real>>,
    mut commands: Commands,
) {
    let Ok((transport, attempt, heard)) = sessions.get(disconnected.event_target()) else {
        return;
    };
    let name = transport.name();
    match &disconnected.reason {
        DisconnectReason::ByUser(reason) => info!("{name} client disconnected: {reason}"),
        DisconnectReason::ByPeer(reason) => warn!("{name} client disconnected by server: {reason}"),
        DisconnectReason::ByError(err) => warn!("{name} client disconnected: {err:#}"),
    }
    let now = time.elapsed_secs_f64();
    // A restarted server has a new certificate: the next connect asks again.
    transports.offer = Ask::Unasked;
    if *transport == Transport::WebSocket {
        return;
    }
    if attempt {
        if transports.choice == TransportChoice::WebTransport {
            warn!("webtransport connect failed, trying it again (webtransport only)");
            return;
        }
        warn!("webtransport connect failed, falling back to websocket");
        transports.gave_up = Some(if transports.worked {
            GaveUp::Until(now + GIVE_UP_SECS)
        } else {
            GaveUp::ForTheRun
        });
        connect_websocket(&mut commands, &endpoints);
        return;
    }
    if transports.choice == TransportChoice::Auto
        && heard.is_some_and(|heard| heard.silence.udp_died())
    {
        warn!("webtransport went quiet while the server answered over http: websocket for now");
        transports.gave_up = Some(GaveUp::Until(now + GIVE_UP_SECS));
    }
    // The first end, if connects keep failing.
    transports.lost.get_or_insert(now);
}

#[cfg(target_family = "wasm")]
fn websocket_config() -> aeronet_websocket::client::ClientConfig {
    aeronet_websocket::client::ClientConfig::default()
        .with_send_buffer_limit(Some(CLIENT_SEND_BUFFER_LIMIT))
}

/// Nagle off, as browsers have it on WebSockets: with it on, small commands wait for the previous
/// segment's ack and leave once per round trip, in clumps, and the server runs on guessed input
/// in between. Native clients connect over plain `ws://`.
#[cfg(not(target_family = "wasm"))]
fn websocket_config() -> aeronet_websocket::client::ClientConfig {
    aeronet_websocket::client::ClientConfig::builder()
        .with_no_encryption()
        .disable_nagle()
        .with_send_buffer_limit(Some(CLIENT_SEND_BUFFER_LIMIT))
}

/// Trusts the server's certificate by its hash alone: any of `hashes`.
#[cfg(not(target_family = "wasm"))]
fn webtransport_config(hashes: Vec<CertificateHash>) -> aeronet_webtransport::client::ClientConfig {
    use aeronet_webtransport::wtransport::tls::Sha256Digest;
    aeronet_webtransport::client::ClientConfig::builder()
        .with_bind_default()
        .with_server_certificate_hashes(hashes.into_iter().map(Sha256Digest::new))
        .build()
}

/// Trusts the server's certificate by its hash alone: any of `hashes`.
#[cfg(target_family = "wasm")]
fn webtransport_config(hashes: Vec<CertificateHash>) -> aeronet_webtransport::client::ClientConfig {
    use aeronet_webtransport::xwt_web::{
        CertificateHash as WebHash, CongestionControl, HashAlgorithm,
    };
    aeronet_webtransport::client::ClientConfig {
        server_certificate_hashes: hashes
            .into_iter()
            .map(|hash| WebHash {
                algorithm: HashAlgorithm::Sha256,
                value: hash.to_vec(),
            })
            .collect(),
        // A hint for the browsers that read it; Chrome runs CUBIC whatever it says.
        congestion_control: CongestionControl::LowLatency,
        // HTTP/3 or fail: pooled onto HTTP/2 the session would have no datagrams.
        require_unreliable: true,
        ..default()
    }
}
