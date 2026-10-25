//! Game traffic from one bridge to another through two tunnel endpoints and a relay, all on
//! loopback: on whatever path the endpoints find, and through the relay only.

mod support;

use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr};
use std::time::Duration;

use anyplay_bridge_protocol::{BindResult, TrafficHeader, Transport};
use anyplay_net::traffic::Traffic;
use anyplay_net::tunnel::{self, Link, LinkChange, Options, RelayAccess, Tunnels};
use iroh::SecretKey;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::mpsc;

const A: Ipv4Addr = Ipv4Addr::new(198, 18, 0, 1);
const B: Ipv4Addr = Ipv4Addr::new(198, 18, 0, 2);

struct Side {
    traffic: Traffic,
    tunnels: Tunnels,
    changes: mpsc::UnboundedReceiver<LinkChange>,
}

async fn side(relay: &support::relay::TestRelay, relay_only: bool) -> Side {
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
            relay_only,
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

async fn linked(changes: &mut mpsc::UnboundedReceiver<LinkChange>) -> Link {
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let change = changes.recv().await.expect("changes");
            if matches!(change.link, Link::Direct | Link::Relay) {
                return change.link;
            }
        }
    })
    .await
    .expect("no tunnel in time")
}

/// Sends from `socket` through `traffic` until `receiver` gets a datagram, and returns it.
async fn exchange(
    socket: &UdpSocket,
    traffic: &Traffic,
    datagram: &[u8],
    receiver: &UdpSocket,
) -> Vec<u8> {
    let mut buf = vec![0u8; 70_000];
    for _ in 0..50 {
        socket
            .send_to(datagram, (Ipv4Addr::LOCALHOST, traffic.udp_port()))
            .await
            .unwrap();
        if let Ok(Ok(len)) =
            tokio::time::timeout(Duration::from_millis(200), receiver.recv(&mut buf)).await
        {
            return buf[..len].to_vec();
        }
    }
    panic!("the datagram never arrived");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn game_traffic_crosses_the_tunnel_both_as_datagrams_and_as_streams() {
    let relay = support::relay::start().await;
    let mut a = side(&relay, false).await;
    let mut b = side(&relay, false).await;

    a.tunnels
        .set_peers(HashMap::from([(b.tunnels.id(), peer(2, B, &b))]));
    b.tunnels
        .set_peers(HashMap::from([(a.tunnels.id(), peer(1, A, &a))]));
    let link_a = linked(&mut a.changes).await;
    linked(&mut b.changes).await;
    assert_eq!(a.tunnels.link_of(B).map(|(link, _)| link), Some(link_a));

    let game_a = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let game_b = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let port = |s: &UdpSocket| s.local_addr().unwrap().port();
    assert_eq!(
        a.traffic.bind(3658, Transport::Datagram, port(&game_a)),
        BindResult::Bound
    );
    assert_eq!(
        b.traffic.bind(3658, Transport::Datagram, port(&game_b)),
        BindResult::Bound
    );

    let mut datagram = TrafficHeader {
        address: B,
        vport: 3658,
        source_vport: 3658,
    }
    .encode()
    .to_vec();
    datagram.extend_from_slice(b"hello");
    let got = exchange(&game_a, &a.traffic, &datagram, &game_b).await;
    let (header, game) = TrafficHeader::split(&got).unwrap();
    assert_eq!(
        header,
        TrafficHeader {
            address: A,
            vport: 3658,
            source_vport: 3658
        }
    );
    assert_eq!(game, b"hello");

    let mut large = TrafficHeader {
        address: A,
        vport: 3658,
        source_vport: 9,
    }
    .encode()
    .to_vec();
    large.extend(std::iter::repeat_n(7u8, 5000));
    let got = exchange(&game_b, &b.traffic, &large, &game_a).await;
    let (header, game) = TrafficHeader::split(&got).unwrap();
    assert_eq!((header.address, header.source_vport), (B, 9));
    assert_eq!(game.len(), 5000);

    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let listening = listener.local_addr().unwrap().port();
    assert_eq!(
        b.traffic.bind(7000, Transport::Stream, listening),
        BindResult::Bound
    );
    let key_b = b.traffic.stream_key();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut head = [0u8; 16 + TrafficHeader::LEN];
        stream.read_exact(&mut head).await.unwrap();
        assert_eq!(&head[..16], &key_b);
        let (header, _) = TrafficHeader::split(&head[16..]).unwrap();
        assert_eq!(
            header,
            TrafficHeader {
                address: A,
                vport: 7000,
                source_vport: 5000
            }
        );
        let mut ping = [0u8; 4];
        stream.read_exact(&mut ping).await.unwrap();
        assert_eq!(&ping, b"ping");
        stream.write_all(b"pong").await.unwrap();
    });
    let mut stream = TcpStream::connect((Ipv4Addr::LOCALHOST, a.traffic.tcp_port()))
        .await
        .unwrap();
    let mut opening = a.traffic.stream_key().to_vec();
    opening.extend_from_slice(
        &TrafficHeader {
            address: B,
            vport: 7000,
            source_vport: 5000,
        }
        .encode(),
    );
    opening.extend_from_slice(b"ping");
    stream.write_all(&opening).await.unwrap();
    let mut pong = [0u8; 4];
    tokio::time::timeout(Duration::from_secs(10), stream.read_exact(&mut pong))
        .await
        .expect("no answer on the stream")
        .unwrap();
    assert_eq!(&pong, b"pong");
    server.await.unwrap();

    let mut wrong_key = TcpStream::connect((Ipv4Addr::LOCALHOST, a.traffic.tcp_port()))
        .await
        .unwrap();
    let mut opening = vec![0u8; 16];
    opening.extend_from_slice(
        &TrafficHeader {
            address: B,
            vport: 7000,
            source_vport: 1,
        }
        .encode(),
    );
    wrong_key.write_all(&opening).await.unwrap();
    let mut rest = Vec::new();
    let closed = tokio::time::timeout(Duration::from_secs(5), wrong_key.read_to_end(&mut rest))
        .await
        .expect("a stream with a wrong key stays open");
    assert!(closed.is_err() || rest.is_empty());

    a.tunnels.set_peers(HashMap::new());
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if a.changes.recv().await.expect("changes").link == Link::Closed {
                return;
            }
        }
    })
    .await
    .expect("the tunnel did not close");
    assert_eq!(a.tunnels.link_of(B), None);
    a.tunnels.close().await;
    b.tunnels.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_relay_only_tunnel_carries_game_traffic_through_the_relay() {
    let relay = support::relay::start().await;
    let mut a = side(&relay, true).await;
    let mut b = side(&relay, true).await;

    a.tunnels
        .set_peers(HashMap::from([(b.tunnels.id(), peer(2, B, &b))]));
    b.tunnels
        .set_peers(HashMap::from([(a.tunnels.id(), peer(1, A, &a))]));
    assert_eq!(linked(&mut a.changes).await, Link::Relay);
    assert_eq!(linked(&mut b.changes).await, Link::Relay);

    let game_a = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let game_b = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let port = |s: &UdpSocket| s.local_addr().unwrap().port();
    for (side, game) in [(&a, &game_a), (&b, &game_b)] {
        assert_eq!(
            side.traffic.bind(3658, Transport::Datagram, port(game)),
            BindResult::Bound
        );
    }
    for (from, game_from, to, game_to, address) in
        [(&a, &game_a, B, &game_b, A), (&b, &game_b, A, &game_a, B)]
    {
        let mut datagram = TrafficHeader {
            address: to,
            vport: 3658,
            source_vport: 3658,
        }
        .encode()
        .to_vec();
        datagram.extend_from_slice(b"relayed");
        let got = exchange(game_from, &from.traffic, &datagram, game_to).await;
        let (header, game) = TrafficHeader::split(&got).unwrap();
        assert_eq!(header.address, address);
        assert_eq!(game, b"relayed");
    }
    // Traffic does not open a direct path either.
    assert_eq!(
        a.tunnels.link_of(B).map(|(link, _)| link),
        Some(Link::Relay)
    );
    assert_eq!(
        b.tunnels.link_of(A).map(|(link, _)| link),
        Some(Link::Relay)
    );
    a.tunnels.close().await;
    b.tunnels.close().await;
}

/// Two sides, the first one dials (the smaller endpoint ID dials).
async fn dialer_and_acceptor(relay: &support::relay::TestRelay) -> (Side, Side) {
    let one = side(relay, true).await;
    let two = side(relay, true).await;
    if one.tunnels.id() < two.tunnels.id() {
        (one, two)
    } else {
        (two, one)
    }
}

/// Connections that end at once are redialed with a growing pause, not in a tight loop.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_tunnel_that_ends_at_once_is_redialed_with_a_growing_pause() {
    let relay = support::relay::start().await;
    let (mut dialer, acceptor) = dialer_and_acceptor(&relay).await;
    // The acceptor does not list the dialer, so it closes every connection as soon as it
    // arrives.
    dialer.tunnels.set_peers(HashMap::from([(
        acceptor.tunnels.id(),
        peer(2, B, &acceptor),
    )]));

    // Every connection that ends reports `Connecting` again; the first report is the start.
    let window = tokio::time::sleep(Duration::from_secs(4));
    tokio::pin!(window);
    let mut ends = 0u32;
    loop {
        tokio::select! {
            _ = &mut window => break,
            change = dialer.changes.recv() => {
                if change.expect("changes").link == Link::Connecting {
                    ends += 1;
                }
            }
        }
    }
    println!("{ends} connection reports in 4 s");
    // Attempts at about 0, 0.5, 1.5 and 3.5 s.
    assert!(
        (3..=7).contains(&ends),
        "{ends} connection reports in 4 s, a growing pause allows about 5"
    );
    dialer.tunnels.close().await;
    acceptor.tunnels.close().await;
}

/// A connection that held for a while resets the pause: after short-lived connections have
/// raised it, the redial of a healthy tunnel still comes at once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_tunnel_that_held_for_a_while_is_redialed_quickly() {
    let relay = support::relay::start().await;
    let (mut dialer, mut acceptor) = dialer_and_acceptor(&relay).await;
    let listed = |of: &Side, account, address| {
        HashMap::from([(of.tunnels.id(), peer(account, address, of))])
    };
    // Short-lived connections first (the acceptor does not list the dialer yet): attempts at
    // about 0, 0.5 and 1.5 s raise the pause to 4 s.
    dialer.tunnels.set_peers(listed(&acceptor, 2, B));
    tokio::time::sleep(Duration::from_millis(2500)).await;
    acceptor.tunnels.set_peers(listed(&dialer, 1, A));
    // The attempt at about 3.5 s stands.
    linked(&mut dialer.changes).await;
    linked(&mut acceptor.changes).await;

    // Hold it past the good-connection mark, then end it from the acceptor's side.
    tokio::time::sleep(Duration::from_millis(10_200)).await;
    acceptor.tunnels.set_peers(HashMap::new());
    acceptor.tunnels.set_peers(listed(&dialer, 1, A));
    while acceptor.changes.try_recv().is_ok() {}
    while dialer.changes.try_recv().is_ok() {}
    let ended = std::time::Instant::now();
    linked(&mut dialer.changes).await;
    let took = ended.elapsed();
    // Without the reset the pause would still be 4 s.
    assert!(
        took < Duration::from_millis(1500),
        "redial took {took:?} after a good connection"
    );
    dialer.tunnels.close().await;
    acceptor.tunnels.close().await;
}
