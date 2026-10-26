//! A relay on loopback with a certificate made for the test.

use std::net::Ipv4Addr;
use std::sync::Arc;

use iroh::RelayUrl;
use iroh_relay::server::{CertConfig, QuicConfig, RelayConfig, Server, ServerConfig, TlsConfig};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};

pub struct TestRelay {
    pub url: RelayUrl,
    pub root: CertificateDer<'static>,
    /// Where it answers address probes (QUIC address discovery).
    pub quic_port: u16,
    _server: Server,
}

pub async fn start() -> TestRelay {
    anyplay_net::install_crypto();
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".into(), "127.0.0.1".into()])
        .expect("certificate");
    let root = cert.cert.der().clone();
    let key = PrivateKeyDer::from(PrivatePkcs8KeyDer::from(cert.signing_key.serialize_der()));
    let tls = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .expect("protocols")
    .with_no_client_auth()
    .with_single_cert(vec![root.clone()], key)
    .expect("server config");
    let mut relay = RelayConfig::new((Ipv4Addr::LOCALHOST, 0));
    relay.tls = Some(TlsConfig::new(
        (Ipv4Addr::LOCALHOST, 0),
        CertConfig::Manual { server_config: tls },
    ));
    let mut config = ServerConfig::default();
    config.relay = Some(relay);
    config.quic = Some(QuicConfig::new((Ipv4Addr::LOCALHOST, 0)));
    let server = Server::spawn(config).await.expect("relay");
    let url = format!(
        "https://localhost:{}",
        server.https_addr().expect("https").port()
    )
    .parse()
    .expect("url");
    let quic_port = server.quic_addr().expect("quic").port();
    TestRelay {
        url,
        root,
        quic_port,
        _server: server,
    }
}
