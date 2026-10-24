//! The event connection to the cloud: kept open while the player is signed in, reopened with
//! growing pauses when it breaks.

use std::time::Duration;

use anyplay_api::{ErrorCode, Event, close};
use futures_util::StreamExt;
use tokio::sync::mpsc::UnboundedSender;
use tokio::task::JoinHandle;
use tokio_websockets::{ClientBuilder, Limits};

use crate::cloud::{Cloud, CloudError};

/// What the event connection reports.
#[derive(Debug)]
pub enum EventInput {
    /// The connection is being opened.
    Connecting,
    /// The cloud took the connection: what changes from now on arrives on it, so lists fetched
    /// now miss nothing.
    Online,
    /// The connection broke; it is opened again after a pause.
    Offline(String),
    /// A message from the cloud.
    Event(Event),
    /// The cloud signed the device out. The connection ends for good.
    SignedOut,
    /// The account is banned. The connection ends for good.
    Banned(Option<u64>),
}

/// Quiet time after which a connection counts as dead. The cloud pings every 20 seconds.
const SILENCE: Duration = Duration::from_secs(50);
const LONGEST_PAUSE: Duration = Duration::from_secs(30);
const LARGEST_MESSAGE: usize = 1024 * 1024;

/// Keeps the event connection of `cloud` until the returned task is aborted or the cloud ends
/// it.
pub fn spawn(cloud: Cloud, out: UnboundedSender<EventInput>) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut pause = Duration::from_secs(1);
        let mut refreshed = false;
        loop {
            let _ = out.send(EventInput::Connecting);
            match session(&cloud, &out).await {
                Ending::SignedOut => {
                    let _ = out.send(EventInput::SignedOut);
                    return;
                }
                Ending::Banned(until) => {
                    let _ = out.send(EventInput::Banned(until));
                    return;
                }
                Ending::Refresh => {
                    if refreshed {
                        tokio::time::sleep(jitter(pause)).await;
                        pause = (pause * 2).min(LONGEST_PAUSE);
                    }
                    refreshed = true;
                    if let Err(error) = cloud.refresh().await {
                        if let Some(ending) = final_ending(&error) {
                            let _ = out.send(ending);
                            return;
                        }
                        let _ = out.send(EventInput::Offline(error.to_string()));
                        tokio::time::sleep(pause).await;
                        pause = (pause * 2).min(LONGEST_PAUSE);
                    }
                }
                Ending::Broken { reason, was_online } => {
                    refreshed = false;
                    let _ = out.send(EventInput::Offline(reason));
                    if was_online {
                        pause = Duration::from_secs(1);
                    }
                    tokio::time::sleep(jitter(pause)).await;
                    pause = (pause * 2).min(LONGEST_PAUSE);
                }
                Ending::GoingAway(reason) => {
                    refreshed = false;
                    let _ = out.send(EventInput::Offline(reason));
                    pause = Duration::from_secs(1);
                    tokio::time::sleep(spread()).await;
                }
            }
        }
    })
}

fn final_ending(error: &CloudError) -> Option<EventInput> {
    match error {
        CloudError::SignedOut => Some(EventInput::SignedOut),
        CloudError::Api(api) if api.error == ErrorCode::Banned => {
            Some(EventInput::Banned(api.until))
        }
        _ => None,
    }
}

enum Ending {
    SignedOut,
    Banned(Option<u64>),
    Refresh,
    Broken {
        reason: String,
        was_online: bool,
    },
    /// The cloud took its node down and asked its clients to come back spread out.
    GoingAway(String),
}

async fn session(cloud: &Cloud, out: &UnboundedSender<EventInput>) -> Ending {
    let access = match cloud.access().await {
        Ok(access) => access,
        Err(error) => {
            return match final_ending(&error) {
                Some(EventInput::Banned(until)) => Ending::Banned(until),
                Some(_) => Ending::SignedOut,
                None => Ending::Broken {
                    reason: error.to_string(),
                    was_online: false,
                },
            };
        }
    };
    let broken = |reason: String| Ending::Broken {
        reason,
        was_online: false,
    };
    let Ok(uri) = cloud.events_url().parse() else {
        return broken("the cloud address is not a URL".into());
    };
    let Ok(value) = format!("Bearer {access}").parse() else {
        return broken("the access token is not a header value".into());
    };
    let builder = match ClientBuilder::from_uri(uri)
        .limits(Limits::default().max_payload_len(Some(LARGEST_MESSAGE)))
        .add_header(http_authorization(), value)
    {
        Ok(builder) => builder,
        Err(error) => return broken(error.to_string()),
    };
    let mut stream = match tokio::time::timeout(Duration::from_secs(15), builder.connect()).await {
        Ok(Ok((stream, _))) => stream,
        Ok(Err(tokio_websockets::Error::Upgrade(
            tokio_websockets::upgrade::Error::DidNotSwitchProtocols(401),
        ))) => return Ending::Refresh,
        Ok(Err(tokio_websockets::Error::Upgrade(
            tokio_websockets::upgrade::Error::DidNotSwitchProtocols(403),
        ))) => {
            // A ban or a revoked device: the refresh tells which, and ends the connection.
            return Ending::Refresh;
        }
        Ok(Err(error)) => return broken(error.to_string()),
        Err(_) => return broken("the cloud did not answer in time".into()),
    };
    // The cloud sends `welcome` first, once it pushes to this connection. Until then a change
    // could reach neither the connection nor lists fetched on going online.
    let mut welcomed = false;
    let mut banned_until = None;
    loop {
        let next = tokio::time::timeout(SILENCE, stream.next()).await;
        let message = match next {
            Ok(Some(Ok(message))) => message,
            Ok(Some(Err(error))) => {
                return Ending::Broken {
                    reason: error.to_string(),
                    was_online: true,
                };
            }
            Ok(None) => {
                return Ending::Broken {
                    reason: "the cloud closed the connection".into(),
                    was_online: true,
                };
            }
            Err(_) => {
                return Ending::Broken {
                    reason: "the cloud went quiet".into(),
                    was_online: true,
                };
            }
        };
        if let Some((code, _)) = message.as_close() {
            return match u16::from(code) {
                close::TOKEN_EXPIRED => Ending::Refresh,
                close::BANNED => Ending::Banned(banned_until),
                close::SIGNED_OUT => Ending::SignedOut,
                close::GOING_AWAY | close::RESTARTING => {
                    Ending::GoingAway("the cloud is restarting".into())
                }
                other => Ending::Broken {
                    reason: format!("closed with code {other}"),
                    was_online: true,
                },
            };
        }
        let Some(text) = message.as_text() else {
            continue;
        };
        match serde_json::from_str::<Event>(text) {
            Ok(Event::Banned { until }) => banned_until = until,
            Ok(Event::SignedOut) => return Ending::SignedOut,
            Ok(Event::Unknown) => {}
            Ok(event) => {
                if !welcomed && matches!(event, Event::Welcome { .. }) {
                    welcomed = true;
                    let _ = out.send(EventInput::Online);
                }
                let _ = out.send(EventInput::Event(event));
            }
            Err(error) => tracing::warn!("unreadable event: {error}"),
        }
    }
}

fn http_authorization() -> http::HeaderName {
    http::header::AUTHORIZATION
}

/// Half the pause plus a random part of up to the whole, so that net services that lost the
/// cloud together come back spread over a whole pause instead of a quarter of one.
fn jitter(pause: Duration) -> Duration {
    let millis = pause.as_millis() as u64;
    Duration::from_millis(millis / 2 + rand::random_range(0..=millis))
}

/// A random moment within [`close::RECONNECT_SPREAD_SECS`], after the cloud asked to come back
/// later.
fn spread() -> Duration {
    Duration::from_millis(rand::random_range(0..=close::RECONNECT_SPREAD_SECS * 1000))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use anyplay_api::{Tokens, path};
    use axum::extract::ws::{Message, WebSocketUpgrade};
    use axum::routing::{get, post};
    use tokio::sync::{Mutex, mpsc, oneshot};

    use super::*;
    use crate::cloud::now;

    /// A cloud that hands out a token and opens the event connection, but registers it, and
    /// sends `welcome`, only when `register` fires.
    async fn cloud_that_waits(register: oneshot::Receiver<()>) -> Cloud {
        crate::install_crypto();
        let register = Arc::new(Mutex::new(Some(register)));
        let app = axum::Router::new()
            .route(
                path::AUTH_REFRESH,
                post(|| async {
                    axum::Json(Tokens {
                        access: "access".into(),
                        access_expires: now() + 3600,
                        refresh: "refresh".into(),
                        device: 1,
                    })
                }),
            )
            .route(
                path::EVENTS,
                get(move |upgrade: WebSocketUpgrade| async move {
                    upgrade.on_upgrade(move |mut socket| async move {
                        if let Some(register) = register.lock().await.take() {
                            let _ = register.await;
                        }
                        let welcome = Event::Welcome {
                            address: "10.64.0.2".into(),
                            server_time: now(),
                        };
                        let text = serde_json::to_string(&welcome).unwrap();
                        let _ = socket.send(Message::Text(text.into())).await;
                        std::future::pending::<()>().await;
                    })
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await });
        let cloud = Cloud::new(&base, Arc::new(|_| {}));
        cloud.resume(1, "refresh".into());
        cloud
    }

    // The cloud registers a connection for pushes after the handshake and says so with
    // `welcome`. Lists fetched on going online must come after that, or what changes in
    // between reaches neither the lists nor the connection.
    #[tokio::test]
    async fn the_connection_is_online_from_the_welcome_on() {
        let (register, registered) = oneshot::channel();
        let cloud = cloud_that_waits(registered).await;
        let (tx, mut rx) = mpsc::unbounded_channel();
        let task = spawn(cloud, tx);
        assert!(matches!(rx.recv().await, Some(EventInput::Connecting)));

        let early = tokio::time::timeout(Duration::from_millis(500), rx.recv()).await;
        assert!(early.is_err(), "before the welcome: {early:?}");

        register.send(()).unwrap();
        assert!(matches!(rx.recv().await, Some(EventInput::Online)));
        assert!(matches!(
            rx.recv().await,
            Some(EventInput::Event(Event::Welcome { .. }))
        ));
        task.abort();
    }

    /// A cloud that sends `welcome` and then closes the event connection with `code`.
    async fn cloud_that_closes(code: u16) -> Cloud {
        crate::install_crypto();
        let app = axum::Router::new()
            .route(
                path::AUTH_REFRESH,
                post(|| async {
                    axum::Json(Tokens {
                        access: "access".into(),
                        access_expires: now() + 3600,
                        refresh: "refresh".into(),
                        device: 1,
                    })
                }),
            )
            .route(
                path::EVENTS,
                get(move |upgrade: WebSocketUpgrade| async move {
                    upgrade.on_upgrade(move |mut socket| async move {
                        let welcome = Event::Welcome {
                            address: "10.64.0.2".into(),
                            server_time: now(),
                        };
                        let text = serde_json::to_string(&welcome).unwrap();
                        let _ = socket.send(Message::Text(text.into())).await;
                        let frame = axum::extract::ws::CloseFrame {
                            code,
                            reason: "restarting".into(),
                        };
                        let _ = socket.send(Message::Close(Some(frame))).await;
                    })
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await });
        let cloud = Cloud::new(&base, Arc::new(|_| {}));
        cloud.resume(1, "refresh".into());
        cloud
    }

    #[tokio::test]
    async fn a_cloud_that_restarts_sends_its_clients_away_to_come_back_later() {
        for code in [close::GOING_AWAY, close::RESTARTING] {
            let cloud = cloud_that_closes(code).await;
            let (tx, _rx) = mpsc::unbounded_channel();
            let ending = session(&cloud, &tx).await;
            assert!(matches!(ending, Ending::GoingAway(_)), "code {code}");
        }
        let cloud = cloud_that_closes(1011).await;
        let (tx, _rx) = mpsc::unbounded_channel();
        assert!(matches!(session(&cloud, &tx).await, Ending::Broken { .. }));
    }

    #[test]
    fn a_pause_spreads_over_a_whole_pause() {
        let pause = Duration::from_secs(1);
        let pauses: Vec<Duration> = (0..2000).map(|_| jitter(pause)).collect();
        assert!(
            pauses
                .iter()
                .all(|p| *p >= pause / 2 && *p <= pause * 3 / 2)
        );
        assert!(pauses.iter().any(|p| *p < Duration::from_millis(600)));
        assert!(pauses.iter().any(|p| *p > Duration::from_millis(1400)));
    }

    #[test]
    fn reconnects_after_a_restart_spread_over_the_whole_window() {
        let window = Duration::from_secs(close::RECONNECT_SPREAD_SECS);
        let waits: Vec<Duration> = (0..2000).map(|_| spread()).collect();
        assert!(waits.iter().all(|w| *w <= window));
        assert!(waits.iter().any(|w| *w < window / 10));
        assert!(waits.iter().any(|w| *w > window * 9 / 10));
    }
}
