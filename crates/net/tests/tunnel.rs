//! Game traffic from one bridge to another through two tunnel endpoints and a relay, all on
//! loopback.

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
    let mut a = side(&relay).await;
    let mut b = side(&relay).await;

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
