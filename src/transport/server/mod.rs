//! The server's listeners: WebSocket, WebTransport beside it, and the offer endpoint that tells
//! clients how to reach the WebTransport one.

mod certificate;
mod offer;
mod quic_window;
mod rotation;

use std::{
    hash::{BuildHasher, RandomState},
    net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener},
    str::FromStr,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use aeronet::io::{
    connection::LocalAddr,
    server::{CloseReason, Closed, Server},
};
use aeronet_replicon::server::{AeronetRepliconServer, AeronetRepliconServerPlugin};
use aeronet_websocket::server::{
    ServerConfig, WebSocketServer, WebSocketServerClient, WebSocketServerPlugin,
};
use aeronet_webtransport::server::{
    SessionRequest, SessionResponse, WebTransportServer, WebTransportServerClient,
    WebTransportServerPlugin,
};
use bevy::prelude::*;
use wtransport::config::QuicTransportConfig;

use super::{DEFAULT_OFFER_PORT, DEFAULT_WEBSOCKET_PORT, DEFAULT_WEBTRANSPORT_PORT, Transport};
use crate::ServerTick;
use certificate::Certificates;
use offer::Offers;
use quic_window::FixedWindow;

/// Where the server listens.
#[derive(Resource, Clone, Copy, Debug, PartialEq, Eq)]
pub struct ServerTransportConfig {
    /// The WebSocket listener (TCP, plain `ws://`).
    pub websocket: SocketAddr,
    /// The WebTransport listener (UDP). QUIC doesn't go through an HTTP proxy in front of the
    /// WebSocket, so a server behind one listens for WebTransport on the network itself.
    pub webtransport: SocketAddr,
    /// The offer endpoint (TCP, plain HTTP).
    pub offer: SocketAddr,
}

impl Default for ServerTransportConfig {
    /// All three on loopback, on the default ports.
    fn default() -> Self {
        Self::on_ip(IpAddr::V4(Ipv4Addr::LOCALHOST))
    }
}

impl ServerTransportConfig {
    /// All three on `ip`, on [`DEFAULT_WEBSOCKET_PORT`], [`DEFAULT_WEBTRANSPORT_PORT`] and
    /// [`DEFAULT_OFFER_PORT`].
    pub fn on_ip(ip: IpAddr) -> Self {
        Self {
            websocket: SocketAddr::new(ip, DEFAULT_WEBSOCKET_PORT),
            webtransport: SocketAddr::new(ip, DEFAULT_WEBTRANSPORT_PORT),
            offer: SocketAddr::new(ip, DEFAULT_OFFER_PORT),
        }
    }

    /// The defaults, moved by `SERVER_BIND_ADDR` (the WebSocket's; WebTransport listens on its
    /// IP), `WT_PORT` (WebTransport's port), `WT_BIND_ADDR` (WebTransport anywhere, and
    /// `WT_PORT` is then unused) and `WT_HASH_ADDR` (the offer's, which stays on loopback
    /// otherwise: a proxy in front of the server forwards to it). Panics on a setting that
    /// doesn't parse.
    pub fn from_env() -> Self {
        Self::from_settings(|name| std::env::var(name).ok())
    }

    /// The defaults, moved by the settings `setting` returns by name.
    fn from_settings(setting: impl Fn(&str) -> Option<String>) -> Self {
        let defaults = Self::default();
        let addr = |name| parsed(&setting, name, "<ip>:<port>");
        let websocket = addr("SERVER_BIND_ADDR").unwrap_or(defaults.websocket);
        let port = parsed(&setting, "WT_PORT", "a port").unwrap_or(DEFAULT_WEBTRANSPORT_PORT);
        Self {
            websocket,
            webtransport: addr("WT_BIND_ADDR").unwrap_or(SocketAddr::new(websocket.ip(), port)),
            offer: addr("WT_HASH_ADDR").unwrap_or(defaults.offer),
        }
    }
}

/// The setting `name` parsed, if it is set; panics if it isn't the `expected` kind.
fn parsed<T: FromStr>(
    setting: &impl Fn(&str) -> Option<String>,
    name: &str,
    expected: &str,
) -> Option<T> {
    let raw = setting(name)?;
    match raw.trim().parse() {
        Ok(value) => Some(value),
        Err(_) => panic!("{name}={raw:?}: expected {expected}"),
    }
}

/// Listens over WebSocket and WebTransport, both carrying replicon, and answers the offer once
/// WebTransport listens: a server that can't have its UDP port sends its clients straight to
/// WebSocket, not to a handshake that times out. It adds aeronet's WebSocket and WebTransport
/// server plugins and replicon's aeronet backend, and opens the listeners at startup. Every
/// WebTransport session request is accepted, and every client's entity carries its
/// [`Transport`].
#[derive(Default)]
pub struct NetAhoyTransportServerPlugin {
    /// Where to listen.
    pub config: ServerTransportConfig,
}

impl NetAhoyTransportServerPlugin {
    /// Listening on `config`'s addresses.
    pub fn new(config: ServerTransportConfig) -> Self {
        Self { config }
    }
}

impl Plugin for NetAhoyTransportServerPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins((
            WebSocketServerPlugin,
            WebTransportServerPlugin,
            AeronetRepliconServerPlugin,
        ))
        .insert_resource(self.config)
        .init_resource::<SharedTick>()
        .add_observer(accept_webtransport_session)
        .add_observer(tag_client::<WebSocketServerClient>(Transport::WebSocket))
        .add_observer(tag_client::<WebTransportServerClient>(
            Transport::WebTransport,
        ))
        .add_systems(Startup, open_listeners)
        .add_systems(FixedPostUpdate, share_tick);
    }
}

/// The server's tick, for the offer endpoint's threads: its loop writes it, and a frozen loop
/// leaves it standing.
#[derive(Resource, Default)]
struct SharedTick(Arc<AtomicU64>);

fn share_tick(tick: Res<ServerTick>, shared: Res<SharedTick>) {
    shared.0.store(tick.0, Ordering::Relaxed);
}

/// Gives each client that connects over the transport marked by `C` its [`Transport`].
fn tag_client<C: Component>(
    transport: Transport,
) -> impl Fn(On<Add, C>, Commands) + Send + Sync + 'static {
    move |client: On<Add, C>, mut commands: Commands| {
        commands.entity(client.event_target()).insert(transport);
    }
}

fn open_listeners(
    mut commands: Commands,
    config: Res<ServerTransportConfig>,
    tick: Res<SharedTick>,
) {
    let config = *config;
    commands
        .spawn((Name::new("WebSocket server"), AeronetRepliconServer))
        .queue(WebSocketServer::open(
            ServerConfig::builder()
                .with_bind_address(config.websocket)
                .with_no_encryption(),
        ));
    info!("websocket server listening on ws://{}", config.websocket);

    let certificates = Arc::new(Certificates::new());
    let offers = Arc::new(Offers {
        certificates: certificates.clone(),
        port: config.webtransport.port(),
        // std's hash keys are random per process: a number no other run of the server has.
        boot: RandomState::new().hash_one(std::process::id()),
        tick: tick.0.clone(),
    });
    let webtransport = wtransport::ServerConfig::builder()
        .with_bind_address(config.webtransport)
        .with_custom_tls_and_transport(
            Certificates::tls_config(certificates),
            webtransport_quic_config(),
        )
        .build();
    commands
        .spawn((Name::new("WebTransport server"), AeronetRepliconServer))
        .queue(WebTransportServer::open(webtransport))
        // Offered only once it listens.
        .observe(move |opened: On<Add, Server>, addrs: Query<&LocalAddr>| {
            if let Ok(addr) = addrs.get(opened.event_target()) {
                info!("webtransport server listening on udp {}", **addr);
            }
            serve_offer(config.offer, offers.clone());
        })
        .observe(|closed: On<Closed>| match &closed.reason {
            CloseReason::ByUser(reason) => warn!("webtransport server closed: {reason}"),
            CloseReason::ByError(err) => warn!("webtransport server closed: {err:#}"),
        });
}

/// Answers the offer on `addr`, or says why it can't: its clients then get no offer, and connect
/// over WebSocket.
fn serve_offer(addr: SocketAddr, offers: Arc<Offers>) {
    let served = TcpListener::bind(addr)
        .and_then(|listener| offer::serve(listener, offers))
        .map_err(|err| warn!("webtransport offer: can't listen on {addr}: {err}"));
    if served.is_ok() {
        info!("webtransport offer on http://{addr}");
    }
}

/// quinn's defaults (Cubic, a 1 MiB datagram send buffer) let a lossy link's collapsed window
/// hold snapshots back, a megabyte of them, so the client gets a stale trickle.
fn webtransport_quic_config() -> QuicTransportConfig {
    let mut config = QuicTransportConfig::default();
    // A window no loss shrinks: BBR's RTT probe cut it to a few packets in join bursts.
    config.congestion_controller_factory(Arc::new(FixedWindow::default()));
    // quinn drops the oldest queued datagram past this, so one held back is never long stale.
    config.datagram_send_buffer_size(32 * 1024);
    // A ping after 2 s with nothing sent, so no quiet moment idles a session out; a client
    // unheard for 10 s is gone, and its session ends then rather than at quinn's 30 s.
    config.keep_alive_interval(Some(Duration::from_secs(2)));
    config.max_idle_timeout(Some(
        Duration::from_secs(10)
            .try_into()
            .expect("10 s is a valid idle timeout"),
    ));
    config
}

fn accept_webtransport_session(mut request: On<SessionRequest>) {
    request.respond(SessionResponse::Accepted);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(settings: &[(&str, &str)]) -> ServerTransportConfig {
        ServerTransportConfig::from_settings(|name| {
            settings
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| (*value).to_owned())
        })
    }

    fn addr(addr: &str) -> SocketAddr {
        addr.parse().unwrap()
    }

    #[test]
    fn no_settings_is_loopback_on_the_default_ports() {
        assert_eq!(config(&[]), ServerTransportConfig::default());
        assert_eq!(config(&[]).webtransport, addr("127.0.0.1:5001"));
    }

    #[test]
    fn webtransport_follows_the_websocket_ip_and_the_offer_stays_on_loopback() {
        let moved = config(&[("SERVER_BIND_ADDR", "0.0.0.0:6000"), ("WT_PORT", " 6001 ")]);
        assert_eq!(moved.websocket, addr("0.0.0.0:6000"));
        assert_eq!(moved.webtransport, addr("0.0.0.0:6001"));
        assert_eq!(moved.offer, addr("127.0.0.1:5002"));
    }

    #[test]
    fn a_webtransport_address_wins_over_its_port() {
        let moved = config(&[
            ("WT_PORT", "6001"),
            ("WT_BIND_ADDR", "[::]:7001"),
            ("WT_HASH_ADDR", "127.0.0.1:7002"),
        ]);
        assert_eq!(moved.websocket, addr("127.0.0.1:5000"));
        assert_eq!(moved.webtransport, addr("[::]:7001"));
        assert_eq!(moved.offer, addr("127.0.0.1:7002"));
    }

    #[test]
    #[should_panic(expected = "WT_PORT=\"udp\": expected a port")]
    fn a_setting_that_does_not_parse_is_refused() {
        config(&[("WT_PORT", "udp")]);
    }
}
