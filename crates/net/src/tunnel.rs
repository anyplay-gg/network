//! Tunnels to the other members of the player's sessions, on iroh.
//!
//! Only endpoints the cloud named as members of a shared session get a tunnel, in either
//! direction. Of each pair, the endpoint with the smaller ID dials and the other accepts, so a
//! pair never holds two connections.

use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use iroh::endpoint::{Connection, presets};
use iroh::{
    Endpoint, EndpointAddr, EndpointId, RelayConfig, RelayMap, RelayMode, RelayUrl, SecretKey,
};
use tokio::sync::mpsc::UnboundedSender;
use tokio::task::JoinHandle;

use crate::traffic::{LARGEST_STREAMED_DATAGRAM, Traffic};

/// The protocol inside every tunnel.
pub const ALPN: &[u8] = b"anyplay/tunnel/0";

/// Who is at the other end of a tunnel.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Peer {
    pub account: u64,
    pub handle: String,
    pub address: Ipv4Addr,
    pub relay: Option<RelayUrl>,
}

/// How a tunnel runs right now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Link {
    Connecting,
    Direct,
    Relay,
    Closed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LinkChange {
    pub endpoint: EndpointId,
    pub account: u64,
    pub link: Link,
    pub rtt: Option<Duration>,
}

/// A relay the net service may use, with the ticket that lets it in.
#[derive(Clone, Debug)]
pub struct RelayAccess {
    pub url: RelayUrl,
    pub token: String,
}

struct Tunnel {
    peer: Peer,
    connection: Option<Connection>,
    link: Link,
    rtt: Option<Duration>,
    task: Option<JoinHandle<()>>,
}

struct State {
    tunnels: HashMap<EndpointId, Tunnel>,
}

/// The iroh endpoint of this net service and its tunnels.
#[derive(Clone)]
pub struct Tunnels {
    endpoint: Endpoint,
    state: Arc<Mutex<State>>,
    traffic: Traffic,
    changes: UnboundedSender<LinkChange>,
}

/// Settings of the endpoint.
pub struct Options {
    pub secret: SecretKey,
    pub relays: Vec<RelayAccess>,
    /// Root certificates to trust for relays besides the usual ones, as for a self-hosted cloud
    /// with its own certificate authority.
    pub relay_roots: Vec<rustls::pki_types::CertificateDer<'static>>,
    /// Bind only this address instead of every interface, as tests do on loopback.
    pub bind: Option<std::net::SocketAddr>,
}

impl Tunnels {
    pub async fn start(
        options: Options,
        traffic: Traffic,
        changes: UnboundedSender<LinkChange>,
    ) -> Result<Self, String> {
        let mut builder = Endpoint::builder(presets::Minimal)
            .secret_key(options.secret)
            .alpns(vec![ALPN.to_vec()])
            .relay_mode(RelayMode::Custom(relay_map(&options.relays)));
        if let Some(bind) = options.bind {
            builder = builder
                .clear_ip_transports()
                .bind_addr(bind)
                .map_err(|e| e.to_string())?;
        }
        if !options.relay_roots.is_empty() {
            builder = builder.ca_tls_config(
                iroh::tls::CaTlsConfig::embedded().with_extra_roots(options.relay_roots),
            );
        }
        let endpoint = builder.bind().await.map_err(|e| e.to_string())?;
        let tunnels = Self {
            endpoint,
            state: Arc::new(Mutex::new(State {
                tunnels: HashMap::new(),
            })),
            traffic,
            changes,
        };
        tokio::spawn(tunnels.clone().accept());
        Ok(tunnels)
    }

    pub fn id(&self) -> EndpointId {
        self.endpoint.id()
    }

    /// The relay this endpoint is reachable through, once it has one.
    pub fn home_relay(&self) -> Option<RelayUrl> {
        self.endpoint.addr().relay_urls().next().cloned()
    }

    /// Waits until the endpoint is reachable through a relay, at most `limit`.
    pub async fn online(&self, limit: Duration) -> bool {
        tokio::time::timeout(limit, self.endpoint.online())
            .await
            .is_ok()
    }

    /// Replaces the tickets of the relays.
    pub async fn set_relays(&self, relays: &[RelayAccess]) {
        for relay in relays {
            let config = RelayConfig::from(relay.url.clone()).with_auth_token(relay.token.clone());
            self.endpoint
                .insert_relay(relay.url.clone(), Arc::new(config))
                .await;
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Makes `peers` the set of endpoints that get a tunnel: closes tunnels to the others and
    /// opens the missing ones this side dials.
    pub fn set_peers(&self, peers: HashMap<EndpointId, Peer>) {
        let me = self.id();
        let mut state = self.lock();
        let gone: Vec<EndpointId> = state
            .tunnels
            .keys()
            .filter(|id| !peers.contains_key(*id))
            .copied()
            .collect();
        for id in gone {
            if let Some(tunnel) = state.tunnels.remove(&id) {
                self.drop_tunnel(id, tunnel);
            }
        }
        for (id, peer) in peers {
            if id == me {
                continue;
            }
            match state.tunnels.get_mut(&id) {
                Some(tunnel) => {
                    if tunnel.peer.address != peer.address {
                        self.traffic.route(tunnel.peer.address, None);
                        if let Some(connection) = &tunnel.connection {
                            self.traffic.route(peer.address, Some(connection.clone()));
                        }
                    }
                    let redial = tunnel.peer.relay != peer.relay && tunnel.connection.is_none();
                    tunnel.peer = peer;
                    if redial && me < id {
                        if let Some(task) = tunnel.task.take() {
                            task.abort();
                        }
                        tunnel.task = Some(tokio::spawn(self.clone().dial(id)));
                    }
                }
                None => {
                    let task = (me < id).then(|| tokio::spawn(self.clone().dial(id)));
                    let account = peer.account;
                    state.tunnels.insert(
                        id,
                        Tunnel {
                            peer,
                            connection: None,
                            link: Link::Connecting,
                            rtt: None,
                            task,
                        },
                    );
                    let _ = self.changes.send(LinkChange {
                        endpoint: id,
                        account,
                        link: Link::Connecting,
                        rtt: None,
                    });
                }
            }
        }
    }

    fn drop_tunnel(&self, id: EndpointId, tunnel: Tunnel) {
        if let Some(task) = tunnel.task {
            task.abort();
        }
        self.traffic.route(tunnel.peer.address, None);
        if let Some(connection) = tunnel.connection {
            connection.close(0u8.into(), b"not in a session together");
        }
        let _ = self.changes.send(LinkChange {
            endpoint: id,
            account: tunnel.peer.account,
            link: Link::Closed,
            rtt: None,
        });
    }

    /// The link and round trip of the tunnel to the peer at `address`.
    pub fn link_of(&self, address: Ipv4Addr) -> Option<(Link, Option<Duration>)> {
        self.lock()
            .tunnels
            .values()
            .find(|t| t.peer.address == address)
            .map(|t| (t.link, t.rtt))
    }

    /// The link of every tunnel, by account.
    pub fn links(&self) -> HashMap<u64, Link> {
        self.lock()
            .tunnels
            .values()
            .map(|t| (t.peer.account, t.link))
            .collect()
    }

    pub async fn close(&self) {
        let tunnels: Vec<(EndpointId, Tunnel)> = self.lock().tunnels.drain().collect();
        for (id, tunnel) in tunnels {
            self.drop_tunnel(id, tunnel);
        }
        self.endpoint.close().await;
    }

    /// Dials `id` until a tunnel stands or the peer is no longer wanted.
    async fn dial(self, id: EndpointId) {
        let mut pause = Duration::from_millis(500);
        loop {
            let relay = {
                let state = self.lock();
                match state.tunnels.get(&id) {
                    Some(tunnel) if tunnel.connection.is_none() => tunnel.peer.relay.clone(),
                    _ => return,
                }
            };
            let mut addr = EndpointAddr::new(id);
            if let Some(relay) = relay {
                addr = addr.with_relay_url(relay);
            }
            match tokio::time::timeout(Duration::from_secs(20), self.endpoint.connect(addr, ALPN))
                .await
            {
                Ok(Ok(connection)) => {
                    if self.adopt(id, connection.clone()) {
                        let ended = self.clone().watch(id, connection).await;
                        if !ended {
                            return;
                        }
                        pause = Duration::from_millis(500);
                        continue;
                    }
                    return;
                }
                Ok(Err(error)) => tracing::debug!("dial {}: {error}", id.fmt_short()),
                Err(_) => tracing::debug!("dial {}: no answer", id.fmt_short()),
            }
            tokio::time::sleep(pause).await;
            pause = (pause * 2).min(Duration::from_secs(30));
        }
    }

    /// Accepts tunnels from members, and refuses everyone else before the handshake ends.
    async fn accept(self) {
        while let Some(incoming) = self.endpoint.accept().await {
            let tunnels = self.clone();
            tokio::spawn(async move {
                let Ok(accepting) = incoming.accept() else {
                    return;
                };
                let Ok(connection) = accepting.await else {
                    return;
                };
                let id = connection.remote_id();
                let wanted = {
                    let state = tunnels.lock();
                    state
                        .tunnels
                        .get(&id)
                        .is_some_and(|t| t.connection.is_none() && tunnels.id() > id)
                };
                if !wanted {
                    connection.close(1u8.into(), b"not in a session together");
                    return;
                }
                if tunnels.adopt(id, connection.clone()) {
                    tunnels.watch(id, connection).await;
                }
            });
        }
    }

    /// Records a fresh connection as the tunnel to `id`; false when `id` is no longer wanted.
    fn adopt(&self, id: EndpointId, connection: Connection) -> bool {
        let mut state = self.lock();
        let Some(tunnel) = state.tunnels.get_mut(&id) else {
            connection.close(1u8.into(), b"not in a session together");
            return false;
        };
        self.traffic
            .route(tunnel.peer.address, Some(connection.clone()));
        tunnel.connection = Some(connection);
        true
    }

    /// Carries the tunnel's traffic and reports its link until it closes. True when the tunnel
    /// is still wanted afterwards, so the dialer tries again.
    async fn watch(self, id: EndpointId, connection: Connection) -> bool {
        let address = {
            let state = self.lock();
            match state.tunnels.get(&id) {
                Some(tunnel) => tunnel.peer.address,
                None => return false,
            }
        };
        let datagrams = {
            let connection = connection.clone();
            let tunnels = self.clone();
            async move {
                while let Ok(datagram) = connection.read_datagram().await {
                    let from = tunnels.address_of(id).unwrap_or(address);
                    tunnels.traffic.deliver_datagram(from, &datagram).await;
                }
            }
        };
        let uni = {
            let connection = connection.clone();
            let tunnels = self.clone();
            async move {
                while let Ok(mut recv) = connection.accept_uni().await {
                    let tunnels = tunnels.clone();
                    tokio::spawn(async move {
                        if let Ok(bytes) = recv.read_to_end(LARGEST_STREAMED_DATAGRAM).await {
                            let from = tunnels.address_of(id).unwrap_or(address);
                            tunnels.traffic.deliver_datagram(from, &bytes).await;
                        }
                    });
                }
            }
        };
        let bi = {
            let connection = connection.clone();
            let tunnels = self.clone();
            async move {
                while let Ok((send, recv)) = connection.accept_bi().await {
                    let tunnels = tunnels.clone();
                    tokio::spawn(async move {
                        let from = tunnels.address_of(id).unwrap_or(address);
                        tunnels.traffic.deliver_stream(from, send, recv).await;
                    });
                }
            }
        };
        let paths = {
            let connection = connection.clone();
            let tunnels = self.clone();
            async move {
                use futures_util::StreamExt;
                let mut stream = connection.paths_stream();
                while let Some(paths) = stream.next().await {
                    let selected = paths.iter().find(|p| p.is_selected());
                    let (link, rtt) = match selected {
                        Some(path) if path.is_relay() => (Link::Relay, Some(path.rtt())),
                        Some(path) => (Link::Direct, Some(path.rtt())),
                        None => (Link::Connecting, None),
                    };
                    tunnels.report(id, link, rtt);
                }
            }
        };
        tokio::select! {
            _ = datagrams => {}
            _ = uni => {}
            _ = bi => {}
            _ = paths => {}
            _ = connection.closed() => {}
        }
        let mut state = self.lock();
        let Some(tunnel) = state.tunnels.get_mut(&id) else {
            return false;
        };
        if tunnel
            .connection
            .as_ref()
            .is_some_and(|c| c.stable_id() == connection.stable_id())
        {
            tunnel.connection = None;
            self.traffic.route(tunnel.peer.address, None);
            tunnel.link = Link::Connecting;
            tunnel.rtt = None;
            let _ = self.changes.send(LinkChange {
                endpoint: id,
                account: tunnel.peer.account,
                link: Link::Connecting,
                rtt: None,
            });
        }
        true
    }

    fn address_of(&self, id: EndpointId) -> Option<Ipv4Addr> {
        self.lock().tunnels.get(&id).map(|t| t.peer.address)
    }

    fn report(&self, id: EndpointId, link: Link, rtt: Option<Duration>) {
        let mut state = self.lock();
        let Some(tunnel) = state.tunnels.get_mut(&id) else {
            return;
        };
        tunnel.rtt = rtt;
        if tunnel.link == link {
            return;
        }
        tunnel.link = link;
        let _ = self.changes.send(LinkChange {
            endpoint: id,
            account: tunnel.peer.account,
            link,
            rtt,
        });
    }
}

fn relay_map(relays: &[RelayAccess]) -> RelayMap {
    relays
        .iter()
        .map(|relay| RelayConfig::from(relay.url.clone()).with_auth_token(relay.token.clone()))
        .collect()
}
