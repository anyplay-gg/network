//! A game's side of the bridge protocol, in Rust: what the bridge in AnyPS5 does, without a
//! game. Tests drive the net service with it, and `anyplay-fake-game` lets a person try the
//! network between two PCs.

use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyplay_bridge_protocol::{
    Activate, ActivateResult, Auth, Bind, BindResult, Decoder, Frame, Hello, Kind, Message, Nonce,
    Role, SessionKind, ShareMode, ShareOpen, SharePad, ShareResult, State, TrafficHeader,
    Transport, WebReply, WebRequest, Welcome, proof, verify,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpStream, UdpSocket};
use tokio::sync::{mpsc, oneshot, watch};

/// How long a request waits for its reply.
const PATIENCE: Duration = Duration::from_secs(30);

type Waiters = Arc<Mutex<HashMap<u32, oneshot::Sender<Message>>>>;

/// What the engine's share play does when the net service speaks to it, with the rules the
/// bridge in AnyPS5 follows (`spec/share-play.md`, Bridge protocol additions): it takes a pad
/// only in a mode that has a pad, on the slot of that mode, with no reserved button bit, and
/// the guest's pad is neutral whenever the mode or the share ends.
#[derive(Clone, Debug, Default)]
pub struct ShareEngine {
    /// The answer to the next `share_open`; `None` is "copying".
    pub refuse_with: Option<ShareResult>,
    /// The last `share_open` that was answered with "copying".
    pub opened: Option<ShareOpen>,
    /// The mode the net service last set; `None` before an open.
    pub mode: Option<ShareMode>,
    /// A `share_close` came after the open.
    pub closed: bool,
    /// Every pad state the engine took, in order.
    pub taken: Vec<SharePad>,
    /// Pad states the engine refused (wrong mode, wrong slot, reserved buttons).
    pub refused: u32,
    /// What the game sees on pad 1 and pad 2 from the guest now; `None` when no guest pad is
    /// connected.
    pub now: [Option<SharePad>; 2],
    /// Frames the net service sent that this engine does not know.
    pub unknown: u32,
}

impl ShareEngine {
    fn slot_of(mode: ShareMode) -> Option<u8> {
        match mode {
            ShareMode::HandOver => Some(0),
            ShareMode::Together => Some(1),
            ShareMode::Watch | ShareMode::Unknown => None,
        }
    }

    fn disconnect(&mut self) {
        self.now = [None, None];
    }

    fn apply(&mut self, message: &Message) -> Option<ShareResult> {
        match message {
            Message::ShareOpen(open) => {
                if self.opened.is_some() && !self.closed {
                    return Some(ShareResult::Running);
                }
                if let Some(result) = self.refuse_with {
                    return Some(result);
                }
                self.opened = Some(open.clone());
                self.closed = false;
                self.mode = Some(open.mode);
                self.taken.clear();
                self.refused = 0;
                self.disconnect();
                Some(ShareResult::Copying)
            }
            Message::ShareClose => {
                self.closed = true;
                self.disconnect();
                None
            }
            Message::ShareMode { mode } => {
                self.mode = Some(*mode);
                self.disconnect();
                None
            }
            Message::SharePad(pad) => {
                let allowed = self.opened.is_some()
                    && !self.closed
                    && self.mode.and_then(Self::slot_of) == Some(pad.slot)
                    && pad.buttons & !0x7FFF == 0;
                if allowed {
                    self.taken.push(*pad);
                    self.now[usize::from(pad.slot)] = Some(*pad);
                } else {
                    self.refused += 1;
                }
                None
            }
            _ => {
                self.unknown += 1;
                None
            }
        }
    }
}

/// A connected bridge.
pub struct Bridge {
    writer: mpsc::UnboundedSender<Frame>,
    waiters: Waiters,
    next: Mutex<u32>,
    welcome: Welcome,
    state: watch::Receiver<State>,
    events: tokio::sync::Mutex<mpsc::UnboundedReceiver<Message>>,
    share: Arc<Mutex<ShareEngine>>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl Drop for Bridge {
    /// The game exits: both halves of the connection go, and the net service sees it close.
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

impl Bridge {
    /// Connects as the bridge does: to `endpoint` with the hex `token` of this game start.
    pub async fn connect(endpoint: &str, token: &str) -> Result<Bridge, String> {
        let address: SocketAddr = endpoint.parse().map_err(|_| "endpoint is not an address")?;
        if !address.ip().is_loopback() {
            return Err("the endpoint must be on loopback".into());
        }
        let token = decode_hex(token).ok_or("token is not hex")?;
        let stream = TcpStream::connect(address)
            .await
            .map_err(|e| e.to_string())?;
        stream.set_nodelay(true).map_err(|e| e.to_string())?;
        let (mut read, mut write) = stream.into_split();
        let bridge_nonce: Nonce = rand::random();
        let hello = Frame::event(&Message::Hello(Hello {
            version: anyplay_bridge_protocol::VERSION,
            nonce: bridge_nonce.to_vec(),
        }))
        .map_err(|e| e.to_string())?;
        let mut bytes = Vec::new();
        hello.encode(&mut bytes).map_err(|e| e.to_string())?;
        write.write_all(&bytes).await.map_err(|e| e.to_string())?;

        let mut decoder = Decoder::new();
        let mut buf = vec![0u8; 64 * 1024];
        let mut answered = false;
        let welcome = loop {
            if let Some(frame) = decoder.next_frame().map_err(|e| e.to_string())? {
                match frame.message().map_err(|e| e.to_string())? {
                    // The net service proves the token first; only then does the bridge answer.
                    Some(Message::Challenge(challenge)) if !answered => {
                        if challenge.version != anyplay_bridge_protocol::VERSION
                            || !verify(
                                &token,
                                Role::Service,
                                &bridge_nonce,
                                &challenge.nonce,
                                &challenge.proof,
                            )
                        {
                            return Err("the net service did not prove the token".into());
                        }
                        let proof = proof(&token, Role::Bridge, &bridge_nonce, &challenge.nonce);
                        let auth = Frame::event(&Message::Auth(Auth { proof }))
                            .map_err(|e| e.to_string())?;
                        let mut bytes = Vec::new();
                        auth.encode(&mut bytes).map_err(|e| e.to_string())?;
                        write.write_all(&bytes).await.map_err(|e| e.to_string())?;
                        answered = true;
                        continue;
                    }
                    Some(Message::Welcome(welcome)) if answered => break welcome,
                    Some(Message::Reject(reject)) => {
                        return Err(format!("rejected: {}", reject.reason));
                    }
                    _ => return Err("the handshake went wrong: unexpected frame".into()),
                }
            }
            let n = tokio::time::timeout(Duration::from_secs(5), read.read(&mut buf))
                .await
                .map_err(|_| "no welcome in time")?
                .map_err(|e| e.to_string())?;
            if n == 0 {
                return Err("the net service closed the connection".into());
            }
            decoder.push(&buf[..n]);
        };

        let (writer, mut outgoing) = mpsc::unbounded_channel::<Frame>();
        let writing = tokio::spawn(async move {
            let mut bytes = Vec::new();
            while let Some(frame) = outgoing.recv().await {
                bytes.clear();
                if frame.encode(&mut bytes).is_ok() && write.write_all(&bytes).await.is_err() {
                    return;
                }
            }
        });
        let waiters: Waiters = Arc::default();
        let (state_tx, state) = watch::channel(State::signed_out());
        let (event_tx, events) = mpsc::unbounded_channel();
        let share: Arc<Mutex<ShareEngine>> = Arc::default();
        let reading = {
            let share = share.clone();
            let waiters = waiters.clone();
            let writer = writer.clone();
            tokio::spawn(async move {
                loop {
                    while let Ok(Some(frame)) = decoder.next_frame() {
                        let message = match frame.message() {
                            Ok(Some(message)) => message,
                            Ok(None) | Err(_) => {
                                if frame.request != 0 && !frame.kind.is_reply() {
                                    let _ = writer.send(frame.unsupported());
                                }
                                continue;
                            }
                        };
                        if frame.request != 0
                            && (frame.kind.is_reply() || frame.kind == Kind::UNSUPPORTED)
                        {
                            let waiter = waiters
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .remove(&frame.request);
                            if let Some(waiter) = waiter {
                                let _ = waiter.send(message);
                            }
                            continue;
                        }
                        match message {
                            Message::State(new) => {
                                let _ = state_tx.send(new);
                            }
                            share_message @ (Message::ShareOpen(_)
                            | Message::ShareClose
                            | Message::ShareMode { .. }
                            | Message::SharePad(_)) => {
                                let answer = share
                                    .lock()
                                    .unwrap_or_else(|e| e.into_inner())
                                    .apply(&share_message);
                                if let (Some(result), true) = (answer, frame.request != 0)
                                    && let Ok(reply) = Frame::numbered(
                                        &Message::ShareOpened { result },
                                        frame.request,
                                    )
                                {
                                    let _ = writer.send(reply);
                                }
                            }
                            other => {
                                let _ = event_tx.send(other);
                            }
                        }
                    }
                    match read.read(&mut buf).await {
                        Ok(0) | Err(_) => {
                            let _ = state_tx.send(State::signed_out());
                            return;
                        }
                        Ok(n) => decoder.push(&buf[..n]),
                    }
                }
            })
        };
        Ok(Bridge {
            writer,
            waiters,
            next: Mutex::new(1),
            welcome,
            state,
            events: tokio::sync::Mutex::new(events),
            share,
            tasks: vec![writing, reading],
        })
    }

    /// What the engine's share play has seen and done so far.
    pub fn share_engine(&self) -> ShareEngine {
        self.share.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Makes the engine answer the next `share_open` with `result` instead of "copying".
    pub fn refuse_share_open(&self, result: ShareResult) {
        self.share
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .refuse_with = Some(result);
    }

    /// Connects with the variables the launcher gives a game.
    pub async fn from_env() -> Result<Bridge, String> {
        let endpoint =
            std::env::var("ANYPS5_NET_ENDPOINT").map_err(|_| "ANYPS5_NET_ENDPOINT is not set")?;
        let token = std::env::var("ANYPS5_NET_TOKEN").map_err(|_| "ANYPS5_NET_TOKEN is not set")?;
        Self::connect(&endpoint, &token).await
    }

    pub fn welcome(&self) -> &Welcome {
        &self.welcome
    }

    /// The latest sign-in state.
    pub fn state(&self) -> State {
        self.state.borrow().clone()
    }

    /// Waits until the state satisfies `test`, at most `limit`.
    pub async fn wait_state(
        &self,
        limit: Duration,
        test: impl Fn(&State) -> bool,
    ) -> Option<State> {
        let mut state = self.state.clone();
        tokio::time::timeout(limit, async {
            loop {
                if test(&state.borrow()) {
                    return state.borrow().clone();
                }
                if state.changed().await.is_err() {
                    return state.borrow().clone();
                }
            }
        })
        .await
        .ok()
        .filter(|s| test(s))
    }

    /// The next event from the net service other than a state change.
    pub async fn next_event(&self, limit: Duration) -> Option<Message> {
        let mut events = self.events.lock().await;
        tokio::time::timeout(limit, events.recv())
            .await
            .ok()
            .flatten()
    }

    /// Waits for an event that `pick` accepts, skipping the others.
    pub async fn wait_event<T>(
        &self,
        limit: Duration,
        pick: impl Fn(Message) -> Option<T>,
    ) -> Option<T> {
        let deadline = tokio::time::Instant::now() + limit;
        loop {
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            let message = self.next_event(left).await?;
            if let Some(found) = pick(message) {
                return Some(found);
            }
        }
    }

    /// Sends an event to the net service.
    pub fn send(&self, message: &Message) {
        if let Ok(frame) = Frame::event(message) {
            let _ = self.writer.send(frame);
        }
    }

    /// Sends a request and waits for its reply.
    pub async fn request(&self, message: &Message) -> Result<Message, String> {
        let number = {
            let mut next = self.next.lock().unwrap_or_else(|e| e.into_inner());
            let number = *next;
            *next = next.wrapping_add(1).max(1);
            number
        };
        let (tx, rx) = oneshot::channel();
        self.waiters
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(number, tx);
        let frame = Frame::numbered(message, number).map_err(|e| e.to_string())?;
        self.writer
            .send(frame)
            .map_err(|_| "the connection is closed")?;
        match tokio::time::timeout(PATIENCE, rx).await {
            Ok(Ok(reply)) => Ok(reply),
            Ok(Err(_)) => Err("the connection closed".into()),
            Err(_) => {
                self.waiters
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&number);
                Err("no reply in time".into())
            }
        }
    }

    /// A web API request as a game sends it.
    pub async fn web(
        &self,
        api_group: &str,
        method: &str,
        path: &str,
        body: &[u8],
    ) -> Result<WebReply, String> {
        let request = Message::WebRequest(WebRequest {
            user_id: 0x1000_0000,
            api_group: api_group.into(),
            path: path.into(),
            method: method.into(),
            headers: vec![("Content-Type".into(), "application/json".into())],
            body: body.to_vec(),
        });
        match self.request(&request).await? {
            Message::WebReply(reply) => Ok(reply),
            Message::Unsupported => Ok(WebReply::not_offered()),
            other => Err(format!("unexpected reply {other:?}")),
        }
    }

    pub async fn activate(&self, session: &str) -> Result<ActivateResult, String> {
        let request = Message::Activate(Activate {
            user_id: 0x1000_0000,
            session: session.into(),
            session_kind: SessionKind::Player,
        });
        match self.request(&request).await? {
            Message::Activated { result } => Ok(result),
            other => Err(format!("unexpected reply {other:?}")),
        }
    }

    /// Opens a game datagram socket on virtual port `vport`.
    pub async fn socket(&self, vport: u16) -> Result<GameSocket, String> {
        let socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .map_err(|e| e.to_string())?;
        let loopback_port = socket.local_addr().map_err(|e| e.to_string())?.port();
        let request = Message::Bind(Bind {
            vport,
            transport: Transport::Datagram,
            loopback_port,
        });
        match self.request(&request).await? {
            Message::Bound {
                result: BindResult::Bound,
            } => Ok(GameSocket {
                socket,
                vport,
                service: SocketAddr::from((Ipv4Addr::LOCALHOST, self.welcome.udp_port)),
            }),
            other => Err(format!("bind refused: {other:?}")),
        }
    }
}

/// A game's datagram socket on a virtual port.
pub struct GameSocket {
    socket: UdpSocket,
    vport: u16,
    service: SocketAddr,
}

impl GameSocket {
    pub async fn send_to(
        &self,
        address: Ipv4Addr,
        vport: u16,
        bytes: &[u8],
    ) -> std::io::Result<()> {
        let mut datagram = TrafficHeader {
            address,
            vport,
            source_vport: self.vport,
        }
        .encode()
        .to_vec();
        datagram.extend_from_slice(bytes);
        self.socket.send_to(&datagram, self.service).await.map(drop)
    }

    /// The next datagram: who sent it from which port, and its bytes.
    pub async fn recv(&self) -> std::io::Result<(Ipv4Addr, u16, Vec<u8>)> {
        let mut buf = vec![0u8; 70_000];
        loop {
            let (len, from) = self.socket.recv_from(&mut buf).await?;
            if from != self.service {
                continue;
            }
            if let Some((header, bytes)) = TrafficHeader::split(&buf[..len]) {
                return Ok((header.address, header.source_vport, bytes.to_vec()));
            }
        }
    }
}

fn decode_hex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(text.get(i..i + 2)?, 16).ok())
        .collect()
}

/// The session in a reply of the network's own API group.
pub fn session_id(reply: &WebReply) -> Option<String> {
    let value: serde_json::Value = serde_json::from_slice(&reply.body).ok()?;
    value.get("id")?.as_str().map(str::to_owned)
}

#[cfg(test)]
mod tests {
    #[test]
    fn hex_tokens_decode() {
        assert_eq!(super::decode_hex("00ff10"), Some(vec![0, 255, 16]));
        assert_eq!(super::decode_hex("0"), None);
        assert_eq!(super::decode_hex("zz"), None);
    }
}
