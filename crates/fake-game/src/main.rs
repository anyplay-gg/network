//! `anyplay-fake-game`: started like a game, with the variables the launcher sets, it plays
//! one of a few parts so that two players can try the network without a game.

use std::net::Ipv4Addr;
use std::time::{Duration, Instant};

use anyplay_bridge_protocol::{ActivateResult, Message, PeerState};
use anyplay_fake_game::{Bridge, session_id};

const USAGE: &str = "\
anyplay-fake-game: plays a game's side of the network

  anyplay-fake-game state           shows the sign-in state the game would see
  anyplay-fake-game host            opens a session and answers every datagram
  anyplay-fake-game join <session>  joins a session and measures the round trip

Start it with ANYPS5_NET_ENDPOINT and ANYPS5_NET_TOKEN set, as the launcher starts a game.
";

const PORT: u16 = 3658;

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> std::process::ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        ["state"] => state().await,
        ["host"] => host().await,
        ["join", session] => join(session).await,
        _ => {
            eprint!("{USAGE}");
            return std::process::ExitCode::from(2);
        }
    };
    match result {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            std::process::ExitCode::FAILURE
        }
    }
}

async fn signed_in() -> Result<Bridge, String> {
    let bridge = Bridge::from_env().await?;
    let state = bridge
        .wait_state(Duration::from_secs(10), |s| s.signed_in)
        .await
        .ok_or("not signed in to the network")?;
    println!(
        "signed in as {} (account {}, address {})",
        state.handle, state.account_id, state.address
    );
    Ok(bridge)
}

async fn state() -> Result<(), String> {
    let bridge = Bridge::from_env().await?;
    tokio::time::sleep(Duration::from_millis(500)).await;
    println!("{:?}", bridge.state());
    Ok(())
}

async fn host() -> Result<(), String> {
    let bridge = signed_in().await?;
    let reply = bridge
        .web(
            "anyplay",
            "POST",
            "/v1/sessions",
            br#"{"max_members":4,"visibility":"friends"}"#,
        )
        .await?;
    let session = session_id(&reply).ok_or(format!("no session: status {}", reply.status))?;
    println!("session {session}");
    if bridge.activate(&session).await? != ActivateResult::Done {
        return Err("the session could not be activated".into());
    }
    let socket = bridge.socket(PORT).await?;
    let peers = async {
        while let Some(message) = bridge.next_event(Duration::from_secs(3600)).await {
            if let Message::Peer(peer) = message {
                println!(
                    "{} is {:?} at {}",
                    peer.handle, peer.peer_state, peer.address
                );
            }
        }
    };
    let echo = async {
        loop {
            let (from, port, bytes) = socket.recv().await.map_err(|e| e.to_string())?;
            socket
                .send_to(from, port, &bytes)
                .await
                .map_err(|e| e.to_string())?;
        }
    };
    tokio::select! {
        () = peers => Ok(()),
        result = echo => result,
    }
}

async fn join(session: &str) -> Result<(), String> {
    let bridge = signed_in().await?;
    let path = format!("/v1/sessions/{session}/join");
    let reply = bridge.web("anyplay", "POST", &path, b"").await?;
    if reply.status != 200 {
        return Err(format!("could not join: status {}", reply.status));
    }
    bridge.activate(session).await?;
    let host = bridge
        .wait_event(Duration::from_secs(60), |message| match message {
            Message::Peer(peer) if peer.peer_state == PeerState::Connected => Some(peer.address),
            _ => None,
        })
        .await
        .ok_or("no tunnel to the host in a minute")?;
    let socket = bridge.socket(PORT + 1).await?;
    let mut rtts = Vec::new();
    for i in 0u32..100 {
        let sent = Instant::now();
        socket
            .send_to(host, PORT, &i.to_le_bytes())
            .await
            .map_err(|e| e.to_string())?;
        if let Ok(Ok((_, _, bytes))) =
            tokio::time::timeout(Duration::from_secs(1), socket.recv()).await
            && bytes == i.to_le_bytes()
        {
            rtts.push(sent.elapsed());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    rtts.sort();
    report(host, &rtts);
    Ok(())
}

fn report(host: Ipv4Addr, rtts: &[Duration]) {
    if rtts.is_empty() {
        println!("no answer from {host}");
        return;
    }
    let at = |q: f64| rtts[((rtts.len() - 1) as f64 * q) as usize].as_secs_f64() * 1000.0;
    println!(
        "{host}: {} of 100 answered, round trip p50 {:.1} ms, p95 {:.1} ms, max {:.1} ms",
        rtts.len(),
        at(0.5),
        at(0.95),
        at(1.0)
    );
}
