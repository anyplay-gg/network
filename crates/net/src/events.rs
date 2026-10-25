//! The event connection to the cloud: kept open while the player is signed in, reopened with
//! growing pauses when it breaks.

use std::time::Duration;

use anyplay_api::{ErrorCode, Event, close};
use futures_util::StreamExt;
use tokio::sync::mpsc::UnboundedSender;
use tokio::task::JoinHandle;
use tokio_websockets::{ClientBuilder, Limits};

use anyplay_control::Restriction;

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
    Banned(Restriction),
    /// The cloud refuses this client because it is older than the version named (empty when the
    /// cloud did not say). The connection ends for good; it is not retried until the service
    /// is started again.
    ClientTooOld(String),
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
                Ending::Banned(restriction) => {
                    let _ = out.send(EventInput::Banned(restriction));
                    return;
                }
                Ending::ClientTooOld(minimum) => {
                    let _ = out.send(EventInput::ClientTooOld(minimum));
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
            Restriction::of_error(api).map(EventInput::Banned)
        }
        CloudError::Api(api) if api.error == ErrorCode::ClientTooOld => Some(
            EventInput::ClientTooOld(api.minimum.as_deref().unwrap_or_default().to_owned()),
        ),
        _ => None,
    }
}

enum Ending {
    SignedOut,
    Banned(Restriction),
    ClientTooOld(String),
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
                Some(EventInput::Banned(restriction)) => Ending::Banned(restriction),
                Some(EventInput::ClientTooOld(minimum)) => Ending::ClientTooOld(minimum),
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
    let Ok(client) = cloud.client().parse::<http::HeaderValue>() else {
        return Ending::Broken {
            reason: "the client name is not a header value".into(),
            was_online: false,
        };
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
        .and_then(|builder| {
            builder.add_header(http::HeaderName::from_static("x-anyplay-client"), client)
        }) {
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
        Ok(Err(tokio_websockets::Error::Upgrade(
            tokio_websockets::upgrade::Error::DidNotSwitchProtocols(426),
        ))) => return Ending::ClientTooOld(cloud.required_version(&access).await),
        Ok(Err(error)) => return broken(error.to_string()),
        Err(_) => return broken("the cloud did not answer in time".into()),
    };
    // The cloud sends `welcome` first, once it pushes to this connection. Until then a change
    // could reach neither the connection nor lists fetched on going online.
    let mut welcomed = false;
    let mut banned = None;
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
                close::BANNED => {
                    Ending::Banned(banned.unwrap_or_else(|| Restriction::of_ban(None, None)))
                }
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
            Ok(Event::Banned { until, statement }) => {
                banned = Some(Restriction::of_ban(until, statement.as_deref()));
            }
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
    use axum::response::IntoResponse;
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
        cloud.resume(1, "refresh".into(), None);
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

    /// A cloud that sends `welcome`, then `notice` if there is one, and closes the event
    /// connection with `code`.
    async fn cloud_that_closes(code: u16, notice: Option<Event>) -> Cloud {
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
                        if let Some(notice) = notice {
                            let text = serde_json::to_string(&notice).unwrap();
                            let _ = socket.send(Message::Text(text.into())).await;
                        }
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
        cloud.resume(1, "refresh".into(), None);
        cloud
    }

    #[tokio::test]
    async fn a_cloud_that_restarts_sends_its_clients_away_to_come_back_later() {
        for code in [close::GOING_AWAY, close::RESTARTING] {
            let cloud = cloud_that_closes(code, None).await;
            let (tx, _rx) = mpsc::unbounded_channel();
            let ending = session(&cloud, &tx).await;
            assert!(matches!(ending, Ending::GoingAway(_)), "code {code}");
        }
        let cloud = cloud_that_closes(1011, None).await;
        let (tx, _rx) = mpsc::unbounded_channel();
        assert!(matches!(session(&cloud, &tx).await, Ending::Broken { .. }));
    }

    // The launcher tells the player why: the ban's end and statement travel from the cloud's
    // `banned` event to the ending of the connection.
    #[tokio::test]
    async fn a_ban_keeps_its_end_and_its_statement() {
        let statement = anyplay_api::Statement {
            id: 8,
            kind: anyplay_api::ActionKind::Ban,
            rule: anyplay_api::Rule::Harassment,
            reason: "Repeated insults.".into(),
            created: 100,
            until: Some(5000),
            lifted: None,
            from_report: true,
            automated: false,
            appeal: anyplay_api::AppealInfo {
                status: anyplay_api::AppealStatus::Available,
                deadline: 9000,
                text: None,
                answer: None,
                decided: None,
            },
        };
        let banned = Event::Banned {
            until: Some(5000),
            statement: Some(Box::new(statement)),
        };
        let cloud = cloud_that_closes(close::BANNED, Some(banned)).await;
        let (tx, _rx) = mpsc::unbounded_channel();
        let Ending::Banned(restriction) = session(&cloud, &tx).await else {
            panic!("not a ban");
        };
        assert_eq!(restriction.kind, Restriction::BANNED);
        assert_eq!(restriction.until, Some(5000));
        assert_eq!(restriction.rule.as_deref(), Some("harassment"));
        assert_eq!(restriction.reason.as_deref(), Some("Repeated insults."));
        assert_eq!(restriction.action, Some(8));

        let bare = cloud_that_closes(close::BANNED, None).await;
        let Ending::Banned(restriction) = session(&bare, &tx).await else {
            panic!("not a ban");
        };
        assert_eq!(restriction, Restriction::new(Restriction::BANNED, None));
    }

    /// A cloud that refuses the event handshake with 426 unless the client is at least
    /// `0.3.0` as the header says; an accepted handshake reports the header it saw.
    async fn cloud_with_a_gate(seen: Arc<Mutex<Vec<String>>>) -> Cloud {
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
                get(
                    move |headers: axum::http::HeaderMap,
                          upgrade: Result<
                        WebSocketUpgrade,
                        axum::extract::ws::rejection::WebSocketUpgradeRejection,
                    >| async move {
                        let client = headers
                            .get("x-anyplay-client")
                            .and_then(|v| v.to_str().ok())
                            .unwrap_or("")
                            .to_owned();
                        seen.lock().await.push(client.clone());
                        if client == "launcher/0.2.0" {
                            return (
                                axum::http::StatusCode::UPGRADE_REQUIRED,
                                axum::Json(serde_json::json!({
                                    "error": "client_too_old",
                                    "message": "Update.",
                                    "minimum": "0.3.0"
                                })),
                            )
                                .into_response();
                        }
                        match upgrade {
                            Ok(upgrade) => upgrade.on_upgrade(|mut socket| async move {
                                let _ = socket.send(Message::Close(None)).await;
                            }),
                            Err(_) => axum::http::StatusCode::BAD_REQUEST.into_response(),
                        }
                    },
                ),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await });
        Cloud::with_client(&base, Arc::new(|_| {}), "launcher/0.2.0".into())
    }

    #[tokio::test]
    async fn the_handshake_names_the_client_and_a_426_ends_the_connection_for_good() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let cloud = cloud_with_a_gate(seen.clone()).await;
        cloud.resume(1, "refresh".into(), None);
        let (tx, mut rx) = mpsc::unbounded_channel();
        let task = spawn(cloud, tx);
        assert!(matches!(rx.recv().await, Some(EventInput::Connecting)));
        match rx.recv().await {
            Some(EventInput::ClientTooOld(minimum)) => assert_eq!(minimum, "0.3.0"),
            other => panic!("{other:?}"),
        }
        // The task ended: nothing more comes, however long it waits, and no second try was made.
        assert!(
            tokio::time::timeout(Duration::from_secs(3), task)
                .await
                .is_ok(),
            "the loop kept running"
        );
        assert!(rx.recv().await.is_none());
        let seen = seen.lock().await;
        assert!(
            !seen.is_empty() && seen.iter().all(|c| c == "launcher/0.2.0"),
            "{seen:?}"
        );
        // One handshake, and one plain request that asked for the minimum.
        assert_eq!(seen.len(), 2, "{seen:?}");
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
