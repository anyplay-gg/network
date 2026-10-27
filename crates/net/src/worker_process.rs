//! The share worker as a child process, on the share link (`spec/share-play.md#the-share-link`).
//!
//! For each stream side the net service listens on a loopback port with a random token, starts
//! the worker program with `--link` and `--token`, and accepts exactly one connection that
//! proves itself with that token. Frames then move between the worker and the hub's channels. The
//! worker has no network of its own and no access to the account; when the hub drops its end,
//! the worker is told to stop and is killed if it does not leave.

use std::io::Write;
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyplay_control::ScreenSource;
use anyplay_share::control::Role;
use anyplay_share::link::{
    Capabilities, Frame, FromWorker, Hello, KIND_HELLO, KIND_WELCOME, LINK_VERSION, ToWorker,
    Welcome,
};
use tokio::sync::mpsc;

use crate::screen::{GameWindow, PlayLaunch, WorkerPort, Workers};

/// How long a worker program has to ask for its capabilities.
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);
/// How long a started worker has to connect to the link.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a worker has to leave after it was told to stop.
const LEAVE_TIMEOUT: Duration = Duration::from_secs(2);

/// Datagrams (a remote peer's video and audio) waiting for the worker. A viewer worker that falls
/// behind loses the newest ones beyond this: the stream is lossy by design and the reassembler
/// asks for a key frame, while a longer queue would only show old pictures later.
pub const MAX_QUEUED_DATAGRAMS: usize = 256;
/// Control messages (start, rate, key frame) waiting for the worker. They are a few per second;
/// a worker that has not taken this many is stuck, and it is ended rather than left to lose them.
pub const MAX_QUEUED_CONTROL: usize = 256;

#[derive(Default)]
struct Counts {
    datagrams: AtomicUsize,
    control: AtomicUsize,
    dropped: AtomicU64,
    /// The control queue overflowed: the worker is ended.
    broken: AtomicBool,
}

/// Why a message did not reach the worker.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueueError {
    /// The worker is gone (or was ended because it stopped taking control messages).
    Closed,
}

/// The service's end of the queue to one worker: never blocks, never grows without limit.
///
/// Overflow policy, per kind of message: a [`ToWorker::Datagram`] that finds
/// [`MAX_QUEUED_DATAGRAMS`] waiting is dropped and counted; any other message that finds
/// [`MAX_QUEUED_CONTROL`] waiting ends the worker (the queue closes after a stop), so a control
/// message is never silently lost; [`ToWorker::Stop`] always gets through.
#[derive(Clone)]
pub struct ToWorkerTx {
    tx: mpsc::UnboundedSender<ToWorker>,
    counts: Arc<Counts>,
}

/// The worker side of [`ToWorkerTx`].
pub struct ToWorkerRx {
    rx: mpsc::UnboundedReceiver<ToWorker>,
    counts: Arc<Counts>,
}

/// A queue to one worker.
pub fn to_worker_channel() -> (ToWorkerTx, ToWorkerRx) {
    let (tx, rx) = mpsc::unbounded_channel();
    let counts = Arc::new(Counts::default());
    (
        ToWorkerTx {
            tx,
            counts: counts.clone(),
        },
        ToWorkerRx { rx, counts },
    )
}

impl ToWorkerTx {
    pub fn send(&self, message: ToWorker) -> Result<(), QueueError> {
        let counts = &self.counts;
        if counts.broken.load(Ordering::SeqCst) {
            return Err(QueueError::Closed);
        }
        match message {
            ToWorker::Datagram(_) => {
                if counts.datagrams.fetch_add(1, Ordering::SeqCst) >= MAX_QUEUED_DATAGRAMS {
                    counts.datagrams.fetch_sub(1, Ordering::SeqCst);
                    counts.dropped.fetch_add(1, Ordering::SeqCst);
                    return Ok(());
                }
            }
            ToWorker::Stop => {}
            _ => {
                if counts.control.fetch_add(1, Ordering::SeqCst) >= MAX_QUEUED_CONTROL {
                    counts.control.fetch_sub(1, Ordering::SeqCst);
                    counts.broken.store(true, Ordering::SeqCst);
                    // Wakes the thread that feeds the worker, which then ends it.
                    let _ = self.tx.send(ToWorker::Stop);
                    return Err(QueueError::Closed);
                }
            }
        }
        self.tx.send(message).map_err(|_| QueueError::Closed)
    }

    /// Datagrams dropped because the worker was behind.
    pub fn dropped(&self) -> u64 {
        self.counts.dropped.load(Ordering::SeqCst)
    }
}

impl ToWorkerRx {
    fn taken(&self, message: Option<ToWorker>) -> Option<ToWorker> {
        let message = message?;
        match message {
            ToWorker::Datagram(_) => self.counts.datagrams.fetch_sub(1, Ordering::SeqCst),
            ToWorker::Stop => 0,
            _ => self.counts.control.fetch_sub(1, Ordering::SeqCst),
        };
        // After an overflow nothing more goes to the worker, only the end.
        if self.counts.broken.load(Ordering::SeqCst) {
            return None;
        }
        Some(message)
    }

    pub async fn recv(&mut self) -> Option<ToWorker> {
        let message = self.rx.recv().await;
        self.taken(message)
    }

    pub fn blocking_recv(&mut self) -> Option<ToWorker> {
        let message = self.rx.blocking_recv();
        self.taken(message)
    }
}

/// Workers that are the program at the path the launcher gave (`screen_worker`).
#[derive(Default)]
pub struct ProcessWorkers {
    state: Mutex<Option<(PathBuf, Capabilities)>>,
    headless: bool,
}

impl ProcessWorkers {
    pub fn new() -> ProcessWorkers {
        ProcessWorkers::default()
    }

    /// Workers that draw no window (`--headless`): for tests on a machine without a desktop.
    pub fn headless(mut self) -> ProcessWorkers {
        self.headless = true;
        self
    }
}

impl Workers for ProcessWorkers {
    fn capabilities(&self) -> Option<Capabilities> {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|(_, caps)| caps.clone())
    }

    fn set_path(&self, path: &str) -> Result<Capabilities, String> {
        let path = PathBuf::from(path);
        if !path.is_file() {
            return Err("there is no share worker at that path".into());
        }
        let capabilities = probe(&path)?;
        *self.state.lock().unwrap_or_else(|e| e.into_inner()) = Some((path, capabilities.clone()));
        Ok(capabilities)
    }

    fn launch(
        &self,
        role: Role,
        source: ScreenSource,
        title: &str,
        game: &GameWindow,
    ) -> Result<WorkerPort, String> {
        let mut args: Vec<String> = vec![
            "--source".into(),
            match source {
                ScreenSource::Game => "game",
                ScreenSource::TestPattern => "test_pattern",
            }
            .into(),
            "--title".into(),
            title.into(),
        ];
        args.extend(
            game.window
                .into_iter()
                .flat_map(|window| ["--window".to_owned(), window.to_string()]),
        );
        args.extend(
            game.pids
                .iter()
                .flat_map(|pid| ["--pid".to_owned(), pid.to_string()]),
        );
        self.spawn(role, args)
    }

    fn launch_play(&self, launch: &PlayLaunch) -> Result<WorkerPort, String> {
        let mut args: Vec<String> = vec!["--title".into(), launch.title.clone()];
        match (&launch.ring, launch.role) {
            (Some(ring), Role::Host) => args.extend([
                "--source".into(),
                "ring".into(),
                "--ring".into(),
                ring.name.clone(),
                "--width".into(),
                ring.width.to_string(),
                "--height".into(),
                ring.height.to_string(),
                "--fps".into(),
                ring.fps.to_string(),
            ]),
            (None, Role::Guest) => args.push("--pad".into()),
            _ => return Err("a host's worker needs a ring and a guest's has none".into()),
        }
        self.spawn(launch.role, args)
    }
}

impl ProcessWorkers {
    /// Starts the worker program for one side with these arguments after the role, and the
    /// share link it connects back to.
    fn spawn(&self, role: Role, args: Vec<String>) -> Result<WorkerPort, String> {
        let path = self
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|(path, _)| path.clone())
            .ok_or("there is no share worker")?;
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .map_err(|e| format!("the share link could not listen: {e}"))?;
        let address = listener.local_addr().map_err(|e| e.to_string())?;
        let token = data_encoding::HEXLOWER.encode(&rand::random::<[u8; 16]>());
        let mut command = Command::new(&path);
        command
            .args([
                "--role",
                match role {
                    Role::Host => "host",
                    Role::Guest => "guest",
                },
            ])
            .args(&args)
            .args(self.headless.then_some("--headless"))
            .env("ANYPLAY_SHARE_ENDPOINT", address.to_string())
            .env("ANYPLAY_SHARE_TOKEN", &token)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit());
        no_window(&mut command);
        let mut child = command
            .spawn()
            .map_err(|e| format!("the share worker could not start: {e}"))?;

        let (to_tx, mut to_rx) = to_worker_channel();
        let (from_tx, from_rx) = mpsc::channel::<FromWorker>(1024);
        thread::spawn(move || {
            let stream = match accept(&listener, &token, role) {
                Ok(stream) => stream,
                Err(error) => {
                    let _ = from_tx.blocking_send(FromWorker::Ended(error));
                    kill(&mut child);
                    return;
                }
            };
            let Ok(mut writer) = stream.try_clone() else {
                kill(&mut child);
                return;
            };
            let reader_tx = from_tx.clone();
            let mut reader = stream;
            let reading = thread::spawn(move || {
                let mut ended = "the share worker closed the link".to_owned();
                loop {
                    match Frame::read_from(&mut reader) {
                        Ok(Some(frame)) => {
                            if let Some(message) = FromWorker::from_frame(&frame)
                                && reader_tx.blocking_send(message).is_err()
                            {
                                return;
                            }
                        }
                        Ok(None) => break,
                        Err(error) => {
                            ended = format!("the share link broke: {error}");
                            break;
                        }
                    }
                }
                let _ = reader_tx.blocking_send(FromWorker::Ended(ended));
            });
            while let Some(message) = to_rx.blocking_recv() {
                let stop = message == ToWorker::Stop;
                if message.to_frame().write_to(&mut writer).is_err() || stop {
                    break;
                }
            }
            // The hub is done with this worker: tell it, give it a moment, then make sure.
            let _ = ToWorker::Stop.to_frame().write_to(&mut writer);
            let _ = writer.flush();
            let _ = writer.shutdown(std::net::Shutdown::Write);
            let deadline = Instant::now() + LEAVE_TIMEOUT;
            while Instant::now() < deadline {
                if matches!(child.try_wait(), Ok(Some(_))) {
                    break;
                }
                thread::sleep(Duration::from_millis(20));
            }
            kill(&mut child);
            let _ = reading.join();
        });
        Ok(WorkerPort {
            to: to_tx,
            from: from_rx,
        })
    }
}

#[cfg(windows)]
fn no_window(command: &mut Command) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    command.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(windows))]
fn no_window(_: &mut Command) {}

fn kill(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

/// Runs the worker with `--capabilities` and reads what it prints.
fn probe(path: &Path) -> Result<Capabilities, String> {
    let mut command = Command::new(path);
    command
        .arg("--capabilities")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    no_window(&mut command);
    let mut child = command
        .spawn()
        .map_err(|e| format!("the share worker could not start: {e}"))?;
    let deadline = Instant::now() + PROBE_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
            _ => {
                kill(&mut child);
                return Err("the share worker did not answer".into());
            }
        }
    }
    let output = child
        .wait_with_output()
        .map_err(|e| format!("the share worker did not answer: {e}"))?;
    serde_json::from_slice(&output.stdout)
        .map_err(|_| "that is not a share worker of this version".to_owned())
}

/// Accepts the one connection of the worker that was started and checks its hello.
fn accept(listener: &TcpListener, token: &str, role: Role) -> Result<TcpStream, String> {
    listener.set_nonblocking(true).map_err(|e| e.to_string())?;
    let deadline = Instant::now() + CONNECT_TIMEOUT;
    let mut stream = loop {
        match listener.accept() {
            Ok((stream, _)) => break stream,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock && Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(10));
            }
            Err(_) => return Err("the share worker never connected".into()),
        }
    };
    stream.set_nonblocking(false).map_err(|e| e.to_string())?;
    stream.set_nodelay(true).map_err(|e| e.to_string())?;
    stream
        .set_read_timeout(Some(CONNECT_TIMEOUT))
        .map_err(|e| e.to_string())?;
    let hello = Frame::read_from(&mut stream)
        .ok()
        .flatten()
        .filter(|f| f.kind == KIND_HELLO)
        .and_then(|f| serde_json::from_slice::<Hello>(&f.payload).ok())
        .ok_or("the share worker did not say hello")?;
    if hello.token != token || hello.version != LINK_VERSION || hello.role != role {
        return Err("the share worker did not prove itself".into());
    }
    Frame::json(
        KIND_WELCOME,
        &Welcome {
            version: LINK_VERSION,
        },
    )
    .write_to(&mut stream)
    .map_err(|e| e.to_string())?;
    stream.set_read_timeout(None).map_err(|e| e.to_string())?;
    Ok(stream)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_worker_that_stops_reading_cannot_make_the_datagram_queue_grow() {
        let (tx, mut rx) = to_worker_channel();
        for n in 0..100_000u32 {
            tx.send(ToWorker::Datagram(n.to_be_bytes().to_vec()))
                .unwrap();
        }
        assert_eq!(tx.dropped(), (100_000 - MAX_QUEUED_DATAGRAMS) as u64);
        let mut kept = 0;
        while kept < MAX_QUEUED_DATAGRAMS {
            assert!(matches!(rx.blocking_recv(), Some(ToWorker::Datagram(_))));
            kept += 1;
        }
        // Room again once the worker has caught up.
        tx.send(ToWorker::Datagram(vec![1])).unwrap();
        assert_eq!(tx.dropped(), (100_000 - MAX_QUEUED_DATAGRAMS) as u64);
    }

    #[test]
    fn datagrams_never_push_a_control_message_out() {
        let (tx, mut rx) = to_worker_channel();
        for _ in 0..MAX_QUEUED_DATAGRAMS * 2 {
            tx.send(ToWorker::Datagram(vec![0])).unwrap();
        }
        tx.send(ToWorker::KeyFrame).unwrap();
        tx.send(ToWorker::Stop).unwrap();
        let mut seen = Vec::new();
        while let Some(m) = rx.blocking_recv() {
            let done = m == ToWorker::Stop;
            seen.push(m);
            if done {
                break;
            }
        }
        assert!(seen.contains(&ToWorker::KeyFrame));
        assert_eq!(seen.last(), Some(&ToWorker::Stop));
    }

    #[test]
    fn a_worker_that_takes_no_control_message_is_ended_not_left_to_lose_them() {
        let (tx, mut rx) = to_worker_channel();
        for _ in 0..MAX_QUEUED_CONTROL {
            tx.send(ToWorker::KeyFrame).unwrap();
        }
        assert_eq!(tx.send(ToWorker::KeyFrame), Err(QueueError::Closed));
        // From then on the queue is closed to everything but the end.
        assert_eq!(
            tx.send(ToWorker::Datagram(vec![1])),
            Err(QueueError::Closed)
        );
        assert_eq!(rx.blocking_recv(), None);
    }

    #[test]
    fn stop_gets_through_a_full_control_queue() {
        let (tx, mut rx) = to_worker_channel();
        for _ in 0..MAX_QUEUED_CONTROL {
            tx.send(ToWorker::KeyFrame).unwrap();
        }
        tx.send(ToWorker::Stop).unwrap();
        let mut keys = 0;
        loop {
            match rx.blocking_recv() {
                Some(ToWorker::KeyFrame) => keys += 1,
                Some(ToWorker::Stop) => break,
                other => panic!("unexpected {other:?}"),
            }
        }
        assert_eq!(keys, MAX_QUEUED_CONTROL);
    }
}
