//! Discord's Social SDK in the service (phase 0 of ADR 0022): the connect flow and the state the
//! launcher is told.
//!
//! Connecting: the SDK authorises the player (Discord's overlay, app or browser) and reports a
//! code; the cloud trades it for tokens (it keeps none); the access token goes to the SDK and the
//! refresh token into the credential store, tied to the AnyPlay account. A later connect uses the
//! refresh token and only asks the player again when Discord refuses it.

use anyplay_api::{DiscordCodeExchange, ErrorCode};
use anyplay_control::{self as control, DiscordInfo, DiscordState, DiscordUser, Done, Failure};
use anyplay_discord::{Event, Snapshot, State};

use super::Service;
use crate::cloud::CloudError;
use crate::credentials::SavedDiscord;

/// The service's side of Discord.
pub(super) struct DiscordSide {
    social: Box<dyn anyplay_discord::Social>,
    snapshot: Snapshot,
    /// A request to the cloud for tokens is out, so the player is connecting although the SDK
    /// has not been told anything yet.
    busy: bool,
    /// What the launcher was last told.
    told: Option<DiscordInfo>,
    /// Saves and deletions of the refresh token, done one after the other in the order they were
    /// asked, by a thread of their own (the credential store blocks).
    writer: Option<std::sync::mpsc::Sender<Option<SavedDiscord>>>,
}

impl DiscordSide {
    pub(super) fn new(social: Box<dyn anyplay_discord::Social>) -> Self {
        let snapshot = social.snapshot();
        Self {
            social,
            snapshot,
            busy: false,
            told: None,
            writer: None,
        }
    }

    fn info(&self) -> DiscordInfo {
        let state = match self.snapshot.state {
            State::Unavailable => DiscordState::Unavailable,
            State::SignedOut if self.busy => DiscordState::Connecting,
            State::SignedOut => DiscordState::SignedOut,
            State::Connecting => DiscordState::Connecting,
            State::Connected => DiscordState::Connected,
        };
        DiscordInfo {
            state,
            user: self.snapshot.user.as_ref().map(|u| DiscordUser {
                id: u.id.clone(),
                name: u.name.clone(),
            }),
            friends: self.snapshot.friends,
        }
    }
}

impl Service {
    fn discord_notice(&self, message: &str) {
        self.event(control::Event::Notice {
            code: "discord_failed".to_owned(),
            message: message.to_owned(),
        });
    }

    /// Tells the launcher the state, when it differs from what it was last told.
    pub(super) fn discord_changed(&mut self) {
        let info = self.discord.info();
        if self.discord.told.as_ref() != Some(&info) {
            self.discord.told = Some(info.clone());
            self.event(control::Event::Discord(info));
        }
    }

    pub(super) fn discord_status(&self, id: u64) {
        self.ok(id, self.discord.info());
    }

    fn discord_store(&self) -> Option<crate::credentials::Store> {
        self.store.as_ref().map(|store| store.discord())
    }

    /// Keeps the refresh token, or forgets it when `None`. Changes are applied in order.
    fn discord_persist(&mut self, saved: Option<SavedDiscord>) {
        if self.discord.writer.is_none() {
            let Some(store) = self.discord_store() else {
                return;
            };
            let (tx, rx) = std::sync::mpsc::channel::<Option<SavedDiscord>>();
            std::thread::spawn(move || {
                for saved in rx {
                    store.save_discord(saved.as_ref());
                }
            });
            self.discord.writer = Some(tx);
        }
        if let Some(writer) = &self.discord.writer {
            let _ = writer.send(saved);
        }
    }

    fn discord_forget_token(&mut self) {
        self.discord_persist(None);
    }

    pub(super) fn discord_connect(&mut self, id: u64) {
        let Some(cloud) = self.signed_in_cloud(id) else {
            return;
        };
        let Some(account) = self.me.as_ref().map(|me| me.id) else {
            return;
        };
        match self.discord.snapshot.state {
            State::Unavailable => {
                self.fail(
                    id,
                    Failure::new(
                        Failure::BAD_STATE,
                        "Discord is not available in this build.",
                    ),
                );
                return;
            }
            State::Connecting | State::Connected => {
                self.ok(id, Done {});
                return;
            }
            State::SignedOut if self.discord.busy => {
                self.ok(id, Done {});
                return;
            }
            State::SignedOut => {}
        }
        self.discord.busy = true;
        self.ok(id, Done {});
        self.discord_changed();
        let store = self.discord_store();
        self.spawn(
            async move {
                let store = store?;
                tokio::task::spawn_blocking(move || store.load_discord())
                    .await
                    .ok()
                    .flatten()
                    .filter(|saved| saved.account == account)
            },
            move |service, saved| {
                if !service.discord_current(account) {
                    return;
                }
                match saved {
                    Some(saved) => service.discord_refresh(cloud, account, saved.refresh),
                    None => service.discord_authorize(),
                }
            },
        );
    }

    fn discord_refresh(
        &mut self,
        cloud: crate::cloud::Cloud,
        account: anyplay_api::AccountId,
        refresh: String,
    ) {
        self.spawn(
            async move { cloud.discord_refresh(&refresh).await },
            move |service, result| match result {
                Ok(tokens) => service.discord_tokens(account, tokens),
                Err(error) if error.is(ErrorCode::GrantRefused) => {
                    tracing::info!("Discord refused the kept token; asking the player again");
                    service.discord_forget_token();
                    service.discord_authorize();
                }
                Err(error) => service.discord_cloud_failed(&error),
            },
        );
    }

    fn discord_authorize(&mut self) {
        if let Err(error) = self.discord.social.authorize() {
            self.discord.busy = false;
            self.discord_notice(&error.to_string());
        }
        self.discord_changed();
    }

    /// Hands the tokens to the SDK and keeps the refresh token.
    fn discord_tokens(
        &mut self,
        account: anyplay_api::AccountId,
        tokens: anyplay_api::DiscordTokens,
    ) {
        // An answer that comes after the player disconnected or signed out is dropped: it must
        // not keep a token again or connect.
        if !self.discord_current(account) {
            return;
        }
        self.discord.busy = false;
        self.discord_persist(Some(SavedDiscord {
            account,
            refresh: tokens.refresh.clone(),
        }));
        if let Err(error) = self.discord.social.connect(&tokens.access) {
            self.discord_notice(&error.to_string());
        }
        self.discord_changed();
    }

    /// Whether a connection flow for `account` is still wanted: one is under way and the player is
    /// still that account.
    fn discord_current(&self, account: anyplay_api::AccountId) -> bool {
        self.discord.busy && self.me.as_ref().map(|me| me.id) == Some(account)
    }

    fn discord_cloud_failed(&mut self, error: &CloudError) {
        self.discord.busy = false;
        // Whatever the SDK started is over: it waits for nothing now.
        self.discord.social.disconnect();
        let failure = self.failure(error);
        self.discord_notice(&failure.message);
        self.discord_changed();
    }

    pub(super) fn discord_disconnect(&mut self, id: u64) {
        self.discord_end(true);
        self.ok(id, Done {});
    }

    /// The connection goes: the player disconnected, signed out or lost the account. The kept
    /// refresh token goes too when `forget_token`.
    pub(super) fn discord_end(&mut self, forget_token: bool) {
        self.discord.busy = false;
        self.discord.social.abort_authorize();
        self.discord.social.disconnect();
        if forget_token {
            self.discord_forget_token();
        }
        self.discord_changed();
    }

    /// Something the SDK reported.
    pub(super) fn discord_event(&mut self, event: Event) {
        match event {
            Event::Changed(snapshot) => {
                self.discord.snapshot = snapshot;
                self.discord_changed();
            }
            Event::Authorized(authorization) => {
                let (Some(cloud), Some(account)) =
                    (self.cloud.clone(), self.me.as_ref().map(|me| me.id))
                else {
                    self.discord.social.disconnect();
                    return;
                };
                self.discord.busy = true;
                let exchange = DiscordCodeExchange {
                    code: authorization.code,
                    code_verifier: authorization.verifier,
                    redirect_uri: authorization.redirect_uri,
                };
                self.spawn(
                    async move { cloud.discord_token(&exchange).await },
                    move |service, result| match result {
                        Ok(tokens) => service.discord_tokens(account, tokens),
                        Err(error) => service.discord_cloud_failed(&error),
                    },
                );
                self.discord_changed();
            }
            Event::AuthorizationFailed(why) => {
                self.discord.busy = false;
                tracing::info!("Discord authorisation ended: {why}");
                self.discord_notice("Discord did not authorise the connection.");
                self.discord_changed();
            }
            Event::Failed(why) => {
                self.discord.busy = false;
                tracing::info!("Discord connection failed: {why}");
                self.discord_notice("Discord refused the connection.");
                self.discord_changed();
            }
        }
    }
}
