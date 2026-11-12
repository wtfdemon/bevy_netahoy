//! Sessions for a game that plays in the browser too: WebTransport first, WebSocket where it
//! fails. Behind the features `transport-client` and `transport-server`.
//!
//! WebTransport's datagrams don't stall on a loss the way TCP does, and each command packet
//! repeats the commands the server hasn't acked, so a lost one is filled by the next (a
//! WebTransport session is [`LinkLossModel::Datagram`](crate::LinkLossModel), a WebSocket one
//! `Stream`). Some links block UDP, so a client that can't get through falls back to WebSocket.
//!
//! # Wiring it up
//!
//! The server, native only, with [`NetAhoyServerPlugin`](crate::NetAhoyServerPlugin) and in
//! place of aeronet's server plugins:
//!
//! ```ignore
//! app.add_plugins((
//!     NetAhoyServerPlugin::<MyGame>::default(),
//!     NetAhoyTransportServerPlugin::new(ServerTransportConfig::from_env()),
//! ));
//! ```
//!
//! It adds aeronet's WebSocket and WebTransport server plugins and replicon's aeronet backend,
//! and at startup opens both listeners and the offer endpoint on the addresses in its
//! [`ServerTransportConfig`]. Every WebTransport session request is accepted: replicon's protocol
//! check and the game's [`JoinRequest`](crate::JoinRequest) observer decide who plays. Each
//! client's entity carries its [`Transport`].
//!
//! A client, native or in the browser, with [`NetAhoyClientPlugin`](crate::NetAhoyClientPlugin)
//! and in place of aeronet's client plugins:
//!
//! ```ignore
//! app.add_plugins((
//!     NetAhoyClientPlugin::<MyGame>::default(),
//!     NetAhoyTransportClientPlugin::from_settings(),
//! ));
//! ```
//!
//! It connects at startup to its [`ServerEndpoints`] and inserts
//! [`Reconnect`](crate::Reconnect) with its connect system, so netahoy reconnects whenever the
//! link drops. Every session carries replicon and its [`Transport`]. A game that connects later
//! (from a menu) turns [`NetAhoyTransportClientPlugin::connect_at_startup`] off and inserts
//! `Reconnect::new(connect.0)` from the [`TransportConnect`] resource when it wants to play;
//! removing `Reconnect` stops it reconnecting.
//!
//! # Ports and addresses
//!
//! By default a server listens on loopback: WebSocket on TCP [`DEFAULT_WEBSOCKET_PORT`] (5000),
//! WebTransport on UDP [`DEFAULT_WEBTRANSPORT_PORT`] (5001) and the offer on TCP
//! [`DEFAULT_OFFER_PORT`] (5002). The WebSocket is plain `ws://`, and the offer plain HTTP: a
//! page served over https reaches both through a proxy that terminates TLS, at `ws` and `wt`
//! beside the page. QUIC doesn't go through an HTTP proxy, so
//! WebTransport is dialled on the server's host itself, on the offer's port.
//! [`ServerEndpoints`] has the client's side of this: [`ServerEndpoints::on_host`] for a server
//! on the default ports, [`ServerEndpoints::for_page`] for a page's address.
//!
//! # The offer and the certificate
//!
//! The server makes its own self-signed certificate and answers its SHA-256 hash, its UDP port,
//! a number for this run of the server and its tick as JSON (an [`Offer`]) on the offer
//! endpoint. The client trusts the certificate by that hash, so there is no certificate to
//! install. Browsers allow that only for a certificate valid at most 14 days: each is valid 14
//! days from an hour before it was made (for clients whose clock runs behind), and the first
//! ask for the offer when one has a day left makes the next. Handshakes get the new one a minute
//! later, and meanwhile the offer lists both, so a client that asked just before still meets the
//! certificate it was told of. Open sessions keep the one they started on, so a server can run
//! for as long as it likes.
//!
//! # When a client falls back
//!
//! [`TransportChoice::Auto`], the default:
//!
//! - A first handshake that fails or hangs for 5 s means WebSocket for the rest of the run: UDP
//!   doesn't get through here.
//! - A session that ended mostly says nothing about UDP: a restarted server or a kick ends it on a
//!   good path. So the next connect asks for the offer again (a restarted server has a new
//!   certificate) and goes back to WebTransport.
//! - Unless UDP alone died: a session that has heard nothing for 2 s asks the server for its
//!   offer over HTTP, and the same run of the server answering with its tick moved on, while the
//!   session hears nothing for a second more, is UDP dying. A whole link going down loses the ask
//!   too, and a frozen server's tick stands still, so neither looks like that. Reconnects within
//!   60 s of it go over WebSocket, as do those within 60 s of a handshake failing after one
//!   worked.
//! - No session ever moves to the other transport: a WebSocket session runs until it ends, and
//!   only a reconnect a minute or more after WebTransport was given up tries it again.
//!
//! [`TransportChoice::WebTransport`] and [`TransportChoice::WebSocket`] pin one, with no fallback.
//!
//! # Settings
//!
//! [`ServerTransportConfig::from_env`], on the server:
//!
//! | variable | |
//! |---|---|
//! | `SERVER_BIND_ADDR=<ip>:<port>` | the WebSocket listener (default `127.0.0.1:5000`); WebTransport listens on its IP |
//! | `WT_PORT=<port>` | the WebTransport UDP port (default 5001) |
//! | `WT_BIND_ADDR=<ip>:<port>` | where WebTransport listens instead of `SERVER_BIND_ADDR`'s IP and `WT_PORT`, e.g. `[::]:5001` for every interface while the WebSocket sits on loopback behind a proxy |
//! | `WT_HASH_ADDR=<ip>:<port>` | the offer endpoint (default `127.0.0.1:5002`) |
//!
//! [`NetAhoyTransportClientPlugin::from_settings`], on a client: natively
//! [`ServerEndpoints::from_env`] and [`TransportChoice::from_env`], in a browser
//! `ServerEndpoints::from_page` and `TransportChoice::from_page`, which read the page's address
//! and its query:
//!
//! | variable | page | |
//! |---|---|---|
//! | `TRANSPORT=auto\|wt\|ws` | `?transport=` | WebTransport first with the fallback (`auto`, the default), or only one of them |
//! | `SERVER_URL=<ws url>` | | the WebSocket (default `ws://127.0.0.1:5000`); WebTransport dials its host |
//! | `WT_HASH_URL=<http url>` | | the offer (default port 5002 on `SERVER_URL`'s host) |
//!
//! # Dependencies
//!
//! Patches apply only in the root workspace, so a game copies the `[patch.crates-io]` block for
//! aeronet from netahoy's `Cargo.toml` into its own: the transport plugins run on the same
//! aeronet fork as the rest of netahoy.

use bevy::prelude::*;
use serde::{Deserialize, Serialize};

#[cfg(feature = "transport-client")]
mod client;
#[cfg(feature = "transport-client")]
mod rules;
#[cfg(all(feature = "transport-server", not(target_family = "wasm")))]
mod server;

#[cfg(feature = "transport-client")]
pub use client::{NetAhoyTransportClientPlugin, ServerEndpoints, TransportConnect};
#[cfg(feature = "transport-client")]
pub use rules::TransportChoice;
#[cfg(all(feature = "transport-server", not(target_family = "wasm")))]
pub use server::{NetAhoyTransportServerPlugin, ServerTransportConfig};

/// The TCP port a server's WebSocket listens on unless told otherwise.
pub const DEFAULT_WEBSOCKET_PORT: u16 = 5000;

/// The UDP port a server's WebTransport listens on unless told otherwise.
pub const DEFAULT_WEBTRANSPORT_PORT: u16 = 5001;

/// The TCP port a server answers its [`Offer`] on unless told otherwise.
pub const DEFAULT_OFFER_PORT: u16 = 5002;

/// The transport a session runs on: on each session a client opens, and on each client's entity
/// on the server.
#[derive(Component, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Transport {
    /// Over TCP: a lost packet holds back everything behind it until it is resent.
    WebSocket,
    /// Over QUIC datagrams: a lost packet is gone, and nothing waits for it.
    WebTransport,
}

impl Transport {
    /// Its name in log lines: `websocket` or `webtransport`.
    pub fn name(self) -> &'static str {
        match self {
            Self::WebSocket => "websocket",
            Self::WebTransport => "webtransport",
        }
    }
}

/// What the server's offer endpoint answers, as JSON: how to reach its WebTransport listener,
/// and which server is answering.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Offer {
    /// The SHA-256 (DER, base 64) of each certificate a handshake may meet now: the one presented,
    /// and for a minute after a renewal the next one too. The client trusts any of them.
    pub hashes: Vec<String>,
    /// The WebTransport listener's UDP port, on the host the client reaches the server at.
    pub port: u16,
    /// This run of the server: a restarted one answers another number.
    pub boot: u64,
    /// The server's tick when it answered: a frozen server's stands still.
    pub tick: u64,
}
