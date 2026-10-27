//! The loopback listener that games' bridges connect to.
//!
//! Each connection is read and written by its own task. The task runs the handshake itself
//! (hello, challenge, auth; ADR 0039), so the token never leaves it and the service hears only of
//! bridges that proved they know it. Frames are size-checked before they are buffered, and until
//! the handshake is done they may carry [`MAX_HELLO_PAYLOAD`] bytes at most.

use std::net::Ipv4Addr;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

pub use anyplay_bridge_protocol::MAX_HELLO_PAYLOAD;
use anyplay_bridge_protocol::{
    Auth, Challenge, Decoder, Frame, MAX_PAYLOAD, Message, NONCE_LEN, Nonce, Reject, Role, VERSION,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc::{self, UnboundedSender};
use tokio::sync::oneshot;

/// How long a bridge has to finish the handshake, from connecting to its auth frame.
pub const HANDSHAKE_TIME: Duration = Duration::from_secs(5);

/// The token of the game start the listener accepts bridges for, shared with the service. Empty
/// while no game is starting or running.
#[derive(Clone, Debug, Default)]
pub struct Tokens(Arc<Mutex<Option<Vec<u8>>>>);

impl Tokens {
    /// Tokens that hold `token` from the start.
    pub fn fixed(token: Vec<u8>) -> Self {
        Tokens(Arc::new(Mutex::new(Some(token))))
    }

    /// Sets the token of the game start, or clears it with `None`.
    pub fn set(&self, token: Option<Vec<u8>>) {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner) = token;
    }

    fn get(&self) -> Option<Vec<u8>> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

/// What the bridge listener hands to the service.
#[derive(Debug)]
pub enum BridgeInput {
    /// A bridge proved that it knows the token. The service answers through `verdict`.
    Hello {
        connection: u64,
        frames: UnboundedSender<Frame>,
        verdict: oneshot::Sender<Result<(), String>>,
    },
    /// A frame after the welcome.
    Frame { connection: u64, frame: Frame },
    /// The connection ended.
    Closed { connection: u64 },
}

/// Listens on a loopback port until the process ends.
pub async fn listen(out: UnboundedSender<BridgeInput>, tokens: Tokens) -> std::io::Result<u16> {
    listen_within(out, tokens, HANDSHAKE_TIME).await
}

/// [`listen`] with another time allowed for the handshake, for tests.
pub async fn listen_within(
    out: UnboundedSender<BridgeInput>,
    tokens: Tokens,
    handshake_time: Duration,
) -> std::io::Result<u16> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
    let port = listener.local_addr()?.port();
    tokio::spawn(async move {
        let mut next = 1u64;
        loop {
            let Ok((stream, from)) = listener.accept().await else {
                // A failing accept (out of file handles, say) repeats at once: do not spin on it.
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                continue;
            };
            if !from.ip().is_loopback() {
                continue;
            }
            let connection = next;
            next += 1;
            let out = out.clone();
            let tokens = tokens.clone();
            tokio::spawn(async move {
                serve(connection, stream, &out, &tokens, handshake_time).await;
                let _ = out.send(BridgeInput::Closed { connection });
            });
        }
    });
    Ok(port)
}

/// The next whole frame, or `None` when the connection ends, breaks the framing or exceeds the
/// decoder's limit.
async fn next_frame(
    read: &mut OwnedReadHalf,
    decoder: &mut Decoder,
    buf: &mut [u8],
) -> Option<Frame> {
    loop {
        match decoder.next_frame() {
            Ok(Some(frame)) => return Some(frame),
            Ok(None) => {}
            Err(_) => return None,
        }
        match read.read(buf).await {
            Ok(0) | Err(_) => return None,
            Ok(n) => decoder.push(&buf[..n]),
        }
    }
}

async fn send(write: &mut OwnedWriteHalf, message: &Message) -> bool {
    let Ok(frame) = Frame::event(message) else {
        return false;
    };
    let mut bytes = Vec::new();
    frame.encode(&mut bytes).is_ok() && write.write_all(&bytes).await.is_ok()
}

/// How a handshake ends without a bridge: `Some(reason)` is sent to the bridge as a reject, and
/// `None` closes the connection without a word.
type Refusal = Option<String>;

fn refuse(reason: impl Into<String>) -> Refusal {
    Some(reason.into())
}

/// Runs the handshake: the bridge's hello with its nonce, the service's challenge with its nonce
/// and its proof, then the bridge's auth with its proof. Each side proves the token over both
/// nonces, and neither sends the token.
async fn handshake(
    read: &mut OwnedReadHalf,
    write: &mut OwnedWriteHalf,
    decoder: &mut Decoder,
    buf: &mut [u8],
    tokens: &Tokens,
) -> Result<(), Refusal> {
    let frame = next_frame(read, decoder, buf).await.ok_or(None)?;
    let Ok(Some(Message::Hello(hello))) = Message::parse(frame.kind, &frame.payload) else {
        return Err(None);
    };
    if hello.version != VERSION {
        return Err(refuse(format!(
            "the bridge speaks protocol version {}, this net service speaks version {VERSION}: \
             update AnyPS5 and the launcher so that both match",
            hello.version
        )));
    }
    let Some(bridge_nonce) = hello
        .nonce
        .get(..NONCE_LEN)
        .and_then(|nonce| Nonce::try_from(nonce).ok())
    else {
        return Err(refuse("the hello carries no nonce"));
    };
    let Some(token) = tokens.get() else {
        return Err(refuse("no game is starting"));
    };

    let service_nonce: Nonce = rand::random();
    let challenge = Message::Challenge(Challenge {
        version: VERSION,
        nonce: service_nonce,
        proof: anyplay_bridge_protocol::proof(&token, Role::Service, &bridge_nonce, &service_nonce),
    });
    if !send(write, &challenge).await {
        return Err(None);
    }

    let frame = next_frame(read, decoder, buf).await.ok_or(None)?;
    let Ok(Some(Message::Auth(Auth { proof }))) = Message::parse(frame.kind, &frame.payload) else {
        return Err(None);
    };
    if !anyplay_bridge_protocol::verify(&token, Role::Bridge, &bridge_nonce, &service_nonce, &proof)
    {
        return Err(refuse("the proof does not match this game start"));
    }
    // The game may have ended, or another may have started, while the bridge answered.
    match tokens.get() {
        Some(now) if anyplay_bridge_protocol::same(&now, &token) => Ok(()),
        _ => Err(refuse("the game start changed")),
    }
}

async fn serve(
    connection: u64,
    stream: TcpStream,
    out: &UnboundedSender<BridgeInput>,
    tokens: &Tokens,
    handshake_time: Duration,
) {
    let _ = stream.set_nodelay(true);
    let (mut read, mut write) = stream.into_split();
    let mut decoder = Decoder::with_max_payload(MAX_HELLO_PAYLOAD);
    let mut buf = vec![0u8; 64 * 1024];

    let done = tokio::time::timeout(
        handshake_time,
        handshake(&mut read, &mut write, &mut decoder, &mut buf, tokens),
    )
    .await;
    match done {
        Ok(Ok(())) => {}
        Ok(Err(Some(reason))) => {
            let reject = Message::Reject(Reject { reason });
            let _ = tokio::time::timeout(Duration::from_secs(1), send(&mut write, &reject)).await;
            let _ = write.shutdown().await;
            return;
        }
        Ok(Err(None)) | Err(_) => return,
    }
    decoder.set_max_payload(MAX_PAYLOAD);

    let (frames, mut outgoing) = mpsc::unbounded_channel();
    let (verdict, answer) = oneshot::channel();
    if out
        .send(BridgeInput::Hello {
            connection,
            frames,
            verdict,
        })
        .is_err()
    {
        return;
    }
    match answer.await {
        Ok(Ok(())) => {}
        Ok(Err(reason)) => {
            let _ = send(&mut write, &Message::Reject(Reject { reason })).await;
            let _ = write.shutdown().await;
            return;
        }
        Err(_) => return,
    }

    let writer = async {
        let mut bytes = Vec::new();
        while let Some(frame) = outgoing.recv().await {
            bytes.clear();
            if frame.encode(&mut bytes).is_err() {
                continue;
            }
            while let Ok(more) = outgoing.try_recv() {
                let _ = more.encode(&mut bytes);
                if bytes.len() > 1024 * 1024 {
                    break;
                }
            }
            if write.write_all(&bytes).await.is_err() {
                return;
            }
        }
    };
    let reader = async {
        loop {
            loop {
                match decoder.next_frame() {
                    Ok(Some(frame)) => {
                        if frame.kind == anyplay_bridge_protocol::Kind::HELLO {
                            continue;
                        }
                        if out.send(BridgeInput::Frame { connection, frame }).is_err() {
                            return;
                        }
                    }
                    Ok(None) => break,
                    Err(_) => return,
                }
            }
            match read.read(&mut buf).await {
                Ok(0) | Err(_) => return,
                Ok(n) => decoder.push(&buf[..n]),
            }
        }
    };
    tokio::select! {
        _ = writer => {}
        _ = reader => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyplay_bridge_protocol::{Hello, Kind, Welcome, proof, verify};
    use tokio::sync::mpsc::UnboundedReceiver;

    const TOKEN: [u8; 32] = [0x5a; 32];

    /// A bridge on the other end of the connection, with every step of the handshake by hand.
    struct Wire {
        stream: TcpStream,
        decoder: Decoder,
        buf: Vec<u8>,
    }

    impl Wire {
        async fn connect(port: u16) -> Wire {
            Wire {
                stream: TcpStream::connect(("127.0.0.1", port)).await.unwrap(),
                decoder: Decoder::new(),
                buf: vec![0; 4096],
            }
        }

        async fn send_raw(&mut self, bytes: &[u8]) {
            self.stream.write_all(bytes).await.unwrap();
        }

        async fn send(&mut self, message: &Message) {
            let mut bytes = Vec::new();
            Frame::event(message).unwrap().encode(&mut bytes).unwrap();
            self.send_raw(&bytes).await;
        }

        async fn hello(&mut self, version: u16, nonce: Vec<u8>) {
            self.send(&Message::Hello(Hello { version, nonce })).await;
        }

        /// The next message, `None` once the net service closed the connection.
        async fn read(&mut self) -> Option<Message> {
            loop {
                if let Some(frame) = self.decoder.next_frame().unwrap() {
                    return Some(frame.message().unwrap().expect("a known kind"));
                }
                let read = self.stream.read(&mut self.buf);
                let n = tokio::time::timeout(Duration::from_secs(10), read)
                    .await
                    .expect("the net service answers or closes")
                    .unwrap_or(0);
                if n == 0 {
                    return None;
                }
                self.decoder.push(&self.buf[..n]);
            }
        }

        /// Hello with `nonce`, then the challenge the net service answers with.
        async fn challenge(&mut self, nonce: Nonce) -> Challenge {
            self.hello(VERSION, nonce.to_vec()).await;
            match self.read().await {
                Some(Message::Challenge(challenge)) => challenge,
                other => panic!("expected a challenge, got {other:?}"),
            }
        }
    }

    async fn start(tokens: Tokens) -> (u16, UnboundedReceiver<BridgeInput>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let port = listen(tx, tokens).await.unwrap();
        (port, rx)
    }

    fn reject_reason(message: Option<Message>) -> String {
        match message {
            Some(Message::Reject(reject)) => reject.reason,
            other => panic!("expected a reject, got {other:?}"),
        }
    }

    /// Nothing but the connection's end reached the service: no hello.
    async fn assert_never_introduced(rx: &mut UnboundedReceiver<BridgeInput>) {
        match rx.recv().await.unwrap() {
            BridgeInput::Closed { .. } => {}
            other => panic!("expected Closed without a hello, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_bridge_that_knows_the_token_is_let_in_and_the_service_proves_itself_first() {
        let (port, mut rx) = start(Tokens::fixed(TOKEN.to_vec())).await;
        let mut wire = Wire::connect(port).await;
        let bridge_nonce = [1u8; NONCE_LEN];
        let challenge = wire.challenge(bridge_nonce).await;
        assert_eq!(challenge.version, VERSION);
        // The service's proof is what a bridge checks before it answers.
        assert!(verify(
            &TOKEN,
            Role::Service,
            &bridge_nonce,
            &challenge.nonce,
            &challenge.proof
        ));
        assert!(!verify(
            &[0x5b; 32],
            Role::Service,
            &bridge_nonce,
            &challenge.nonce,
            &challenge.proof
        ));
        wire.send(&Message::Auth(Auth {
            proof: proof(&TOKEN, Role::Bridge, &bridge_nonce, &challenge.nonce),
        }))
        .await;

        let BridgeInput::Hello {
            frames, verdict, ..
        } = rx.recv().await.unwrap()
        else {
            panic!("the first input is the hello");
        };
        let welcome = Message::Welcome(Welcome {
            version: VERSION,
            udp_port: 1,
            tcp_port: 2,
            stream_key: [3; 16],
        });
        frames.send(Frame::event(&welcome).unwrap()).unwrap();
        verdict.send(Ok(())).unwrap();
        assert_eq!(wire.read().await, Some(welcome));
    }

    #[tokio::test]
    async fn the_token_is_never_on_the_wire() {
        let (port, _rx) = start(Tokens::fixed(TOKEN.to_vec())).await;
        let mut wire = Wire::connect(port).await;
        let challenge = wire.challenge([2; NONCE_LEN]).await;
        let mut bytes = Vec::new();
        Frame::event(&Message::Challenge(challenge))
            .unwrap()
            .encode(&mut bytes)
            .unwrap();
        assert!(!bytes.windows(8).any(|window| window == &TOKEN[..8]));
    }

    #[tokio::test]
    async fn a_fresh_nonce_gives_a_fresh_challenge() {
        let (port, _rx) = start(Tokens::fixed(TOKEN.to_vec())).await;
        let first = Wire::connect(port).await.challenge([4; NONCE_LEN]).await;
        let second = Wire::connect(port).await.challenge([4; NONCE_LEN]).await;
        assert_ne!(first.nonce, second.nonce);
        assert_ne!(first.proof, second.proof);
    }

    #[tokio::test]
    async fn a_bridge_with_another_token_is_rejected() {
        let (port, mut rx) = start(Tokens::fixed(TOKEN.to_vec())).await;
        let mut wire = Wire::connect(port).await;
        let bridge_nonce = [5u8; NONCE_LEN];
        let challenge = wire.challenge(bridge_nonce).await;
        wire.send(&Message::Auth(Auth {
            proof: proof(&[0x11; 32], Role::Bridge, &bridge_nonce, &challenge.nonce),
        }))
        .await;
        assert!(reject_reason(wire.read().await).contains("does not match"));
        assert_eq!(wire.read().await, None);
        assert_never_introduced(&mut rx).await;
    }

    #[tokio::test]
    async fn a_proof_of_the_service_is_not_accepted_as_the_bridges() {
        let (port, mut rx) = start(Tokens::fixed(TOKEN.to_vec())).await;
        let mut wire = Wire::connect(port).await;
        let challenge = wire.challenge([6; NONCE_LEN]).await;
        // Reflecting the service's own proof back.
        wire.send(&Message::Auth(Auth {
            proof: challenge.proof,
        }))
        .await;
        assert!(reject_reason(wire.read().await).contains("does not match"));
        assert_never_introduced(&mut rx).await;
    }

    #[tokio::test]
    async fn a_recorded_auth_does_not_work_on_another_connection() {
        let (port, mut rx) = start(Tokens::fixed(TOKEN.to_vec())).await;
        let bridge_nonce = [7u8; NONCE_LEN];
        let mut first = Wire::connect(port).await;
        let challenge = first.challenge(bridge_nonce).await;
        let recorded = Auth {
            proof: proof(&TOKEN, Role::Bridge, &bridge_nonce, &challenge.nonce),
        };
        // The same hello, so the same bridge nonce, but the service picks a new nonce.
        let mut second = Wire::connect(port).await;
        second.challenge(bridge_nonce).await;
        second.send(&Message::Auth(recorded)).await;
        assert!(reject_reason(second.read().await).contains("does not match"));
        // Only the first connection's end and the second's are ever reported, never a hello.
        drop(first);
        for _ in 0..2 {
            assert_never_introduced(&mut rx).await;
        }
    }

    #[tokio::test]
    async fn a_bridge_of_version_0_gets_a_clear_reject() {
        let (port, mut rx) = start(Tokens::fixed(TOKEN.to_vec())).await;
        let mut wire = Wire::connect(port).await;
        // What version 0 sent: the version, then the token itself.
        wire.hello(0, TOKEN.to_vec()).await;
        let reason = reject_reason(wire.read().await);
        assert!(reason.contains("version 0"), "{reason}");
        assert!(reason.contains("update"), "{reason}");
        assert_eq!(wire.read().await, None);
        assert_never_introduced(&mut rx).await;
    }

    #[tokio::test]
    async fn a_hello_without_a_nonce_is_rejected() {
        let (port, mut rx) = start(Tokens::fixed(TOKEN.to_vec())).await;
        let mut wire = Wire::connect(port).await;
        wire.hello(VERSION, vec![1; NONCE_LEN - 1]).await;
        assert!(reject_reason(wire.read().await).contains("nonce"));
        assert_never_introduced(&mut rx).await;
    }

    #[tokio::test]
    async fn no_bridge_is_let_in_while_no_game_is_starting() {
        let (port, mut rx) = start(Tokens::default()).await;
        let mut wire = Wire::connect(port).await;
        wire.hello(VERSION, vec![1; NONCE_LEN]).await;
        assert!(reject_reason(wire.read().await).contains("no game"));
        assert_never_introduced(&mut rx).await;
    }

    #[tokio::test]
    async fn a_game_that_ended_during_the_handshake_lets_nobody_in() {
        let tokens = Tokens::fixed(TOKEN.to_vec());
        let (port, mut rx) = start(tokens.clone()).await;
        let mut wire = Wire::connect(port).await;
        let bridge_nonce = [8u8; NONCE_LEN];
        let challenge = wire.challenge(bridge_nonce).await;
        tokens.set(Some(vec![0x77; 32]));
        wire.send(&Message::Auth(Auth {
            proof: proof(&TOKEN, Role::Bridge, &bridge_nonce, &challenge.nonce),
        }))
        .await;
        assert!(reject_reason(wire.read().await).contains("does not match"));
        assert_never_introduced(&mut rx).await;
    }

    #[tokio::test]
    async fn a_frame_that_is_not_auth_closes_the_connection() {
        let (port, mut rx) = start(Tokens::fixed(TOKEN.to_vec())).await;
        let mut wire = Wire::connect(port).await;
        wire.challenge([9; NONCE_LEN]).await;
        wire.send(&Message::Unsupported).await;
        assert_eq!(wire.read().await, None);
        assert_never_introduced(&mut rx).await;
    }

    #[tokio::test]
    async fn a_second_hello_instead_of_auth_closes_the_connection() {
        let (port, mut rx) = start(Tokens::fixed(TOKEN.to_vec())).await;
        let mut wire = Wire::connect(port).await;
        wire.challenge([9; NONCE_LEN]).await;
        wire.hello(VERSION, vec![9; NONCE_LEN]).await;
        assert_eq!(wire.read().await, None);
        assert_never_introduced(&mut rx).await;
    }

    #[tokio::test]
    async fn an_oversized_payload_before_hello_is_refused() {
        let (port, mut rx) = start(Tokens::fixed(TOKEN.to_vec())).await;
        let mut wire = Wire::connect(port).await;
        let oversized = Frame {
            kind: Kind::HELLO,
            request: 0,
            payload: vec![0u8; MAX_HELLO_PAYLOAD + 1],
        };
        let mut bytes = Vec::new();
        oversized.encode(&mut bytes).unwrap();
        wire.send_raw(&bytes).await;
        assert_eq!(wire.read().await, None);
        assert_never_introduced(&mut rx).await;
    }

    #[tokio::test]
    async fn an_oversized_declared_length_before_hello_is_refused_at_once() {
        let (port, mut rx) = start(Tokens::fixed(TOKEN.to_vec())).await;
        let mut wire = Wire::connect(port).await;
        // Only the header: the length alone is enough to refuse.
        let mut header = Vec::new();
        header.extend_from_slice(&(MAX_HELLO_PAYLOAD as u32 + 1).to_le_bytes());
        header.extend_from_slice(&Kind::HELLO.0.to_le_bytes());
        header.extend_from_slice(&0u16.to_le_bytes());
        header.extend_from_slice(&0u32.to_le_bytes());
        wire.send_raw(&header).await;
        assert_eq!(wire.read().await, None);
        assert_never_introduced(&mut rx).await;
    }

    #[tokio::test]
    async fn an_oversized_auth_is_refused_too() {
        let (port, mut rx) = start(Tokens::fixed(TOKEN.to_vec())).await;
        let mut wire = Wire::connect(port).await;
        wire.challenge([10; NONCE_LEN]).await;
        let oversized = Frame {
            kind: Kind::AUTH,
            request: 0,
            payload: vec![0u8; MAX_HELLO_PAYLOAD + 1],
        };
        let mut bytes = Vec::new();
        oversized.encode(&mut bytes).unwrap();
        wire.send_raw(&bytes).await;
        assert_eq!(wire.read().await, None);
        assert_never_introduced(&mut rx).await;
    }

    #[tokio::test]
    async fn a_hello_of_exactly_the_limit_still_gets_a_challenge() {
        let (port, _rx) = start(Tokens::fixed(TOKEN.to_vec())).await;
        let mut wire = Wire::connect(port).await;
        // The nonce first, then the extra bytes a receiver ignores: 2 + 1022 = 1024.
        wire.hello(VERSION, vec![3; MAX_HELLO_PAYLOAD - 2]).await;
        assert!(matches!(wire.read().await, Some(Message::Challenge(_))));
    }

    #[tokio::test]
    async fn a_handshake_split_over_tiny_writes_works() {
        let (port, mut rx) = start(Tokens::fixed(TOKEN.to_vec())).await;
        let mut wire = Wire::connect(port).await;
        let bridge_nonce = [11u8; NONCE_LEN];
        let mut hello = Vec::new();
        Frame::event(&Message::Hello(Hello {
            version: VERSION,
            nonce: bridge_nonce.to_vec(),
        }))
        .unwrap()
        .encode(&mut hello)
        .unwrap();
        for chunk in hello.chunks(3) {
            wire.send_raw(chunk).await;
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        let Some(Message::Challenge(challenge)) = wire.read().await else {
            panic!("expected a challenge");
        };
        let mut auth = Vec::new();
        Frame::event(&Message::Auth(Auth {
            proof: proof(&TOKEN, Role::Bridge, &bridge_nonce, &challenge.nonce),
        }))
        .unwrap()
        .encode(&mut auth)
        .unwrap();
        for chunk in auth.chunks(5) {
            wire.send_raw(chunk).await;
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        assert!(matches!(
            rx.recv().await.unwrap(),
            BridgeInput::Hello { .. }
        ));
    }

    #[tokio::test]
    async fn a_bridge_that_stalls_in_the_handshake_is_dropped() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let port = listen_within(
            tx,
            Tokens::fixed(TOKEN.to_vec()),
            Duration::from_millis(200),
        )
        .await
        .unwrap();
        let mut wire = Wire::connect(port).await;
        wire.challenge([12; NONCE_LEN]).await;
        // No auth: the connection is closed when the time is up.
        assert_eq!(wire.read().await, None);
        assert_never_introduced(&mut rx).await;
    }

    #[tokio::test]
    async fn a_bridge_that_never_says_hello_is_dropped() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let port = listen_within(
            tx,
            Tokens::fixed(TOKEN.to_vec()),
            Duration::from_millis(200),
        )
        .await
        .unwrap();
        let mut wire = Wire::connect(port).await;
        assert_eq!(wire.read().await, None);
        assert_never_introduced(&mut rx).await;
    }
}
