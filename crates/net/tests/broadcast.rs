//! A game's broadcasts on the virtual LAN: three net services on loopback and a relay. A and B
//! are members of one session, C shares another session with A only. Games broadcast on 19132
//! (RakNet discovery) and 7551 (NetherNet discovery); the other member gets each datagram from
//! the sender's virtual address and port, and C never gets one.

mod support;

use std::collections::{HashMap, HashSet};
use std::net::{Ipv4Addr, SocketAddr};
use std::time::Duration;

use anyplay_bridge_protocol::{BindResult, TrafficHeader, Transport};
use anyplay_net::broadcast::{BROADCAST_BURST, LARGEST_BROADCAST};
use anyplay_net::traffic::Traffic;
use anyplay_net::tunnel::{self, Link, LinkChange, Options, RelayAccess, Tunnels};
use iroh::SecretKey;
use tokio::net::UdpSocket;
use tokio::sync::mpsc;

const A: Ipv4Addr = Ipv4Addr::new(198, 18, 0, 1);
const B: Ipv4Addr = Ipv4Addr::new(198, 18, 0, 2);
const C: Ipv4Addr = Ipv4Addr::new(198, 18, 0, 3);
const LIMITED: Ipv4Addr = Ipv4Addr::BROADCAST;
const SUBNET: Ipv4Addr = Ipv4Addr::new(198, 19, 255, 255);

struct Side {
    traffic: Traffic,
    tunnels: Tunnels,
    changes: mpsc::UnboundedReceiver<LinkChange>,
}

async fn side(relay: &support::relay::TestRelay) -> Side {
    let traffic = Traffic::start().await.unwrap();
    let (tx, changes) = mpsc::unbounded_channel();
    let tunnels = Tunnels::start(
        Options {
            secret: SecretKey::generate(),
            relays: vec![RelayAccess {
                url: relay.url.clone(),
                token: "ticket".into(),
            }],
            relay_roots: vec![relay.root.clone()],
            bind: Some(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))),
            relay_only: false,
        },
        traffic.clone(),
        tx,
    )
    .await
    .unwrap();
    assert!(
        tunnels.online(Duration::from_secs(10)).await,
        "relay not reached"
    );
    Side {
        traffic,
        tunnels,
        changes,
    }
}

fn peer(account: u64, address: Ipv4Addr, of: &Side) -> tunnel::Peer {
    tunnel::Peer {
        account,
        handle: format!("player{account}"),
        address,
        relay: of.tunnels.home_relay(),
    }
}

async fn linked(changes: &mut mpsc::UnboundedReceiver<LinkChange>) {
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let change = changes.recv().await.expect("changes");
            if matches!(change.link, Link::Direct | Link::Relay) {
                return;
            }
        }
    })
    .await
    .expect("no tunnel in time");
}

/// A game socket bound to `vport` on `side`, as the bridge binds it.
async fn game_socket(side: &Side, vport: u16) -> UdpSocket {
    let socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let port = socket.local_addr().unwrap().port();
    assert_eq!(
        side.traffic.bind(vport, Transport::Datagram, port),
        BindResult::Bound
    );
    socket
}

fn datagram(to: Ipv4Addr, vport: u16, source_vport: u16, game: &[u8]) -> Vec<u8> {
    let mut out = TrafficHeader {
        address: to,
        vport,
        source_vport,
    }
    .encode()
    .to_vec();
    out.extend_from_slice(game);
    out
}

/// Sends `datagram` from `from` until `to` receives one (the tunnels may still be coming up) and
/// returns what arrived.
async fn deliver(from: &UdpSocket, side: &Side, datagram: &[u8], to: &UdpSocket) -> Vec<u8> {
    let mut buf = vec![0u8; 70_000];
    for _ in 0..50 {
        from.send_to(datagram, (Ipv4Addr::LOCALHOST, side.traffic.udp_port()))
            .await
            .unwrap();
        if let Ok(Ok(len)) =
            tokio::time::timeout(Duration::from_millis(200), to.recv(&mut buf)).await
        {
            return buf[..len].to_vec();
        }
    }
    panic!("the datagram never arrived");
}

/// True when `socket` receives nothing for 300 ms.
async fn silent(socket: &UdpSocket) -> bool {
    let mut buf = vec![0u8; 70_000];
    tokio::time::timeout(Duration::from_millis(300), socket.recv(&mut buf))
        .await
        .is_err()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn broadcasts_reach_the_other_members_of_the_session_and_nobody_else() {
    let relay = support::relay::start().await;
    let mut a = side(&relay).await;
    let mut b = side(&relay).await;
    let mut c = side(&relay).await;

    // A holds tunnels to B (same session) and to C (another session); only B is in its audience.
    a.tunnels.set_peers(HashMap::from([
        (b.tunnels.id(), peer(2, B, &b)),
        (c.tunnels.id(), peer(3, C, &c)),
    ]));
    b.tunnels
        .set_peers(HashMap::from([(a.tunnels.id(), peer(1, A, &a))]));
    c.tunnels
        .set_peers(HashMap::from([(a.tunnels.id(), peer(1, A, &a))]));
    a.traffic.set_audience(HashSet::from([B]));
    b.traffic.set_audience(HashSet::from([A]));
    c.traffic.set_audience(HashSet::from([A]));
    linked(&mut a.changes).await;
    linked(&mut a.changes).await;
    linked(&mut b.changes).await;
    linked(&mut c.changes).await;

    let a_raknet = game_socket(&a, 19132).await;
    let a_nethernet = game_socket(&a, 7551).await;
    let b_raknet = game_socket(&b, 19132).await;
    let b_nethernet = game_socket(&b, 7551).await;
    // The joiner's discovery socket on an ephemeral port, which the host answers by unicast.
    let b_joiner = game_socket(&b, 53211).await;
    let c_raknet = game_socket(&c, 19132).await;
    let c_nethernet = game_socket(&c, 7551).await;

    // A RakNet unconnected ping to the limited broadcast, from the game's own port 19132.
    let ping = datagram(LIMITED, 19132, 19132, &[0x01, 1, 2, 3, 4, 5, 6, 7, 8]);
    let got = deliver(&a_raknet, &a, &ping, &b_raknet).await;
    let (header, game) = TrafficHeader::split(&got).unwrap();
    assert_eq!(
        header,
        TrafficHeader {
            address: A,
            vport: 19132,
            source_vport: 19132
        },
        "the receiver sees the sender's virtual address and port"
    );
    assert_eq!(game, &[0x01, 1, 2, 3, 4, 5, 6, 7, 8]);

    // A NetherNet discovery request to the subnet broadcast, from an ephemeral port.
    let request = datagram(SUBNET, 7551, 53211, b"discovery request");
    let got = deliver(&b_joiner, &b, &request, &a_nethernet).await;
    let (header, game) = TrafficHeader::split(&got).unwrap();
    assert_eq!(
        header,
        TrafficHeader {
            address: B,
            vport: 7551,
            source_vport: 53211
        }
    );
    assert_eq!(game, b"discovery request");

    // The host answers the request's source by unicast, and the joiner hears it.
    let response = datagram(
        header.address,
        header.source_vport,
        7551,
        b"discovery response",
    );
    let got = deliver(&a_nethernet, &a, &response, &b_joiner).await;
    let (header, game) = TrafficHeader::split(&got).unwrap();
    assert_eq!(
        header,
        TrafficHeader {
            address: A,
            vport: 53211,
            source_vport: 7551
        }
    );
    assert_eq!(game, b"discovery response");

    // The sender does not hear its own broadcast.
    assert!(silent(&a_raknet).await);
    assert!(silent(&b_nethernet).await);
    // C holds a tunnel to A, but is in none of the sessions A broadcasts to.
    assert!(silent(&c_raknet).await);
    assert!(silent(&c_nethernet).await);
    assert_eq!(a.traffic.broadcasts.totals().too_fast, 0);
    assert!(a.traffic.broadcasts.totals().copies >= 1);

    a.tunnels.close().await;
    b.tunnels.close().await;
    c.tunnels.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn broadcasts_are_limited_in_size_and_rate_and_need_an_audience() {
    let relay = support::relay::start().await;
    let a = side(&relay).await;
    let game = game_socket(&a, 19132).await;
    let to = (Ipv4Addr::LOCALHOST, a.traffic.udp_port());

    // In no session: accepted and counted, sent to no one.
    game.send_to(&datagram(LIMITED, 19132, 19132, b"x"), to)
        .await
        .unwrap();
    wait_for(|| a.traffic.broadcasts.totals().alone == 1).await;

    let large = vec![0u8; LARGEST_BROADCAST + 1];
    game.send_to(&datagram(LIMITED, 19132, 19132, &large), to)
        .await
        .unwrap();
    wait_for(|| a.traffic.broadcasts.totals().too_large == 1).await;

    // A flood passes the burst and no more.
    for _ in 0..(BROADCAST_BURST * 2) {
        game.send_to(&datagram(SUBNET, 19132, 19132, b"flood"), to)
            .await
            .unwrap();
    }
    wait_for(|| {
        let totals = a.traffic.broadcasts.totals();
        totals.accepted + totals.too_fast > u64::from(BROADCAST_BURST * 2)
    })
    .await;
    let totals = a.traffic.broadcasts.totals();
    assert!(totals.too_fast >= 10, "{totals:?}");
    assert!(
        totals.accepted <= u64::from(BROADCAST_BURST) + 20,
        "{totals:?}"
    );
    a.tunnels.close().await;
}

async fn wait_for(mut done: impl FnMut() -> bool) {
    for _ in 0..200 {
        if done() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("the counters never reached the expected values");
}
