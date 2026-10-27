//! The worker's end of the share link on a loopback socket.

use std::io::{self, BufReader};
use std::net::TcpStream;
use std::sync::mpsc::{self, TrySendError};
use std::thread;

use anyplay_share::control::Role;
use anyplay_share::link::{
    Frame, FromWorker, Hello, KIND_HELLO, KIND_WELCOME, LINK_VERSION, ToWorker, Welcome,
};

use crate::Port;

/// How many messages from the service wait for the worker's main loop. The service feeds a
/// viewer's worker the video and audio datagrams of a remote peer, so this is the bound on what a
/// stalled worker can pile up: a datagram that finds the queue full is dropped (the stream is
/// lossy by design, the reassembler asks for a key frame), a control message waits for room
/// instead (the reading thread blocks, the link backs up and the service's own bound on its
/// side takes over). Nothing but a datagram is ever dropped.
pub const QUEUE: usize = 512;

/// Hands a message from the link to the worker: see [`QUEUE`]. `false` when the worker is gone.
fn deliver(queue: &mpsc::SyncSender<ToWorker>, message: ToWorker) -> bool {
    if matches!(message, ToWorker::Datagram(_)) {
        return !matches!(queue.try_send(message), Err(TrySendError::Disconnected(_)));
    }
    queue.send(message).is_ok()
}

/// Connects to the net service's listener at `addr`, proves itself with `token` and returns the
/// port the worker runs on. Two threads move the frames: one reads the service's messages, one
/// writes the worker's. When the service hangs up, the worker's receiver reports it by
/// disconnecting after a [`ToWorker::Stop`].
pub fn connect(addr: &str, token: &str, role: Role) -> io::Result<Port> {
    let stream = TcpStream::connect(addr)?;
    stream.set_nodelay(true)?;
    let mut writer = stream.try_clone()?;
    let mut reader = BufReader::new(stream);
    Frame::json(
        KIND_HELLO,
        &Hello {
            version: LINK_VERSION,
            token: token.to_owned(),
            role,
        },
    )
    .write_to(&mut writer)?;
    let welcome = Frame::read_from(&mut reader)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?
        .filter(|f| f.kind == KIND_WELCOME)
        .and_then(|f| serde_json::from_slice::<Welcome>(&f.payload).ok())
        .ok_or_else(|| io::Error::new(io::ErrorKind::ConnectionRefused, "no welcome"))?;
    if welcome.version != LINK_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "the net service speaks another version of the share link",
        ));
    }
    let (to_worker, from_service) = mpsc::sync_channel(QUEUE);
    let (to_service, from_worker) = mpsc::channel::<FromWorker>();
    thread::spawn(move || {
        while let Ok(Some(frame)) = Frame::read_from(&mut reader) {
            if let Some(message) = ToWorker::from_frame(&frame)
                && !deliver(&to_worker, message)
            {
                return;
            }
        }
        // The service hung up or sent nonsense: the worker ends.
        // A full queue must not keep the worker from ending: the stop waits for room.
        let _ = to_worker.send(ToWorker::Stop);
    });
    thread::spawn(move || {
        // Each frame is one write: a datagram or a small message, never batched.
        while let Ok(message) = from_worker.recv() {
            if message.to_frame().write_to(&mut writer).is_err() {
                return;
            }
        }
    });
    Ok(Port {
        from_service,
        to_service,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stalled_worker_does_not_pile_up_datagrams() {
        let (tx, rx) = mpsc::sync_channel(4);
        for n in 0..10_000u32 {
            // Never blocks, never fails while the worker is alive.
            assert!(deliver(&tx, ToWorker::Datagram(n.to_be_bytes().to_vec())));
        }
        assert_eq!(rx.try_iter().count(), 4, "only the queue's worth is kept");
        // A control message is not dropped when there is room, and arrives in order.
        assert!(deliver(&tx, ToWorker::KeyFrame));
        assert!(deliver(&tx, ToWorker::Stop));
        assert_eq!(rx.try_recv().unwrap(), ToWorker::KeyFrame);
        assert_eq!(rx.try_recv().unwrap(), ToWorker::Stop);
        drop(rx);
        assert!(!deliver(&tx, ToWorker::Datagram(vec![1])));
    }
}
