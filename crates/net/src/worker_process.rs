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
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};

use anyplay_control::ScreenSource;
use anyplay_share::control::Role;
use anyplay_share::link::{
    Capabilities, Frame, FromWorker, Hello, KIND_HELLO, KIND_WELCOME, LINK_VERSION, ToWorker,
    Welcome,
};
use tokio::sync::mpsc;

use crate::screen::{GameWindow, WorkerPort, Workers};

/// How long a worker program has to ask for its capabilities.
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);
/// How long a started worker has to connect to the link.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a worker has to leave after it was told to stop.
const LEAVE_TIMEOUT: Duration = Duration::from_secs(2);

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
                "--source",
                match source {
                    ScreenSource::Game => "game",
                    ScreenSource::TestPattern => "test_pattern",
                },
                "--title",
                title,
            ])
            .args(
                game.window
                    .into_iter()
                    .flat_map(|window| ["--window".to_owned(), window.to_string()]),
            )
            .args(
                game.pids
                    .iter()
                    .flat_map(|pid| ["--pid".to_owned(), pid.to_string()]),
            )
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

        let (to_tx, mut to_rx) = mpsc::unbounded_channel::<ToWorker>();
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
