//! Game traffic between the bridge's loopback sockets and the tunnels.
//!
//! This is the hot path. Datagrams go from the bridge's UDP socket straight into a tunnel and
//! back without passing the service's own task; the only shared state is the route table behind
//! a read-mostly lock that no one holds across an await.

use std::collections::{HashMap, HashSet};
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use anyplay_bridge_protocol::{BindResult, STREAM_KEY_LEN, TrafficHeader, Transport};
use bytes::{BufMut, Bytes, BytesMut};
use iroh::endpoint::{Connection, RecvStream, SendStream};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::mpsc;
use tokio::time::Instant;

use crate::broadcast::{BroadcastStats, Limiter, Verdict, is_broadcast};

/// The header in front of game bytes inside a tunnel: the receiving and the sending virtual
/// port. The addresses follow from the tunnel itself.
pub const TUNNEL_HEADER: usize = 4;
/// The largest datagram a game may send; bigger ones are dropped.
const LARGEST_DATAGRAM: usize = 64 * 1024;
/// Oversized datagrams that may wait for each peer's sender; further ones are dropped.
const OVERSIZE_QUEUE: usize = 64;
/// How long one oversized datagram may take to open its stream and hand over its bytes before
/// the stream is reset.
const OVERSIZE_SEND_TIMEOUT: Duration = Duration::from_millis(250);
/// Queued oversized datagrams older than this are stale for a game and dropped unsent.
const OVERSIZE_MAX_AGE: Duration = Duration::from_millis(500);
/// How long to wait for the tunnel header when a stream is opened.
pub const STREAM_HEADER_TIMEOUT: Duration = Duration::from_secs(5);
/// How long a stream may stay idle with no data in either direction before it is closed.
/// Games can stay quiet in menus, lobbies, or during turn-based play for several minutes
/// without exchanging packets; 5 minutes gives ample headroom while still bounding leaked
/// streams from unclean peer drops.
pub const STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(300);

/// How long the receiving side waits for the rest of an oversized datagram before it gives up.
pub const OVERSIZE_READ_TIMEOUT: Duration = Duration::from_secs(1);

/// The tunnel to one peer and the bounded sender for its oversized datagrams.
struct Peer {
    connection: Connection,
    oversize: mpsc::Sender<Queued>,
}

/// An oversized datagram waiting for its stream.
struct Queued {
    bytes: Bytes,
    queued: Instant,
}

#[derive(Default)]
struct Routes {
    /// The tunnel to each peer, by the peer's virtual address.
    peers: HashMap<Ipv4Addr, Peer>,
    datagrams: HashMap<u16, u16>,
    streams: HashMap<u16, u16>,
    /// Loopback ports of bound datagram sockets: only these may send.
    senders: HashSet<u16>,
    /// The other members of the player's current session, by virtual address: where a broadcast
    /// goes. Nobody else ever gets one.
    audience: HashSet<Ipv4Addr>,
    /// Who may reach the game: the members of the session it plays in. `None` lets every peer of a
    /// tunnel through; the service always sets it, so a member of another session or party the
    /// player shares a tunnel with does not reach the game's ports.
    inbound: Option<HashSet<Ipv4Addr>>,
}

/// Counts what the data path drops, for the log and for tests.
#[derive(Debug, Default)]
pub struct Drops {
    pub unbound_sender: std::sync::atomic::AtomicU64,
    pub unknown_peer: std::sync::atomic::AtomicU64,
    pub unbound_port: std::sync::atomic::AtomicU64,
    /// Oversized datagrams dropped because the peer's queue of them was full.
    pub oversize_backlog: std::sync::atomic::AtomicU64,
    /// Oversized datagrams that waited too long or whose stream timed out or failed.
    pub oversize_expired: std::sync::atomic::AtomicU64,
}

/// The loopback side of game traffic and the routes into the tunnels.
#[derive(Clone)]
pub struct Traffic {
    udp: Arc<UdpSocket>,
    udp_port: u16,
    tcp_port: u16,
    key: [u8; STREAM_KEY_LEN],
    routes: Arc<RwLock<Routes>>,
    pub drops: Arc<Drops>,
    pub broadcasts: Arc<BroadcastStats>,
    limiter: Arc<Mutex<Limiter>>,
}

impl Traffic {
    /// Opens the loopback sockets and starts carrying what the bridge sends.
    pub async fn start() -> std::io::Result<Self> {
        let udp = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await?;
        let tcp = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
        let traffic = Self {
            udp_port: udp.local_addr()?.port(),
            tcp_port: tcp.local_addr()?.port(),
            udp: Arc::new(udp),
            key: rand::random(),
            routes: Arc::default(),
            drops: Arc::default(),
            broadcasts: Arc::default(),
            limiter: Arc::new(Mutex::new(Limiter::new(std::time::Instant::now()))),
        };
        tokio::spawn(traffic.clone().carry_datagrams());
        tokio::spawn(traffic.clone().carry_streams(tcp));
        tokio::spawn(traffic.clone().summarize_broadcasts());
        Ok(traffic)
    }

    pub fn udp_port(&self) -> u16 {
        self.udp_port
    }

    pub fn tcp_port(&self) -> u16 {
        self.tcp_port
    }

    pub fn stream_key(&self) -> [u8; STREAM_KEY_LEN] {
        self.key
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, Routes> {
        self.routes.read().unwrap_or_else(|e| e.into_inner())
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, Routes> {
        self.routes.write().unwrap_or_else(|e| e.into_inner())
    }

    /// Sends game traffic for `vport` to the bridge's socket at `loopback_port`.
    pub fn bind(&self, vport: u16, transport: Transport, loopback_port: u16) -> BindResult {
        let mut routes = self.write();
        let table = match transport {
            Transport::Datagram => &mut routes.datagrams,
            Transport::Stream => &mut routes.streams,
            Transport::Unknown => return BindResult::Unknown,
        };
        if table.contains_key(&vport) {
            return BindResult::InUse;
        }
        table.insert(vport, loopback_port);
        if transport == Transport::Datagram {
            routes.senders.insert(loopback_port);
        }
        BindResult::Bound
    }

    pub fn unbind(&self, vport: u16, transport: Transport) {
        let mut routes = self.write();
        match transport {
            Transport::Datagram => {
                if let Some(port) = routes.datagrams.remove(&vport)
                    && !routes.datagrams.values().any(|p| *p == port)
                {
                    routes.senders.remove(&port);
                }
            }
            Transport::Stream => {
                routes.streams.remove(&vport);
            }
            Transport::Unknown => {}
        }
    }

    /// Forgets every binding, as when the game's bridge disconnects.
    pub fn unbind_all(&self) {
        let mut routes = self.write();
        routes.datagrams.clear();
        routes.streams.clear();
        routes.senders.clear();
    }

    /// Routes `address` into `connection`, or removes the route when `connection` is `None`.
    pub fn route(&self, address: Ipv4Addr, connection: Option<Connection>) {
        let mut routes = self.write();
        match connection {
            Some(connection) => {
                // Dropping the old entry closes its queue, which ends its sender task.
                let oversize = spawn_oversize_sender(connection.clone(), self.drops.clone());
                routes.peers.insert(
                    address,
                    Peer {
                        connection,
                        oversize,
                    },
                )
            }
            None => routes.peers.remove(&address),
        };
    }

    /// Writes the broadcast counters to the log once a minute while they change. They name no one.
    async fn summarize_broadcasts(self) {
        let mut last = self.broadcasts.totals();
        loop {
            tokio::time::sleep(crate::broadcast::SUMMARY_EVERY).await;
            let totals = self.broadcasts.totals();
            if totals != last {
                last = totals;
                tracing::info!(
                    target: "anyplay::broadcast",
                    accepted = totals.accepted,
                    copies = totals.copies,
                    alone = totals.alone,
                    too_large = totals.too_large,
                    too_fast = totals.too_fast,
                    "game broadcasts"
                );
            }
        }
    }

    /// Makes `members` the addresses a broadcast goes to: the other members of the player's
    /// current session. Called whenever the session or its members change.
    pub fn set_audience(&self, members: HashSet<Ipv4Addr>) {
        self.write().audience = members;
    }

    /// Limits what the tunnels deliver to the game to the peers at `members`.
    pub fn set_inbound(&self, members: HashSet<Ipv4Addr>) {
        self.write().inbound = Some(members);
    }

    /// Whether the peer at `from` may reach the game.
    fn may_reach_game(&self, from: Ipv4Addr) -> bool {
        self.read()
            .inbound
            .as_ref()
            .is_none_or(|set| set.contains(&from))
    }

    /// Bridge to tunnel: every datagram from a bound loopback socket goes to its peer.
    async fn carry_datagrams(self) {
        let mut buf = vec![0u8; LARGEST_DATAGRAM + TrafficHeader::LEN];
        loop {
            let (len, from) = match self.udp.recv_from(&mut buf).await {
                Ok(received) => received,
                Err(error) => {
                    // Windows reports an earlier send to a closed port on the next receive.
                    tracing::debug!("loopback receive: {error}");
                    if error.kind() != std::io::ErrorKind::ConnectionReset {
                        // Any other failure repeats at once: do not spin on it.
                        tokio::time::sleep(Duration::from_millis(50)).await;
                    }
                    continue;
                }
            };
            if !is_loopback(from) || !self.read().senders.contains(&from.port()) {
                count(&self.drops.unbound_sender);
                continue;
            }
            let Some((header, game)) = TrafficHeader::split(&buf[..len]) else {
                continue;
            };
            if is_broadcast(header.address) {
                self.broadcast(header.vport, header.source_vport, game);
                continue;
            }
            let route = self
                .read()
                .peers
                .get(&header.address)
                .map(|p| (p.connection.clone(), p.oversize.clone()));
            let Some((connection, oversize)) = route else {
                count(&self.drops.unknown_peer);
                continue;
            };
            send_datagram(
                &connection,
                &oversize,
                &self.drops,
                header.vport,
                header.source_vport,
                game,
            );
        }
    }

    /// Copies a broadcast into the tunnel of every member of the audience, within the limits. The
    /// receivers see it as an ordinary datagram from this player's address and port.
    fn broadcast(&self, vport: u16, source_vport: u16, game: &[u8]) {
        let verdict = self
            .limiter
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .admit(std::time::Instant::now(), game.len());
        match verdict {
            Verdict::TooLarge => return BroadcastStats::add(&self.broadcasts.too_large, 1),
            Verdict::TooFast => return BroadcastStats::add(&self.broadcasts.too_fast, 1),
            Verdict::Send => {}
        }
        let routes: Vec<(Connection, mpsc::Sender<Queued>)> = {
            let routes = self.read();
            routes
                .audience
                .iter()
                .filter_map(|address| routes.peers.get(address))
                .map(|p| (p.connection.clone(), p.oversize.clone()))
                .collect()
        };
        BroadcastStats::add(&self.broadcasts.accepted, 1);
        if routes.is_empty() {
            return BroadcastStats::add(&self.broadcasts.alone, 1);
        }
        BroadcastStats::add(&self.broadcasts.copies, routes.len() as u64);
        for (connection, oversize) in &routes {
            send_datagram(connection, oversize, &self.drops, vport, source_vport, game);
        }
    }

    /// Tunnel to bridge: a datagram from the peer at `from`.
    pub async fn deliver_datagram(&self, from: Ipv4Addr, datagram: &[u8]) {
        if !self.may_reach_game(from) {
            count(&self.drops.unknown_peer);
            return;
        }
        let Some((vport, source_vport, game)) = split_tunnel_header(datagram) else {
            return;
        };
        let port = self.read().datagrams.get(&vport).copied();
        let Some(port) = port else {
            count(&self.drops.unbound_port);
            return;
        };
        let header = TrafficHeader {
            address: from,
            vport,
            source_vport,
        };
        let mut packet = Vec::with_capacity(TrafficHeader::LEN + game.len());
        packet.extend_from_slice(&header.encode());
        packet.extend_from_slice(game);
        if let Err(error) = self.udp.send_to(&packet, (Ipv4Addr::LOCALHOST, port)).await {
            tracing::debug!("loopback send to {port}: {error}");
        }
    }

    /// Bridge to tunnel: every stream the bridge opens on the loopback listener.
    async fn carry_streams(self, listener: TcpListener) {
        loop {
            let Ok((stream, from)) = listener.accept().await else {
                // A failing accept (out of file handles, say) repeats at once: do not spin on it.
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                continue;
            };
            if !is_loopback(from) {
                continue;
            }
            let traffic = self.clone();
            tokio::spawn(async move {
                if let Err(reason) = traffic.open_stream(stream).await {
                    tracing::debug!("game stream refused: {reason}");
                }
            });
        }
    }

    async fn open_stream(&self, stream: TcpStream) -> Result<(), String> {
        self.open_stream_with(stream, STREAM_HEADER_TIMEOUT, STREAM_IDLE_TIMEOUT)
            .await
    }

    async fn open_stream_with(
        &self,
        mut stream: TcpStream,
        header_timeout: Duration,
        idle_timeout: Duration,
    ) -> Result<(), String> {
        let mut head = [0u8; STREAM_KEY_LEN + TrafficHeader::LEN];
        tokio::time::timeout(header_timeout, stream.read_exact(&mut head))
            .await
            .map_err(|_| "no header in time".to_owned())?
            .map_err(|e| e.to_string())?;
        if !same(&head[..STREAM_KEY_LEN], &self.key) {
            return Err("wrong stream key".into());
        }
        let (header, _) = TrafficHeader::split(&head[STREAM_KEY_LEN..]).ok_or("short header")?;
        let connection = self
            .read()
            .peers
            .get(&header.address)
            .map(|p| p.connection.clone());
        let connection = connection.ok_or("no tunnel to that address")?;
        let (mut send, recv) = connection.open_bi().await.map_err(|e| e.to_string())?;
        send.write_all(&tunnel_header(header.vport, header.source_vport))
            .await
            .map_err(|e| e.to_string())?;
        pipe(stream, send, recv, idle_timeout).await;
        Ok(())
    }

    /// Tunnel to bridge: a stream the peer at `from` opened.
    pub async fn deliver_stream(&self, from: Ipv4Addr, send: SendStream, recv: RecvStream) {
        self.deliver_stream_with(from, send, recv, STREAM_HEADER_TIMEOUT, STREAM_IDLE_TIMEOUT)
            .await;
    }

    /// Tunnel to bridge with explicit timeouts for tests.
    pub async fn deliver_stream_with(
        &self,
        from: Ipv4Addr,
        mut send: SendStream,
        mut recv: RecvStream,
        header_timeout: Duration,
        idle_timeout: Duration,
    ) {
        if !self.may_reach_game(from) {
            count(&self.drops.unknown_peer);
            let _ = send.reset(0u8.into());
            return;
        }
        let mut head = [0u8; TUNNEL_HEADER];
        let read_head = tokio::time::timeout(header_timeout, recv.read_exact(&mut head)).await;
        if !matches!(read_head, Ok(Ok(_))) {
            let _ = send.reset(0u8.into());
            let _ = recv.stop(0u8.into());
            return;
        }
        let Some((vport, source_vport, _)) = split_tunnel_header(&head) else {
            let _ = send.reset(0u8.into());
            let _ = recv.stop(0u8.into());
            return;
        };
        let port = self.read().streams.get(&vport).copied();
        let Some(port) = port else {
            count(&self.drops.unbound_port);
            let _ = send.reset(0u8.into());
            return;
        };
        let Ok(mut stream) = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).await else {
            let _ = send.reset(0u8.into());
            return;
        };
        let header = TrafficHeader {
            address: from,
            vport,
            source_vport,
        };
        let mut opening = self.key.to_vec();
        opening.extend_from_slice(&header.encode());
        if stream.write_all(&opening).await.is_err() {
            return;
        }
        pipe(stream, send, recv, idle_timeout).await;
    }
}

/// Sends game bytes through a tunnel: as one QUIC datagram when it fits, else through the
/// peer's bounded sender, which gives it a stream of its own so that a large datagram still
/// arrives. A full queue drops the datagram, as a full socket buffer would.
fn send_datagram(
    connection: &Connection,
    oversize: &mpsc::Sender<Queued>,
    drops: &Drops,
    vport: u16,
    source_vport: u16,
    game: &[u8],
) {
    let mut out = BytesMut::with_capacity(TUNNEL_HEADER + game.len());
    out.put_slice(&tunnel_header(vport, source_vport));
    out.put_slice(game);
    let out: Bytes = out.freeze();
    if connection
        .max_datagram_size()
        .is_some_and(|max| out.len() <= max)
    {
        if let Err(error) = connection.send_datagram(out) {
            tracing::debug!("tunnel datagram: {error}");
        }
        return;
    }
    let queued = Queued {
        bytes: out,
        queued: Instant::now(),
    };
    if oversize.try_send(queued).is_err() {
        count(&drops.oversize_backlog);
    }
}

/// Starts the one task that sends a peer's oversized datagrams, one stream at a time. It ends
/// when the peer's route is dropped or the connection closes.
fn spawn_oversize_sender(connection: Connection, drops: Arc<Drops>) -> mpsc::Sender<Queued> {
    let (queue, mut waiting) = mpsc::channel::<Queued>(OVERSIZE_QUEUE);
    tokio::spawn(async move {
        while let Some(item) = waiting.recv().await {
            if item.queued.elapsed() > OVERSIZE_MAX_AGE {
                count(&drops.oversize_expired);
                continue;
            }
            match send_on_stream(&connection, &item.bytes).await {
                Ok(true) => {}
                Ok(false) => count(&drops.oversize_expired),
                Err(()) => return,
            }
        }
    });
    queue
}

/// Writes `bytes` on a new unidirectional stream within `OVERSIZE_SEND_TIMEOUT`. `Ok(false)`:
/// the time ran out or the write failed and the stream was reset; `Err`: the connection is gone.
async fn send_on_stream(connection: &Connection, bytes: &Bytes) -> Result<bool, ()> {
    let deadline = Instant::now() + OVERSIZE_SEND_TIMEOUT;
    let mut send = match tokio::time::timeout_at(deadline, connection.open_uni()).await {
        Ok(Ok(send)) => send,
        Ok(Err(_)) => return Err(()),
        Err(_) => return Ok(false),
    };
    match tokio::time::timeout_at(deadline, send.write_all(bytes)).await {
        Ok(Ok(())) => {
            let _ = send.finish();
            Ok(true)
        }
        _ => {
            let _ = send.reset(0u8.into());
            Ok(false)
        }
    }
}

/// Reads one oversized datagram from a stream the peer at `from` opened and delivers it. A
/// stream that does not complete in `OVERSIZE_READ_TIMEOUT` is abandoned.
pub async fn receive_oversized(traffic: &Traffic, from: Ipv4Addr, mut recv: RecvStream) {
    let read = tokio::time::timeout(
        OVERSIZE_READ_TIMEOUT,
        recv.read_to_end(LARGEST_STREAMED_DATAGRAM),
    )
    .await;
    match read {
        Ok(Ok(bytes)) => traffic.deliver_datagram(from, &bytes).await,
        Ok(Err(_)) => {}
        Err(_) => {
            let _ = recv.stop(0u8.into());
        }
    }
}

/// The largest oversized datagram read from a stream.
pub const LARGEST_STREAMED_DATAGRAM: usize = TUNNEL_HEADER + LARGEST_DATAGRAM;

fn tunnel_header(vport: u16, source_vport: u16) -> [u8; TUNNEL_HEADER] {
    let mut out = [0; TUNNEL_HEADER];
    out[..2].copy_from_slice(&vport.to_le_bytes());
    out[2..].copy_from_slice(&source_vport.to_le_bytes());
    out
}

fn split_tunnel_header(bytes: &[u8]) -> Option<(u16, u16, &[u8])> {
    let (head, rest) = bytes.split_at_checked(TUNNEL_HEADER)?;
    Some((
        u16::from_le_bytes([head[0], head[1]]),
        u16::from_le_bytes([head[2], head[3]]),
        rest,
    ))
}

/// Copies both ways between a loopback stream and a tunnel stream, ending when either side
/// ends or when neither side transfers any bytes within `idle_timeout`.
async fn pipe(
    stream: TcpStream,
    mut send: SendStream,
    mut recv: RecvStream,
    idle_timeout: Duration,
) {
    let _ = stream.set_nodelay(true);
    let (mut read, mut write) = stream.into_split();
    let activity = Arc::new(AtomicBool::new(false));

    let act_out = activity.clone();
    let out = async move {
        let mut buf = [0u8; 16 * 1024];
        loop {
            match read.read(&mut buf).await {
                Ok(0) | Err(_) => {
                    let _ = send.finish();
                    break;
                }
                Ok(n) => {
                    act_out.store(true, Ordering::Relaxed);
                    if send.write_all(&buf[..n]).await.is_err() {
                        break;
                    }
                    act_out.store(true, Ordering::Relaxed);
                }
            }
        }
    };

    let act_in = activity.clone();
    let back = async move {
        let mut buf = [0u8; 16 * 1024];
        loop {
            match recv.read(&mut buf).await {
                Ok(Some(n)) if n > 0 => {
                    act_in.store(true, Ordering::Relaxed);
                    if write.write_all(&buf[..n]).await.is_err() {
                        break;
                    }
                    act_in.store(true, Ordering::Relaxed);
                }
                _ => {
                    let _ = write.shutdown().await;
                    break;
                }
            }
        }
    };

    let watchdog = async move {
        loop {
            tokio::time::sleep(idle_timeout).await;
            if !activity.swap(false, Ordering::Relaxed) {
                break;
            }
        }
    };

    tokio::select! {
        _ = async { tokio::join!(out, back) } => {}
        _ = watchdog => {}
    }
}

fn is_loopback(address: SocketAddr) -> bool {
    address.ip().is_loopback()
}

/// Compares secrets in time that does not depend on where they differ.
pub fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn count(counter: &std::sync::atomic::AtomicU64) {
    counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tunnel_headers_carry_both_ports() {
        let mut bytes = tunnel_header(3658, 9).to_vec();
        bytes.extend_from_slice(b"x");
        assert_eq!(split_tunnel_header(&bytes), Some((3658, 9, &b"x"[..])));
        assert_eq!(split_tunnel_header(&bytes[..3]), None);
    }

    #[tokio::test]
    async fn only_the_games_session_reaches_its_ports() {
        let traffic = Traffic::start().await.unwrap();
        let game = tokio::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let port = game.local_addr().unwrap().port();
        assert_eq!(
            traffic.bind(3658, Transport::Datagram, port),
            BindResult::Bound
        );
        let inside = Ipv4Addr::new(10, 77, 0, 2);
        let outside = Ipv4Addr::new(10, 77, 0, 3);
        let mut datagram = tunnel_header(3658, 3658).to_vec();
        datagram.extend_from_slice(b"hi");
        let mut buf = [0u8; 64];

        traffic.set_inbound(HashSet::from([inside]));
        traffic.deliver_datagram(outside, &datagram).await;
        let nothing = tokio::time::timeout(Duration::from_millis(300), game.recv(&mut buf)).await;
        assert!(
            nothing.is_err(),
            "a peer outside the session reached the game"
        );
        assert_eq!(
            traffic
                .drops
                .unknown_peer
                .load(std::sync::atomic::Ordering::Relaxed),
            1
        );

        traffic.deliver_datagram(inside, &datagram).await;
        let got = tokio::time::timeout(Duration::from_secs(2), game.recv(&mut buf)).await;
        assert!(got.is_ok(), "a member of the session must reach the game");
    }

    #[test]
    fn secrets_compare_whole() {
        assert!(same(b"abc", b"abc"));
        assert!(!same(b"abc", b"abd"));
        assert!(!same(b"abc", b"ab"));
    }

    const TEST_ALPN: &[u8] = b"anyplay/test-oversize";

    async fn test_endpoint() -> iroh::Endpoint {
        iroh::Endpoint::builder(iroh::endpoint::presets::Minimal)
            .alpns(vec![TEST_ALPN.to_vec()])
            .relay_mode(iroh::RelayMode::Disabled)
            .clear_ip_transports()
            .bind_addr((Ipv4Addr::LOCALHOST, 0))
            .unwrap()
            .bind()
            .await
            .unwrap()
    }

    /// Two connected loopback endpoints: the dialer's and the acceptor's connection, plus the
    /// endpoints, which must stay alive.
    async fn connected() -> (Connection, Connection, [iroh::Endpoint; 2]) {
        let server = test_endpoint().await;
        let client = test_endpoint().await;
        let addr = iroh::EndpointAddr::new(server.id())
            .with_ip_addr(server.bound_sockets().into_iter().next().unwrap());
        let accepting = {
            let server = server.clone();
            tokio::spawn(async move {
                let incoming = server.accept().await.unwrap();
                incoming.accept().unwrap().await.unwrap()
            })
        };
        let dialed = client.connect(addr, TEST_ALPN).await.unwrap();
        (dialed, accepting.await.unwrap(), [server, client])
    }

    fn alive_tasks() -> usize {
        tokio::runtime::Handle::current()
            .metrics()
            .num_alive_tasks()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_stalled_receiver_costs_a_bounded_queue_not_a_task_per_packet() {
        let (dialed, _accepted, _endpoints) = connected().await;
        let drops = Arc::new(Drops::default());
        let oversize = spawn_oversize_sender(dialed.clone(), drops.clone());
        let before = alive_tasks();
        // The acceptor never calls accept_uni, so its stream limit and windows fill up.
        let game = vec![9u8; 5000];
        for i in 0..10_000u32 {
            send_datagram(&dialed, &oversize, &drops, 1, 1, &game);
            if i % 64 == 0 {
                tokio::task::yield_now().await;
            }
        }
        let peak = alive_tasks();
        assert!(peak <= before + 1, "tasks grew with the packets");
        let backlog = drops
            .oversize_backlog
            .load(std::sync::atomic::Ordering::Relaxed);
        assert!(backlog > 9_000, "only {backlog} dropped");
        // Let the sender hit its timeouts on the stalled streams: it neither grows nor ends.
        tokio::time::sleep(Duration::from_millis(1500)).await;
        println!(
            "NEW tasks before={before} peak={peak} after_1.5s={}",
            alive_tasks()
        );
        assert!(alive_tasks() <= before + 1);
        assert!(
            drops
                .oversize_expired
                .load(std::sync::atomic::Ordering::Relaxed)
                > 0,
            "queued packets neither went out nor expired"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn oversized_datagrams_arrive_intact_in_both_directions() {
        let (a, b, _endpoints) = connected().await;
        for (from, to) in [(a.clone(), b.clone()), (b, a)] {
            let receiving = Traffic::start().await.unwrap();
            let socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
            let port = socket.local_addr().unwrap().port();
            assert_eq!(
                receiving.bind(7, Transport::Datagram, port),
                BindResult::Bound
            );
            let peer = Ipv4Addr::new(198, 18, 0, 9);
            {
                let (receiving, to) = (receiving.clone(), to.clone());
                tokio::spawn(async move {
                    while let Ok(recv) = to.accept_uni().await {
                        receive_oversized(&receiving, peer, recv).await;
                    }
                });
            }
            {
                let (receiving, to) = (receiving.clone(), to.clone());
                tokio::spawn(async move {
                    while let Ok(datagram) = to.read_datagram().await {
                        receiving.deliver_datagram(peer, &datagram).await;
                    }
                });
            }
            let drops = Arc::new(Drops::default());
            let oversize = spawn_oversize_sender(from.clone(), drops.clone());
            // 1400 B may fit a datagram or not, depending on the path; 5000 B never does.
            for size in [1400usize, 5000] {
                let game: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
                send_datagram(&from, &oversize, &drops, 7, 3, &game);
                let mut buf = vec![0u8; 70_000];
                let len = tokio::time::timeout(Duration::from_secs(5), socket.recv(&mut buf))
                    .await
                    .expect("the datagram never arrived")
                    .unwrap();
                let (header, got) = TrafficHeader::split(&buf[..len]).unwrap();
                assert_eq!(
                    (header.address, header.vport, header.source_vport),
                    (peer, 7, 3)
                );
                assert_eq!(got, &game[..], "{size} B arrived changed");
            }
            assert_eq!(
                drops
                    .oversize_backlog
                    .load(std::sync::atomic::Ordering::Relaxed),
                0
            );
        }
    }

    #[tokio::test]
    async fn a_port_is_bound_once_per_transport() {
        let traffic = Traffic::start().await.unwrap();
        assert_eq!(
            traffic.bind(7, Transport::Datagram, 1000),
            BindResult::Bound
        );
        assert_eq!(
            traffic.bind(7, Transport::Datagram, 1001),
            BindResult::InUse
        );
        assert_eq!(traffic.bind(7, Transport::Stream, 1002), BindResult::Bound);
        traffic.unbind(7, Transport::Datagram);
        assert_eq!(
            traffic.bind(7, Transport::Datagram, 1003),
            BindResult::Bound
        );
        assert!(traffic.read().senders.contains(&1003));
        assert!(!traffic.read().senders.contains(&1000));
    }

    #[tokio::test]
    async fn datagrams_from_unbound_sockets_are_dropped() {
        let traffic = Traffic::start().await.unwrap();
        let socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let header = TrafficHeader {
            address: Ipv4Addr::new(198, 18, 0, 2),
            vport: 1,
            source_vport: 1,
        };
        socket
            .send_to(&header.encode(), (Ipv4Addr::LOCALHOST, traffic.udp_port()))
            .await
            .unwrap();
        let port = socket.local_addr().unwrap().port();
        for _ in 0..100 {
            if traffic
                .drops
                .unbound_sender
                .load(std::sync::atomic::Ordering::Relaxed)
                == 1
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(
            traffic
                .drops
                .unbound_sender
                .load(std::sync::atomic::Ordering::Relaxed),
            1
        );
        traffic.bind(1, Transport::Datagram, port);
        socket
            .send_to(&header.encode(), (Ipv4Addr::LOCALHOST, traffic.udp_port()))
            .await
            .unwrap();
        for _ in 0..100 {
            if traffic
                .drops
                .unknown_peer
                .load(std::sync::atomic::Ordering::Relaxed)
                == 1
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(
            traffic
                .drops
                .unknown_peer
                .load(std::sync::atomic::Ordering::Relaxed),
            1
        );
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn silent_peer_stream_header_times_out() {
        let (client, server, _endpoints) = connected().await;
        let traffic = Traffic::start().await.unwrap();
        let peer_addr = Ipv4Addr::new(10, 77, 0, 2);
        traffic.set_inbound(HashSet::from([peer_addr]));

        let (mut client_send, mut client_recv) = client.open_bi().await.unwrap();
        // Send a partial 1-byte header so the QUIC stream frame is transmitted and accepted.
        client_send.write_all(&[1]).await.unwrap();
        let (server_send, server_recv) = server.accept_bi().await.unwrap();

        // Server waits for the 4-byte header with a 50 ms timeout, but peer stays silent.
        traffic
            .deliver_stream_with(
                peer_addr,
                server_send,
                server_recv,
                Duration::from_millis(50),
                Duration::from_millis(100),
            )
            .await;

        let mut buf = [0u8; 16];
        let res = client_recv.read(&mut buf).await;
        assert!(
            res.is_err() || res.as_ref().is_ok_and(|opt| opt.is_none()),
            "silent peer stream must be dropped"
        );
        drop(client_send);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn silent_peer_stream_idle_times_out() {
        let (client, server, _endpoints) = connected().await;
        let traffic = Traffic::start().await.unwrap();
        let peer_addr = Ipv4Addr::new(10, 77, 0, 2);
        traffic.set_inbound(HashSet::from([peer_addr]));

        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        assert_eq!(
            traffic.bind(8000, Transport::Stream, port),
            BindResult::Bound
        );

        let (mut client_send, mut client_recv) = client.open_bi().await.unwrap();
        // Client sends the 4-byte tunnel header to establish the stream to port 8000.
        client_send
            .write_all(&tunnel_header(8000, 5000))
            .await
            .unwrap();

        let (server_send, server_recv) = server.accept_bi().await.unwrap();

        let delivery = tokio::spawn(async move {
            traffic
                .deliver_stream_with(
                    peer_addr,
                    server_send,
                    server_recv,
                    Duration::from_secs(5),
                    Duration::from_millis(50),
                )
                .await;
        });

        // The game side accepts the connection and reads the traffic opening header.
        let (mut game_stream, _) = listener.accept().await.unwrap();
        let mut key_buf = [0u8; 16 + TrafficHeader::LEN];
        game_stream.read_exact(&mut key_buf).await.unwrap();

        // Both sides now stay silent. The watchdog should fire after 50 ms idle timeout.
        let mut buf = [0u8; 16];
        let res = client_recv.read(&mut buf).await;
        assert!(
            res.is_err() || res.as_ref().is_ok_and(|opt| opt.is_none()),
            "idle stream must be dropped"
        );

        delivery.await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn slow_live_stream_survives_idle_timeout() {
        let (client, server, _endpoints) = connected().await;
        let traffic = Traffic::start().await.unwrap();
        let peer_addr = Ipv4Addr::new(10, 77, 0, 2);
        traffic.set_inbound(HashSet::from([peer_addr]));

        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        assert_eq!(
            traffic.bind(8000, Transport::Stream, port),
            BindResult::Bound
        );

        let (mut client_send, _client_recv) = client.open_bi().await.unwrap();
        client_send
            .write_all(&tunnel_header(8000, 5000))
            .await
            .unwrap();

        let (server_send, server_recv) = server.accept_bi().await.unwrap();

        let idle_timeout = Duration::from_millis(60);
        let delivery = tokio::spawn(async move {
            traffic
                .deliver_stream_with(
                    peer_addr,
                    server_send,
                    server_recv,
                    Duration::from_secs(5),
                    idle_timeout,
                )
                .await;
        });

        let (mut game_stream, _) = listener.accept().await.unwrap();
        let mut key_buf = [0u8; 16 + TrafficHeader::LEN];
        game_stream.read_exact(&mut key_buf).await.unwrap();

        // Send a byte every ~idle/3 across 2 full idle timeout intervals
        let step = Duration::from_millis(20);
        let mut received = Vec::new();
        for i in 0..6u8 {
            tokio::time::sleep(step).await;
            client_send.write_all(&[i]).await.unwrap();
            let mut byte = [0u8; 1];
            game_stream.read_exact(&mut byte).await.unwrap();
            received.push(byte[0]);
        }

        assert_eq!(received, vec![0, 1, 2, 3, 4, 5]);

        drop(client_send);
        delivery.await.unwrap();
    }

    #[tokio::test]
    async fn silent_bridge_stream_header_times_out() {
        let traffic = Traffic::start().await.unwrap();
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let client = TcpStream::connect((Ipv4Addr::LOCALHOST, port))
            .await
            .unwrap();
        let (server, _) = listener.accept().await.unwrap();

        let res = traffic
            .open_stream_with(
                server,
                Duration::from_millis(50),
                Duration::from_millis(100),
            )
            .await;
        assert_eq!(res.unwrap_err(), "no header in time");
        drop(client);
    }
}
