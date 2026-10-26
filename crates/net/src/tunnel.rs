//! Tunnels to the other members of the player's sessions, on iroh.
//!
//! Only endpoints the cloud named as members of a shared session get a tunnel, in either
//! direction. Of each pair, the endpoint with the smaller ID dials and the other accepts, so a
//! pair never holds two connections.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::StreamExt as _;
use iroh::endpoint::{Connection, presets};
use iroh::{
    Endpoint, EndpointAddr, EndpointId, RelayConfig, RelayMap, RelayMode, RelayUrl, SecretKey,
    Watcher as _,
};
use iroh_relay::RelayQuicConfig;
use tokio::sync::mpsc::UnboundedSender;
use tokio::task::JoinHandle;

use crate::traffic::{Traffic, receive_oversized};
use crate::{screen, voice};

/// A connection that held at least this long counts as good: the next dial starts over at
/// `FIRST_PAUSE`.
const GOOD_CONNECTION: Duration = Duration::from_secs(10);
/// The wait before the first redial, doubled after every connection that failed or ended early.
const FIRST_PAUSE: Duration = Duration::from_millis(500);
/// The longest wait between two dials.
const LONGEST_PAUSE: Duration = Duration::from_secs(30);

/// How many oversized datagrams one connection reads from streams at once.
const MAX_OVERSIZE_READS: usize = 32;

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

/// How often tunnels have settled on each kind of path since the service started. Only the
/// totals are kept: nobody can tell from them who played with whom.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LinkTotals {
    /// Times a tunnel came up on, or moved to, a direct path.
    pub direct: u64,
    /// Times a tunnel came up on, or moved to, a relay path.
    pub relay: u64,
    /// Times a tunnel that ran through a relay found a direct path (included in `direct`).
    pub upgrades: u64,
}

impl LinkTotals {
    /// Counts a tunnel going from `before` (`None` when it was not up) to `after`. Answers
    /// whether the path changed in a way that counts: `Connecting` and `Closed` do not.
    pub fn record(&mut self, before: Option<Link>, after: Link) -> bool {
        if before == Some(after) {
            return false;
        }
        match after {
            Link::Direct => {
                self.direct += 1;
                if before == Some(Link::Relay) {
                    self.upgrades += 1;
                }
                true
            }
            Link::Relay => {
                self.relay += 1;
                true
            }
            Link::Connecting | Link::Closed => false,
        }
    }

    /// Direct paths as a share of all paths, once there is one.
    pub fn direct_share(&self) -> Option<f64> {
        let all = self.direct + self.relay;
        (all > 0).then(|| self.direct as f64 / all as f64)
    }
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

/// What the net report found about reaching this PC without a relay.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Reach {
    /// A relay answered a UDP address probe.
    pub udp: bool,
    /// Whether two relays saw different public addresses (a NAT that maps each destination
    /// apart); unknown with a single relay.
    pub varies: Option<bool>,
    /// The public address the relays saw is one of this PC's own: no NAT in between.
    pub public: bool,
}

impl Reach {
    /// Reads a net report; `own` are this PC's addresses. A relay-only endpoint is never
    /// reached directly, whatever the probes found.
    pub fn of(
        report: &iroh::unstable_net_report::NetReport,
        own: &[IpAddr],
        relay_only: bool,
    ) -> Self {
        if relay_only {
            return Reach::default();
        }
        let seen = [
            report.global_v4.map(|a| IpAddr::V4(*a.ip())),
            report.global_v6.map(|a| IpAddr::V6(*a.ip())),
        ];
        Reach {
            udp: report.has_udp(),
            varies: report.mapping_varies_by_dest(),
            public: seen.into_iter().flatten().any(|ip| own.contains(&ip)),
        }
    }
}

/// The NAT type games are told, as the platform counts it: 1 when others reach this PC
/// directly, 2 when they reach it through a NAT or a relay, 3 when only some can; 0 while
/// nothing is known. `relay` is whether a relay answers, which every peer can reach.
pub fn nat_type(reach: Option<&Reach>, relay: bool) -> u8 {
    match reach {
        Some(reach) if reach.udp && reach.public => 1,
        _ if relay => 2,
        Some(reach) if reach.udp && reach.varies == Some(false) => 2,
        Some(_) => 3,
        None => 0,
    }
}

/// This PC's own addresses, loopback included.
async fn own_addresses() -> Vec<IpAddr> {
    let addresses = netwatch::interfaces::State::new().await.local_addresses;
    addresses
        .regular
        .into_iter()
        .chain(addresses.loopback)
        .collect()
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
    /// Where connections with the voice ALPN go; none while no voice runs.
    voice: Arc<Mutex<Option<voice::Sink>>>,
    /// Where connections with the screen ALPN go; none while the player is in no party.
    screen: Arc<Mutex<Option<screen::Sink>>>,
    relay_only: bool,
    relay_quic_port: Option<u16>,
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
    /// Never a direct path: every tunnel goes through a relay, and peers never learn this
    /// side's addresses. `bind` is then unused.
    pub relay_only: bool,
    /// The relays' UDP port for address probes; their usual one (7842) when `None`.
    pub relay_quic_port: Option<u16>,
}

impl Tunnels {
    pub async fn start(
        options: Options,
        traffic: Traffic,
        changes: UnboundedSender<LinkChange>,
    ) -> Result<Self, String> {
        let mut builder = Endpoint::builder(presets::Minimal)
            .secret_key(options.secret)
            .alpns(vec![
                ALPN.to_vec(),
                voice::ALPN.to_vec(),
                screen::ALPN.to_vec(),
            ])
            .relay_mode(RelayMode::Custom(relay_map(
                &options.relays,
                options.relay_quic_port,
            )));
        if options.relay_only {
            builder = builder.clear_ip_transports();
        } else if let Some(bind) = options.bind {
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
            voice: Arc::new(Mutex::new(None)),
            screen: Arc::new(Mutex::new(None)),
            relay_only: options.relay_only,
            relay_quic_port: options.relay_quic_port,
        };
        tokio::spawn(tunnels.clone().accept());
        Ok(tunnels)
    }

    pub fn id(&self) -> EndpointId {
        self.endpoint.id()
    }

    /// The endpoint, for the voice connections that share it with the tunnels.
    pub fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    /// Sets who takes the connections that arrive with the voice ALPN. Without one they are
    /// refused.
    pub(crate) fn set_voice_sink(&self, sink: Option<voice::Sink>) {
        *self.voice.lock().unwrap_or_else(|e| e.into_inner()) = sink;
    }

    /// Sets who takes the connections that arrive with the screen ALPN. Without one they are
    /// refused.
    pub(crate) fn set_screen_sink(&self, sink: Option<screen::Sink>) {
        *self.screen.lock().unwrap_or_else(|e| e.into_inner()) = sink;
    }

    /// The relay this endpoint is reachable through, once it has one.
    pub fn home_relay(&self) -> Option<RelayUrl> {
        self.endpoint.addr().relay_urls().next().cloned()
    }

    /// Hands `send` what each net report finds, the last one first, until the endpoint
    /// closes.
    pub fn watch_reach(&self, send: impl Fn(Reach) + Send + Sync + 'static) {
        let endpoint = self.endpoint.clone();
        let relay_only = self.relay_only;
        tokio::spawn(async move {
            let mut reports = endpoint.net_report().stream();
            loop {
                let report = tokio::select! {
                    _ = endpoint.closed() => return,
                    report = reports.next() => match report {
                        Some(Some(report)) => report,
                        Some(None) => continue,
                        None => return,
                    },
                };
                send(Reach::of(&report, &own_addresses().await, relay_only));
            }
        });
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
            let config = relay_config(relay, self.relay_quic_port);
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
        let mut pause = FIRST_PAUSE;
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
                    if !self.adopt(id, connection.clone()) {
                        return;
                    }
                    let since = Instant::now();
                    if !self.clone().watch(id, connection).await {
                        return;
                    }
                    // A connection that ends at once (the other side does not list this one
                    // yet) must not be redialed in a tight loop.
                    if since.elapsed() >= GOOD_CONNECTION {
                        // A healthy tunnel that dropped is redialed at once, as before.
                        pause = FIRST_PAUSE;
                        continue;
                    }
                }
                Ok(Err(error)) => tracing::debug!("dial {}: {error}", id.fmt_short()),
                Err(_) => tracing::debug!("dial {}: no answer", id.fmt_short()),
            }
            tokio::time::sleep(pause).await;
            pause = (pause * 2).min(LONGEST_PAUSE);
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
                if connection.alpn() == voice::ALPN {
                    let sink = tunnels
                        .voice
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .clone();
                    match sink {
                        Some(sink) => sink(connection),
                        None => connection.close(1u8.into(), b"no voice here"),
                    }
                    return;
                }
                if connection.alpn() == screen::ALPN {
                    let sink = tunnels
                        .screen
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .clone();
                    match sink {
                        Some(sink) => sink(connection),
                        None => connection.close(1u8.into(), b"no screen here"),
                    }
                    return;
                }
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
                // At most this many oversized datagrams are read at once; the next stream is
                // accepted when one finishes, and the sender's stream limit holds the rest back.
                let reading = Arc::new(tokio::sync::Semaphore::new(MAX_OVERSIZE_READS));
                loop {
                    let Ok(permit) = reading.clone().acquire_owned().await else {
                        return;
                    };
                    let Ok(recv) = connection.accept_uni().await else {
                        return;
                    };
                    let tunnels = tunnels.clone();
                    tokio::spawn(async move {
                        let from = tunnels.address_of(id).unwrap_or(address);
                        receive_oversized(&tunnels.traffic, from, recv).await;
                        drop(permit);
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

fn relay_map(relays: &[RelayAccess], quic_port: Option<u16>) -> RelayMap {
    relays
        .iter()
        .map(|relay| relay_config(relay, quic_port))
        .collect()
}

fn relay_config(relay: &RelayAccess, quic_port: Option<u16>) -> RelayConfig {
    let mut config = RelayConfig::from(relay.url.clone()).with_auth_token(relay.token.clone());
    if let Some(port) = quic_port {
        config.quic = Some(RelayQuicConfig::new(port));
    }
    config
}

#[cfg(test)]
mod tests {
    use super::*;
    use iroh::unstable_net_report::NetReport;

    fn reach(udp: bool, varies: Option<bool>, public: bool) -> Reach {
        Reach {
            udp,
            varies,
            public,
        }
    }

    #[test]
    fn the_nat_type_follows_the_net_report() {
        assert_eq!(nat_type(None, false), 0, "nothing known yet");
        assert_eq!(
            nat_type(None, true),
            2,
            "a relay answers before the first report"
        );
        assert_eq!(nat_type(Some(&reach(true, None, true)), false), 1);
        assert_eq!(nat_type(Some(&reach(true, Some(true), true)), true), 1);
        // Behind a NAT: the relay is the floor, a NAT that keeps one address is 2 without it.
        assert_eq!(nat_type(Some(&reach(true, Some(true), false)), true), 2);
        assert_eq!(nat_type(Some(&reach(false, None, false)), true), 2);
        assert_eq!(nat_type(Some(&reach(true, Some(false), false)), false), 2);
        assert_eq!(nat_type(Some(&reach(true, Some(true), false)), false), 3);
        assert_eq!(nat_type(Some(&reach(true, None, false)), false), 3);
        assert_eq!(nat_type(Some(&reach(false, None, false)), false), 3);
        // UDP that never came back says nothing about the address.
        assert_eq!(nat_type(Some(&reach(false, None, true)), false), 3);
    }

    #[test]
    fn a_report_is_read_against_this_pc_s_addresses() {
        let mut report = NetReport::default();
        report.udp_v4 = true;
        report.global_v4 = Some("203.0.113.7:4000".parse().unwrap());
        report.mapping_varies_by_dest_ipv4 = Some(false);
        let own: Vec<IpAddr> = vec!["192.168.1.20".parse().unwrap()];
        assert_eq!(
            Reach::of(&report, &own, false),
            reach(true, Some(false), false)
        );
        let own: Vec<IpAddr> = vec!["203.0.113.7".parse().unwrap()];
        assert_eq!(
            Reach::of(&report, &own, false),
            reach(true, Some(false), true)
        );
        assert_eq!(
            Reach::of(&report, &own, true),
            Reach::default(),
            "relay only"
        );
        report.global_v4 = None;
        report.udp_v4 = false;
        report.mapping_varies_by_dest_ipv4 = None;
        report.udp_v6 = true;
        report.global_v6 = Some("[2001:db8::5]:4000".parse().unwrap());
        let own: Vec<IpAddr> = vec!["2001:db8::5".parse().unwrap()];
        assert_eq!(Reach::of(&report, &own, false), reach(true, None, true));
    }

    #[test]
    fn totals_count_each_settled_path_once() {
        let mut totals = LinkTotals::default();
        assert_eq!(totals.direct_share(), None);
        assert!(!totals.record(None, Link::Connecting), "not a path");
        assert!(totals.record(Some(Link::Connecting), Link::Relay));
        assert!(!totals.record(Some(Link::Relay), Link::Relay), "no change");
        assert!(
            totals.record(Some(Link::Relay), Link::Direct),
            "found a direct path"
        );
        assert!(!totals.record(Some(Link::Direct), Link::Closed));
        assert!(totals.record(Some(Link::Connecting), Link::Direct));
        assert_eq!(
            totals,
            LinkTotals {
                direct: 2,
                relay: 1,
                upgrades: 1
            }
        );
        assert!((totals.direct_share().unwrap() - 2.0 / 3.0).abs() < 1e-9);
    }
}
