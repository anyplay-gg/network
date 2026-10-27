//! The loopback listener that games' bridges connect to.
//!
//! Each connection is read and written by its own task; the service decides on the hello and
//! gets every later frame. Frames are size-checked before they are buffered.

use std::net::Ipv4Addr;
use std::time::Duration;

use anyplay_bridge_protocol::{Decoder, Frame, Kind, Message, Reject};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc::{self, UnboundedSender};
use tokio::sync::oneshot;

/// The largest payload accepted for the hello frame. A hello carries a 2-byte version and a
/// token (a 32-byte secret), so 1024 bytes is ample and stops an unauthenticated peer buffering
/// up to [`anyplay_bridge_protocol::MAX_PAYLOAD`].
pub const MAX_HELLO_PAYLOAD: usize = 1024;

/// What the bridge listener hands to the service.
#[derive(Debug)]
pub enum BridgeInput {
    /// A bridge introduced itself. The service answers through `verdict`.
    Hello {
        connection: u64,
        version: u16,
        token: Vec<u8>,
        frames: UnboundedSender<Frame>,
        verdict: oneshot::Sender<Result<(), String>>,
    },
    /// A frame after the welcome.
    Frame { connection: u64, frame: Frame },
    /// The connection ended.
    Closed { connection: u64 },
}

/// Listens on a loopback port until the process ends.
pub async fn listen(out: UnboundedSender<BridgeInput>) -> std::io::Result<u16> {
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
            tokio::spawn(async move {
                serve(connection, stream, &out).await;
                let _ = out.send(BridgeInput::Closed { connection });
            });
        }
    });
    Ok(port)
}

async fn serve(connection: u64, stream: TcpStream, out: &UnboundedSender<BridgeInput>) {
    let _ = stream.set_nodelay(true);
    let (mut read, mut write) = stream.into_split();
    let mut decoder = Decoder::new();
    let mut buf = vec![0u8; 64 * 1024];

    let hello = tokio::time::timeout(Duration::from_secs(5), async {
        let mut total = 0usize;
        let mut header_buf = [0u8; 4];
        let mut header_read = 0usize;
        loop {
            match decoder.next_frame() {
                Ok(Some(frame)) => {
                    if frame.payload.len() > MAX_HELLO_PAYLOAD {
                        return None;
                    }
                    return Some(frame);
                }
                Ok(None) => {}
                Err(_) => return None,
            }
            match read.read(&mut buf).await {
                Ok(0) | Err(_) => return None,
                Ok(n) => {
                    total = total.saturating_add(n);
                    if total > 12 + MAX_HELLO_PAYLOAD {
                        return None;
                    }
                    if header_read < 4 {
                        let take = (4 - header_read).min(n);
                        header_buf[header_read..header_read + take].copy_from_slice(&buf[..take]);
                        header_read += take;
                        if header_read == 4 {
                            let announced = u32::from_le_bytes(header_buf) as usize;
                            if announced > MAX_HELLO_PAYLOAD {
                                return None;
                            }
                        }
                    }
                    decoder.push(&buf[..n]);
                }
            }
        }
    })
    .await;
    let Ok(Some(hello)) = hello else {
        return;
    };
    let Ok(Some(Message::Hello(hello))) = Message::parse(hello.kind, &hello.payload) else {
        return;
    };

    let (frames, mut outgoing) = mpsc::unbounded_channel();
    let (verdict, answer) = oneshot::channel();
    if out
        .send(BridgeInput::Hello {
            connection,
            version: hello.version,
            token: hello.token,
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
            if let Ok(frame) = Frame::event(&Message::Reject(Reject { reason })) {
                let mut bytes = Vec::new();
                if frame.encode(&mut bytes).is_ok() {
                    let _ = write.write_all(&bytes).await;
                }
            }
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
                        if frame.kind == Kind::HELLO {
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
    use anyplay_bridge_protocol::{Frame, Hello, Message, VERSION};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;

    #[tokio::test]
    async fn a_hello_within_the_limit_is_accepted() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let port = listen(tx).await.unwrap();
        let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();

        let hello = Frame::event(&Message::Hello(Hello {
            version: VERSION,
            token: vec![1, 2, 3, 4],
        }))
        .unwrap();
        let mut bytes = Vec::new();
        hello.encode(&mut bytes).unwrap();
        stream.write_all(&bytes).await.unwrap();

        match rx.recv().await.unwrap() {
            BridgeInput::Hello {
                version,
                token,
                verdict,
                ..
            } => {
                assert_eq!(version, VERSION);
                assert_eq!(token, vec![1, 2, 3, 4]);
                let _ = verdict.send(Ok(()));
            }
            other => panic!("expected Hello, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn an_oversized_payload_before_hello_is_refused() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let port = listen(tx).await.unwrap();
        let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();

        let oversized = Frame {
            kind: Kind::HELLO,
            request: 0,
            payload: vec![0u8; MAX_HELLO_PAYLOAD + 1],
        };
        let mut bytes = Vec::new();
        oversized.encode(&mut bytes).unwrap();
        stream.write_all(&bytes).await.unwrap();

        let mut answer = [0u8; 1];
        let read = stream.read(&mut answer).await.unwrap();
        assert_eq!(read, 0);

        match rx.recv().await.unwrap() {
            BridgeInput::Closed { .. } => {}
            other => panic!("expected Closed without Hello, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn an_oversized_declared_length_before_hello_is_refused_at_once() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let port = listen(tx).await.unwrap();
        let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();

        let declared_len = (MAX_HELLO_PAYLOAD as u32) + 1;
        let mut header = Vec::new();
        header.extend_from_slice(&declared_len.to_le_bytes());
        header.extend_from_slice(&Kind::HELLO.0.to_le_bytes());
        header.extend_from_slice(&0u16.to_le_bytes());
        header.extend_from_slice(&0u32.to_le_bytes());
        stream.write_all(&header).await.unwrap();

        let mut answer = [0u8; 1];
        let read = stream.read(&mut answer).await.unwrap();
        assert_eq!(read, 0);

        match rx.recv().await.unwrap() {
            BridgeInput::Closed { .. } => {}
            other => panic!("expected Closed without Hello, got {other:?}"),
        }
    }
}
