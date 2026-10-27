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
        loop {
            match decoder.next_frame() {
                Ok(Some(frame)) => return Some(frame),
                Ok(None) => {}
                Err(_) => return None,
            }
            match read.read(&mut buf).await {
                Ok(0) | Err(_) => return None,
                Ok(n) => decoder.push(&buf[..n]),
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
