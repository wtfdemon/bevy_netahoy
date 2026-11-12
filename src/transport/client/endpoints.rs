//! Where a client finds the server: its WebSocket, its offer, and the host its WebTransport
//! listener is on.

use bevy::prelude::*;

use crate::transport::{DEFAULT_OFFER_PORT, DEFAULT_WEBSOCKET_PORT};

/// The server's addresses, as a client reaches them.
#[derive(Resource, Clone, Debug, PartialEq, Eq)]
pub struct ServerEndpoints {
    /// The WebSocket URL: `ws://<host>:<port>`, or `wss://...` through a proxy.
    pub websocket: String,
    /// Where the [`Offer`](crate::transport::Offer) is asked for: `http://...`, or `https://...`
    /// through a proxy (in a browser only: a native client asks over plain HTTP).
    pub offer: String,
    /// The host the WebTransport session dials, on the offer's UDP port. QUIC doesn't go through
    /// an HTTP proxy, so this is the server's host itself.
    pub webtransport_host: String,
}

impl ServerEndpoints {
    /// A server on `host` (a name or an IP, an IPv6 one in brackets) on the default ports, over
    /// plain `ws://` and `http://`.
    pub fn on_host(host: &str) -> Self {
        Self {
            websocket: format!("ws://{host}:{DEFAULT_WEBSOCKET_PORT}"),
            offer: format!("http://{host}:{DEFAULT_OFFER_PORT}/"),
            webtransport_host: dial_host(host),
        }
    }

    /// `SERVER_URL` (default `ws://127.0.0.1:5000`) and `WT_HASH_URL` (default
    /// `http://<SERVER_URL's host>:5002/`). WebTransport dials `SERVER_URL`'s host.
    #[cfg(not(target_family = "wasm"))]
    pub fn from_env() -> Self {
        Self::from_settings(
            std::env::var("SERVER_URL").ok(),
            std::env::var("WT_HASH_URL").ok(),
        )
    }

    /// The server a page at `url` plays on. A page served over https plays on the server behind
    /// the same proxy: its WebSocket at `ws` beside the page and its offer at `wt`. One served
    /// over plain http plays on the page's host, on the default ports. Either way WebTransport
    /// dials the page's host.
    pub fn for_page(url: &str) -> Self {
        let (authority, rest) = split_authority(url);
        let host = match host_of(authority) {
            "" => "127.0.0.1",
            host => host,
        };
        if !url.starts_with("https://") {
            return Self::on_host(host);
        }
        let path = rest.split(['?', '#']).next().unwrap_or_default();
        let dir = path.rfind('/').map_or("/", |end| &path[..=end]);
        Self {
            websocket: format!("wss://{authority}{dir}ws"),
            offer: format!("https://{authority}{dir}wt"),
            webtransport_host: dial_host(host),
        }
    }

    /// The server the page this client runs in plays on: [`Self::for_page`] at the page's
    /// address.
    #[cfg(target_family = "wasm")]
    pub fn from_page() -> Self {
        let href = web_sys::window().and_then(|window| window.location().href().ok());
        Self::for_page(&href.unwrap_or_default())
    }

    /// The WebSocket at `websocket`, else 127.0.0.1's, and the offer at `offer`, else on the
    /// WebSocket's host.
    #[cfg(not(target_family = "wasm"))]
    fn from_settings(websocket: Option<String>, offer: Option<String>) -> Self {
        let websocket =
            websocket.unwrap_or_else(|| format!("ws://127.0.0.1:{DEFAULT_WEBSOCKET_PORT}"));
        let host = host_of(split_authority(&websocket).0).to_owned();
        Self {
            offer: offer.unwrap_or_else(|| format!("http://{host}:{DEFAULT_OFFER_PORT}/")),
            webtransport_host: dial_host(&host),
            websocket,
        }
    }
}

/// Chrome dials QUIC to `localhost` on ::1 alone, where TCP falls back to 127.0.0.1, and a server
/// listens for WebTransport on its WebSocket's IP, by default 127.0.0.1.
fn dial_host(host: &str) -> String {
    match host {
        "localhost" => "127.0.0.1".to_owned(),
        host => host.to_owned(),
    }
}

/// The authority of `url` (`host:port`) and what follows it, the scheme dropped.
fn split_authority(url: &str) -> (&str, &str) {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    rest.split_at(rest.find(['/', '?', '#']).unwrap_or(rest.len()))
}

/// The host of an authority, its port dropped; an IPv6 literal keeps its brackets.
fn host_of(authority: &str) -> &str {
    match authority.rfind(']') {
        Some(end) => &authority[..=end],
        None => authority
            .rsplit_once(':')
            .map_or(authority, |(host, _)| host),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_host_gets_the_default_ports() {
        let endpoints = ServerEndpoints::on_host("10.0.0.2");
        assert_eq!(endpoints.websocket, "ws://10.0.0.2:5000");
        assert_eq!(endpoints.offer, "http://10.0.0.2:5002/");
        assert_eq!(endpoints.webtransport_host, "10.0.0.2");
    }

    #[test]
    fn localhost_is_dialled_on_ipv4() {
        let endpoints = ServerEndpoints::on_host("localhost");
        assert_eq!(endpoints.websocket, "ws://localhost:5000");
        assert_eq!(endpoints.webtransport_host, "127.0.0.1");
    }

    #[test]
    fn the_host_comes_out_of_an_authority() {
        assert_eq!(host_of("10.0.0.2:6000"), "10.0.0.2");
        assert_eq!(host_of("example.com"), "example.com");
        assert_eq!(host_of("[::1]:5000"), "[::1]");
        assert_eq!(host_of("[::1]"), "[::1]");
        assert_eq!(
            split_authority("wss://example.com/game/ws"),
            ("example.com", "/game/ws")
        );
        assert_eq!(split_authority("http://h:1?x"), ("h:1", "?x"));
    }

    #[cfg(not(target_family = "wasm"))]
    #[test]
    fn the_settings_default_to_a_server_on_loopback() {
        assert_eq!(
            ServerEndpoints::from_settings(None, None),
            ServerEndpoints::on_host("127.0.0.1")
        );
    }

    #[cfg(not(target_family = "wasm"))]
    #[test]
    fn the_offer_follows_the_websocket_host_unless_it_is_set() {
        let moved = ServerEndpoints::from_settings(Some("ws://[::1]:6000".into()), None);
        assert_eq!(moved.offer, "http://[::1]:5002/");
        assert_eq!(moved.webtransport_host, "[::1]");
        let both = ServerEndpoints::from_settings(
            Some("ws://localhost:6000".into()),
            Some("http://10.0.0.9:7000/offer".into()),
        );
        assert_eq!(both.websocket, "ws://localhost:6000");
        assert_eq!(both.offer, "http://10.0.0.9:7000/offer");
        assert_eq!(both.webtransport_host, "127.0.0.1");
    }

    #[test]
    fn a_page_over_https_plays_behind_its_proxy() {
        let endpoints =
            ServerEndpoints::for_page("https://play.example.com:9105/game/index.html?transport=ws");
        assert_eq!(endpoints.websocket, "wss://play.example.com:9105/game/ws");
        assert_eq!(endpoints.offer, "https://play.example.com:9105/game/wt");
        assert_eq!(endpoints.webtransport_host, "play.example.com");
        let root = ServerEndpoints::for_page("https://play.example.com");
        assert_eq!(root.websocket, "wss://play.example.com/ws");
        assert_eq!(root.offer, "https://play.example.com/wt");
    }

    #[test]
    fn a_page_over_http_plays_on_its_host() {
        assert_eq!(
            ServerEndpoints::for_page("http://localhost:8080/"),
            ServerEndpoints::on_host("localhost")
        );
        assert_eq!(
            ServerEndpoints::for_page("file:///home/me/web/index.html"),
            ServerEndpoints::on_host("127.0.0.1")
        );
    }
}
