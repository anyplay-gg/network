//! Game traffic between the bridge's loopback sockets and the tunnels.
//!
//! This is the hot path. Datagrams go from the bridge's UDP socket straight into a tunnel and
//! back without passing the service's own task; the only shared state is the route table behind
//! a read-mostly lock that no one holds across an await.

use std::collections::{HashMap, HashSet};
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use anyplay_bridge_protocol::{BindResult, STREAM_KEY_LEN, TrafficHeader, Transport};
use bytes::{BufMut, Bytes, BytesMut};
use iroh::endpoint::{Connection, RecvStream, SendStream};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};

use crate::broadcast::{BroadcastStats, Limiter, Verdict, is_broadcast};

/// The header in front of game bytes inside a tunnel: the receiving and the sending virtual
/// port. The addresses follow from the tunnel itself.
pub const TUNNEL_HEADER: usize = 4;
/// The largest datagram a game may send; bigger ones are dropped.
const LARGEST_DATAGRAM: usize = 64 * 1024;

#[derive(Default)]
struct Routes {
    /// The tunnel to each peer, by the peer's virtual address.
    peers: HashMap<Ipv4Addr, Connection>,
    datagrams: HashMap<u16, u16>,
    streams: HashMap<u16, u16>,
    /// Loopback ports of bound datagram sockets: only these may send.
    senders: HashSet<u16>,
    /// The other members of the player's current session, by virtual address: where a broadcast
    /// goes. Nobody else ever gets one.
    audience: HashSet<Ipv4Addr>,
}

/// Counts what the data path drops, for the log and for tests.
#[derive(Debug, Default)]
pub struct Drops {
    pub unbound_sender: std::sync::atomic::AtomicU64,
    pub unknown_peer: std::sync::atomic::AtomicU64,
    pub unbound_port: std::sync::atomic::AtomicU64,
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
            limiter: Arc::new(Mutex::new(Limiter::new(Instant::now()))),
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
            Some(connection) => routes.peers.insert(address, connection),
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

    /// Bridge to tunnel: every datagram from a bound loopback socket goes to its peer.
    async fn carry_datagrams(self) {
        let mut buf = vec![0u8; LARGEST_DATAGRAM + TrafficHeader::LEN];
        loop {
            let (len, from) = match self.udp.recv_from(&mut buf).await {
                Ok(received) => received,
                Err(error) => {
                    // Windows reports an earlier send to a closed port on the next receive.
                    tracing::debug!("loopback receive: {error}");
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
            let connection = self.read().peers.get(&header.address).cloned();
            let Some(connection) = connection else {
                count(&self.drops.unknown_peer);
                continue;
            };
            send_datagram(&connection, header.vport, header.source_vport, game);
        }
    }

    /// Copies a broadcast into the tunnel of every member of the audience, within the limits. The
    /// receivers see it as an ordinary datagram from this player's address and port.
    fn broadcast(&self, vport: u16, source_vport: u16, game: &[u8]) {
        let verdict = self
            .limiter
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .admit(Instant::now(), game.len());
        match verdict {
            Verdict::TooLarge => return BroadcastStats::add(&self.broadcasts.too_large, 1),
            Verdict::TooFast => return BroadcastStats::add(&self.broadcasts.too_fast, 1),
            Verdict::Send => {}
        }
        let connections: Vec<Connection> = {
            let routes = self.read();
            routes
                .audience
                .iter()
                .filter_map(|address| routes.peers.get(address).cloned())
                .collect()
        };
        BroadcastStats::add(&self.broadcasts.accepted, 1);
        if connections.is_empty() {
            return BroadcastStats::add(&self.broadcasts.alone, 1);
        }
        BroadcastStats::add(&self.broadcasts.copies, connections.len() as u64);
        for connection in &connections {
            send_datagram(connection, vport, source_vport, game);
        }
    }

    /// Tunnel to bridge: a datagram from the peer at `from`.
    pub async fn deliver_datagram(&self, from: Ipv4Addr, datagram: &[u8]) {
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

    async fn open_stream(&self, mut stream: TcpStream) -> Result<(), String> {
        let mut head = [0u8; STREAM_KEY_LEN + TrafficHeader::LEN];
        tokio::time::timeout(Duration::from_secs(5), stream.read_exact(&mut head))
            .await
            .map_err(|_| "no header in time".to_owned())?
            .map_err(|e| e.to_string())?;
        if !same(&head[..STREAM_KEY_LEN], &self.key) {
            return Err("wrong stream key".into());
        }
        let (header, _) = TrafficHeader::split(&head[STREAM_KEY_LEN..]).ok_or("short header")?;
        let connection = self.read().peers.get(&header.address).cloned();
        let connection = connection.ok_or("no tunnel to that address")?;
        let (mut send, recv) = connection.open_bi().await.map_err(|e| e.to_string())?;
        send.write_all(&tunnel_header(header.vport, header.source_vport))
            .await
            .map_err(|e| e.to_string())?;
        pipe(stream, send, recv).await;
        Ok(())
    }

    /// Tunnel to bridge: a stream the peer at `from` opened.
    pub async fn deliver_stream(&self, from: Ipv4Addr, mut send: SendStream, mut recv: RecvStream) {
        let mut head = [0u8; TUNNEL_HEADER];
        if recv.read_exact(&mut head).await.is_err() {
            return;
        }
        let Some((vport, source_vport, _)) = split_tunnel_header(&head) else {
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
        pipe(stream, send, recv).await;
    }
}

/// Sends game bytes through a tunnel: as one QUIC datagram when it fits, else on a stream of
/// its own so that a large datagram still arrives.
pub fn send_datagram(connection: &Connection, vport: u16, source_vport: u16, game: &[u8]) {
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
    let connection = connection.clone();
    tokio::spawn(async move {
        if let Ok(mut send) = connection.open_uni().await {
            let _ = send.write_all(&out).await;
            let _ = send.finish();
        }
    });
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

/// Copies both ways between a loopback stream and a tunnel stream until either side ends.
async fn pipe(stream: TcpStream, mut send: SendStream, mut recv: RecvStream) {
    let _ = stream.set_nodelay(true);
    let (mut read, mut write) = stream.into_split();
    let out = async {
        let _ = tokio::io::copy(&mut read, &mut send).await;
        let _ = send.finish();
    };
    let back = async {
        let _ = tokio::io::copy(&mut recv, &mut write).await;
        let _ = write.shutdown().await;
    };
    tokio::join!(out, back);
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

    #[test]
    fn secrets_compare_whole() {
        assert!(same(b"abc", b"abc"));
        assert!(!same(b"abc", b"abd"));
        assert!(!same(b"abc", b"ab"));
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
}
