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
    /// The connection is open.
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
    Broken { reason: String, was_online: bool },
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
    let _ = out.send(EventInput::Online);
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
                let _ = out.send(EventInput::Event(event));
            }
            Err(error) => tracing::warn!("unreadable event: {error}"),
        }
    }
}

fn http_authorization() -> http::HeaderName {
    http::header::AUTHORIZATION
}

/// The pause, stretched by up to a quarter so that many net services do not reconnect at once.
fn jitter(pause: Duration) -> Duration {
    let quarter = pause.as_millis() as u64 / 4;
    pause + Duration::from_millis(rand::random_range(0..=quarter))
}
