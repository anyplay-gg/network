//! The worker's end of the share link on a loopback socket.

use std::io::{self, BufReader};
use std::net::TcpStream;
use std::sync::mpsc;
use std::thread;

use anyplay_share::control::Role;
use anyplay_share::link::{
    Frame, FromWorker, Hello, KIND_HELLO, KIND_WELCOME, LINK_VERSION, ToWorker, Welcome,
};

use crate::Port;

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
    let (to_worker, from_service) = mpsc::channel();
    let (to_service, from_worker) = mpsc::channel::<FromWorker>();
    thread::spawn(move || {
        while let Ok(Some(frame)) = Frame::read_from(&mut reader) {
            if let Some(message) = ToWorker::from_frame(&frame)
                && to_worker.send(message).is_err()
            {
                return;
            }
        }
        // The service hung up or sent nonsense: the worker ends.
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
