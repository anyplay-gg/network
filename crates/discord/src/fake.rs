//! The fake [`Social`]: what CI and every test run. It talks to nobody, answers at once, and
//! records what it was asked so a test can check it.

use std::sync::{Arc, Mutex};

use tokio::sync::mpsc::UnboundedSender;

use crate::{Authorization, Error, Event, Snapshot, Social, State, User};

/// How the fake behaves, and the record of what it was asked. Clones share the record.
#[derive(Clone, Debug)]
pub struct FakeScript {
    /// The player that connecting leads to.
    pub user: User,
    /// The number of friends the player has.
    pub friends: u32,
    /// The player closes Discord's prompt instead of agreeing.
    pub deny_authorization: bool,
    /// Discord refuses every access token it is given.
    pub reject_tokens: bool,
    /// What the fake was asked.
    pub seen: Seen,
}

impl Default for FakeScript {
    fn default() -> Self {
        Self {
            user: User {
                id: "1563714917998133289".into(),
                name: "Test Player".into(),
            },
            friends: 3,
            deny_authorization: false,
            reject_tokens: false,
            seen: Seen::default(),
        }
    }
}

/// The record of a fake's calls.
#[derive(Clone, Debug, Default)]
pub struct Seen(Arc<Mutex<Log>>);

#[derive(Debug, Default)]
struct Log {
    authorizations: u32,
    aborts: u32,
    disconnects: u32,
    tokens: Vec<String>,
}

impl Seen {
    fn log(&self) -> std::sync::MutexGuard<'_, Log> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// How often an authorisation was started.
    pub fn authorizations(&self) -> u32 {
        self.log().authorizations
    }

    pub fn aborts(&self) -> u32 {
        self.log().aborts
    }

    pub fn disconnects(&self) -> u32 {
        self.log().disconnects
    }

    /// The access tokens it was connected with, in order.
    pub fn tokens(&self) -> Vec<String> {
        self.log().tokens.clone()
    }
}

/// See the module.
pub struct Fake {
    script: FakeScript,
    events: UnboundedSender<Event>,
    snapshot: Snapshot,
}

impl Fake {
    pub fn new(script: FakeScript, events: UnboundedSender<Event>) -> Self {
        Self {
            script,
            events,
            snapshot: Snapshot::new(State::SignedOut),
        }
    }

    fn change(&mut self, snapshot: Snapshot) {
        if self.snapshot != snapshot {
            self.snapshot = snapshot.clone();
            let _ = self.events.send(Event::Changed(snapshot));
        }
    }
}

impl Social for Fake {
    fn snapshot(&self) -> Snapshot {
        self.snapshot.clone()
    }

    fn authorize(&mut self) -> Result<(), Error> {
        if self.snapshot.state != State::SignedOut {
            return Err(Error::Busy);
        }
        let number = {
            let mut log = self.script.seen.log();
            log.authorizations += 1;
            log.authorizations
        };
        self.change(Snapshot::new(State::Connecting));
        if self.script.deny_authorization {
            let _ = self.events.send(Event::AuthorizationFailed(
                "The authorization was closed.".into(),
            ));
            self.change(Snapshot::new(State::SignedOut));
        } else {
            let _ = self.events.send(Event::Authorized(Authorization {
                code: format!("fake-code-{number}"),
                verifier: format!("fake-verifier-{number}"),
                redirect_uri: "http://127.0.0.1/callback".into(),
            }));
        }
        Ok(())
    }

    fn abort_authorize(&mut self) {
        self.script.seen.log().aborts += 1;
        if self.snapshot.state == State::Connecting {
            self.change(Snapshot::new(State::SignedOut));
        }
    }

    fn connect(&mut self, access_token: &str) -> Result<(), Error> {
        if self.snapshot.state == State::Connected {
            return Err(Error::Busy);
        }
        self.script.seen.log().tokens.push(access_token.to_owned());
        self.change(Snapshot::new(State::Connecting));
        if self.script.reject_tokens || access_token.is_empty() {
            let _ = self
                .events
                .send(Event::Failed("Discord refused the token.".into()));
            self.change(Snapshot::new(State::SignedOut));
        } else {
            self.change(Snapshot {
                state: State::Connected,
                user: Some(self.script.user.clone()),
                friends: Some(self.script.friends),
            });
        }
        Ok(())
    }

    fn disconnect(&mut self) {
        self.script.seen.log().disconnects += 1;
        self.change(Snapshot::new(State::SignedOut));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc::unbounded_channel;

    fn drain(rx: &mut tokio::sync::mpsc::UnboundedReceiver<Event>) -> Vec<Event> {
        let mut all = Vec::new();
        while let Ok(event) = rx.try_recv() {
            all.push(event);
        }
        all
    }

    #[test]
    fn authorizing_then_connecting_reaches_connected_with_the_player() {
        let (tx, mut rx) = unbounded_channel();
        let script = FakeScript::default();
        let mut fake = Fake::new(script.clone(), tx);
        fake.authorize().unwrap();
        let events = drain(&mut rx);
        assert_eq!(events.len(), 2, "{events:?}");
        assert_eq!(events[0], Event::Changed(Snapshot::new(State::Connecting)));
        let Event::Authorized(auth) = &events[1] else {
            panic!("{events:?}");
        };
        assert_eq!(auth.code, "fake-code-1");
        fake.connect("access-1").unwrap();
        let snapshot = fake.snapshot();
        assert_eq!(snapshot.state, State::Connected);
        assert_eq!(snapshot.user, Some(script.user.clone()));
        assert_eq!(snapshot.friends, Some(3));
        assert_eq!(script.seen.tokens(), ["access-1"]);
        assert_eq!(script.seen.authorizations(), 1);
        assert_eq!(fake.connect("again"), Err(Error::Busy));
        fake.disconnect();
        assert_eq!(fake.snapshot(), Snapshot::new(State::SignedOut));
        assert_eq!(script.seen.disconnects(), 1);
    }

    #[test]
    fn a_closed_prompt_ends_signed_out() {
        let (tx, mut rx) = unbounded_channel();
        let mut fake = Fake::new(
            FakeScript {
                deny_authorization: true,
                ..FakeScript::default()
            },
            tx,
        );
        fake.authorize().unwrap();
        let events = drain(&mut rx);
        assert!(matches!(events[1], Event::AuthorizationFailed(_)));
        assert_eq!(fake.snapshot().state, State::SignedOut);
    }

    #[test]
    fn a_refused_token_fails_and_stays_signed_out() {
        let (tx, mut rx) = unbounded_channel();
        let mut fake = Fake::new(
            FakeScript {
                reject_tokens: true,
                ..FakeScript::default()
            },
            tx,
        );
        fake.connect("bad").unwrap();
        let events = drain(&mut rx);
        assert!(events.iter().any(|e| matches!(e, Event::Failed(_))));
        assert_eq!(fake.snapshot().state, State::SignedOut);
    }

    #[test]
    fn a_second_authorization_while_busy_is_refused() {
        let (tx, _rx) = unbounded_channel();
        let mut fake = Fake::new(FakeScript::default(), tx);
        fake.authorize().unwrap();
        assert_eq!(fake.authorize(), Err(Error::Busy));
    }
}
