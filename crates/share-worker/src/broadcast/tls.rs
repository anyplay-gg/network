//! TLS for `rtmps://`: rustls (ring, no OpenSSL), the system's own certificate checks, and a
//! link that can be read on one thread while another writes.
//!
//! A [`rustls::StreamOwned`] cannot be shared by a reader and a writer. The publisher needs both
//! (the reader answers pings while the sender writes pictures), so here the connection state sits
//! in one mutex and the socket is read *outside* it: the reader waits for bytes without holding the
//! lock, then takes it only to feed them to rustls and to send what rustls wants sent back.

use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::sync::{Arc, Mutex, MutexGuard};

use rustls::pki_types::ServerName;
use rustls::{CertificateError, ClientConfig, ClientConnection, Error as TlsError};

/// The client settings for real services: rustls with the ring provider, and the certificate
/// checks of the operating system (its trust store, its revocation and date rules).
pub fn system_config() -> Result<Arc<ClientConfig>, String> {
    use rustls_platform_verifier::BuilderVerifierExt;
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| format!("the secure connection could not be set up: {e}"))?
        .with_platform_verifier()
        .map_err(|e| format!("the system's certificates could not be read: {e}"))?
        .with_no_client_auth();
    Ok(Arc::new(config))
}

struct Inner {
    conn: ClientConnection,
    out: TcpStream,
}

impl Inner {
    /// Sends what rustls has queued (data, alerts, key updates).
    fn flush_tls(&mut self) -> io::Result<()> {
        while self.conn.wants_write() {
            self.conn.write_tls(&mut self.out)?;
        }
        self.out.flush()
    }
}

type Shared = Arc<Mutex<Inner>>;

fn lock(shared: &Shared) -> io::Result<MutexGuard<'_, Inner>> {
    shared
        .lock()
        .map_err(|_| io::Error::other("the secure connection broke"))
}

/// The writing half.
pub struct TlsWriter {
    shared: Shared,
}

/// The reading half.
pub struct TlsReader {
    shared: Shared,
    input: TcpStream,
    plain: VecDeque<u8>,
    ended: bool,
}

/// Closes the secure session the polite way (a `close_notify`), from any thread.
#[derive(Clone)]
pub struct TlsCloser {
    shared: Shared,
}

impl TlsCloser {
    pub fn close(&self) {
        if let Ok(mut inner) = self.shared.lock() {
            inner.conn.send_close_notify();
            let _ = inner.flush_tls();
        }
    }
}

/// Opens the secure session to `host` over `tcp` (handshake included) and splits it.
pub fn connect(
    config: Arc<ClientConfig>,
    host: &str,
    mut tcp: TcpStream,
) -> io::Result<(TlsReader, TlsWriter, TlsCloser)> {
    let name = ServerName::try_from(host.to_owned())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "the server name is not valid"))?;
    let mut conn = ClientConnection::new(config, name)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    while conn.is_handshaking() {
        conn.complete_io(&mut tcp)?;
    }
    let input = tcp.try_clone()?;
    let shared = Arc::new(Mutex::new(Inner { conn, out: tcp }));
    Ok((
        TlsReader {
            shared: shared.clone(),
            input,
            plain: VecDeque::new(),
            ended: false,
        },
        TlsWriter {
            shared: shared.clone(),
        },
        TlsCloser { shared },
    ))
}

impl Read for TlsReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        loop {
            if !self.plain.is_empty() {
                let n = buf.len().min(self.plain.len());
                for (slot, byte) in buf.iter_mut().zip(self.plain.drain(..n)) {
                    *slot = byte;
                }
                return Ok(n);
            }
            if self.ended {
                return Ok(0);
            }
            let mut raw = [0u8; 16 * 1024];
            // Waits here, without the lock: a writer is never held up by a quiet server.
            let got = self.input.read(&mut raw)?;
            if got == 0 {
                self.ended = true;
                continue;
            }
            let mut inner = lock(&self.shared)?;
            let mut rest = &raw[..got];
            let mut failure = None;
            while !rest.is_empty() {
                let used = inner.conn.read_tls(&mut rest)?;
                if let Err(error) = inner.conn.process_new_packets() {
                    failure = Some(error);
                    break;
                }
                if used == 0 {
                    break;
                }
            }
            let mut chunk = [0u8; 8 * 1024];
            if failure.is_none() {
                loop {
                    match inner.conn.reader().read(&mut chunk) {
                        Ok(0) => {
                            // The server said goodbye (close_notify).
                            self.ended = true;
                            break;
                        }
                        Ok(n) => self.plain.extend(&chunk[..n]),
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => {
                            self.ended = true;
                            break;
                        }
                        Err(e) => return Err(e),
                    }
                }
            }
            // An alert for a failure goes out before the failure is reported.
            let _ = inner.flush_tls();
            if let Some(error) = failure {
                return Err(io::Error::new(io::ErrorKind::InvalidData, error));
            }
        }
    }
}

impl Write for TlsWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut inner = lock(&self.shared)?;
        inner.conn.writer().write_all(buf)?;
        inner.flush_tls()?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        lock(&self.shared)?.flush_tls()
    }
}

/// What a failed secure connection means, in words a player can act on. The tag says what
/// kind of failure it was (the launcher chooses its message by it): `tls_untrusted` (the
/// certificate is not one this PC trusts, or is for another server), `tls_expired` (the
/// certificate is not valid at this PC's date and time) and `tls_failed` (anything else).
pub fn explain(error: &io::Error) -> Option<(&'static str, String)> {
    let tls = error.get_ref()?.downcast_ref::<TlsError>()?;
    Some(match tls {
        TlsError::InvalidCertificate(problem) => match problem {
            CertificateError::Expired
            | CertificateError::ExpiredContext { .. }
            | CertificateError::NotValidYet
            | CertificateError::NotValidYetContext { .. } => (
                "tls_expired",
                "the server's certificate is not valid at this PC's date and time".to_owned(),
            ),
            CertificateError::NotValidForName | CertificateError::NotValidForNameContext { .. } => {
                (
                    "tls_untrusted",
                    "the server's certificate is for a different server name".to_owned(),
                )
            }
            CertificateError::UnknownIssuer => (
                "tls_untrusted",
                "this PC does not trust the server's certificate".to_owned(),
            ),
            CertificateError::Revoked => (
                "tls_untrusted",
                "the server's certificate has been revoked".to_owned(),
            ),
            _ => (
                "tls_untrusted",
                "the server's certificate was not accepted".to_owned(),
            ),
        },
        TlsError::AlertReceived(_) => (
            "tls_failed",
            "the server refused the secure connection".to_owned(),
        ),
        TlsError::NoCertificatesPresented | TlsError::PeerIncompatible(_) => (
            "tls_failed",
            "the server and this PC have no secure settings in common".to_owned(),
        ),
        _ => (
            "tls_failed",
            "the secure connection to the server failed".to_owned(),
        ),
    })
}
