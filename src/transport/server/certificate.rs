//! The server's WebTransport certificate: self-signed, trusted by its hash, and replaced before it
//! expires without touching open sessions.
//!
//! A browser trusts a certificate by its hash only while it is valid at most 14 days, so the
//! server makes its own and hands out the hash with the offer. Each handshake asks
//! [`Certificates`] which certificate to present, and TLS checks it only in the handshake, so a
//! new one serves the sessions that start after it while the open ones run on. It is made when a
//! client asks for the offer and the old one has a day left by the wall clock, and presented a
//! minute later ([`super::rotation`]).

use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime},
};

use aeronet_webtransport::cert;
use bevy::log::info;
use wtransport::{
    Identity,
    tls::{
        WEBTRANSPORT_ALPN,
        rustls::{
            self,
            crypto::CryptoProvider,
            pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer},
            server::{ClientHello, ResolvesServerCert},
            sign::CertifiedKey,
        },
        self_signed::time::OffsetDateTime,
    },
};

use super::rotation::Rotation;

/// How long a certificate is valid: the most a browser takes for one trusted by its hash.
const VALID: Duration = Duration::from_secs(14 * 24 * 3600);

/// Its validity starts this long before it is made, for clients whose clock runs behind.
const BACKDATE: Duration = Duration::from_secs(3600);

/// The certificate the WebTransport listener presents, and the hashes for the offer.
#[derive(Debug)]
pub(super) struct Certificates {
    provider: CryptoProvider,
    rotation: Mutex<Rotation<Certificate>>,
}

#[derive(Debug)]
struct Certificate {
    key: Arc<CertifiedKey>,
    hash: String,
}

impl Certificates {
    /// Starts on a certificate made now.
    pub(super) fn new() -> Self {
        let provider = rustls::crypto::ring::default_provider();
        let (first, ends) = Certificate::new(&provider);
        let rotation = Mutex::new(Rotation::new(first, ends));
        Self { provider, rotation }
    }

    /// The hashes to offer: the presented certificate's, and the next one's while it waits to
    /// take over. One with a day left gets its successor first.
    pub(super) fn offer_hashes(&self) -> Vec<String> {
        let mut rotation = self.rotation.lock().unwrap();
        rotation
            .offered(SystemTime::now(), Instant::now(), || {
                Certificate::new(&self.provider)
            })
            .map(|certificate| certificate.hash.clone())
            .collect()
    }

    /// The listener's TLS: wtransport's own (TLS 1.3, HTTP/3's ALPN), presenting whichever
    /// certificate is current.
    pub(super) fn tls_config(certificates: Arc<Self>) -> rustls::ServerConfig {
        let mut config =
            rustls::ServerConfig::builder_with_provider(Arc::new(certificates.provider.clone()))
                .with_protocol_versions(&[&rustls::version::TLS13])
                .expect("ring has TLS 1.3")
                .with_no_client_auth()
                .with_cert_resolver(certificates);
        config.alpn_protocols = vec![WEBTRANSPORT_ALPN.to_vec()];
        config
    }
}

impl ResolvesServerCert for Certificates {
    fn resolve(&self, _: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        let mut rotation = self.rotation.lock().unwrap();
        Some(rotation.presented(Instant::now()).key.clone())
    }
}

impl Certificate {
    /// A new certificate, and when it expires.
    fn new(provider: &CryptoProvider) -> (Self, SystemTime) {
        let not_before = OffsetDateTime::now_utc() - BACKDATE;
        let not_after = not_before + VALID;
        let identity = Identity::self_signed_builder()
            .subject_alt_names(["localhost", "127.0.0.1", "::1"])
            .not_before(not_before)
            .not_after(not_after)
            .build()
            .expect("the names are valid");
        let chain = identity.certificate_chain().as_slice();
        let hash = cert::hash_to_b64(chain[0].hash());
        let key = CertifiedKey::from_der(
            chain
                .iter()
                .map(|certificate| CertificateDer::from(certificate.der().to_vec()))
                .collect(),
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(
                identity.private_key().secret_der().to_vec(),
            )),
            provider,
        )
        .expect("the key was made for this certificate");
        info!("webtransport certificate {hash}: valid {not_before} to {not_after}");
        let certificate = Self {
            key: Arc::new(key),
            hash,
        };
        (certificate, not_after.into())
    }
}
