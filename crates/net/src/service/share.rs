//! Share play in the service (`spec/share-play.md`): the invitation, the cloud's `Share`, and the
//! [`play::Hub`] that holds the one share connection.
//!
//! The cloud decides who shares with whom; the hub connects only to the endpoint the cloud's
//! `Share` names, and only while that account is in the player's party. This side keeps the
//! cloud's copy of the share and the invitations, binds the hub to an active share, carries the
//! hub's reports to the cloud (mode, path, end), the game's bridge and the launcher, and answers
//! the launcher's commands.

use std::collections::HashMap;

use anyplay_api::{
    AcceptShare, Account, AccountId, CreateShare, ErrorCode, Share, ShareGoneReason,
    ShareInvitation, ShareMode, ShareState, UpdateShare,
};
use anyplay_bridge_protocol::Frame;
use anyplay_control::{self as control, Done, Failure, SharePhase, ShareRole, ShareView};
use anyplay_share::control::Role;
use iroh::{EndpointId, RelayUrl};
use tokio::sync::mpsc;

use super::{Input, Service};
use crate::answers::is_older;
use crate::cloud::CloudError;
use crate::play::{self, Bind, Output, code};

/// The service's side of share play.
#[derive(Default)]
pub(super) struct ShareSide {
    /// Exists while the player is signed in with tunnels up; idle until a share is bound.
    hub: Option<play::Hub>,
    /// Counts the hubs, so that a report of an older one is dropped.
    pub(super) generation: u64,
    /// The cloud's copy of the share the player is in.
    share: Option<Share>,
    invitations: HashMap<String, ShareInvitation>,
    /// What the hub last reported about the share.
    view: Option<play::View>,
    /// Whether the cloud offers share play; unknown until asked.
    enabled: Option<bool>,
    /// What the launcher was last told.
    told: Option<(bool, Option<ShareView>)>,
}

impl ShareSide {
    pub(super) fn hub_is_there(&self, generation: u64) -> bool {
        generation == self.generation && self.hub.is_some()
    }
}

/// How an end of the share by the cloud closes the connection.
fn close_code(reason: ShareGoneReason) -> u32 {
    match reason {
        ShareGoneReason::Blocked | ShareGoneReason::Banned => code::BLOCKED,
        ShareGoneReason::Timeout | ShareGoneReason::Quota => code::TIMED_OUT,
        ShareGoneReason::Declined | ShareGoneReason::Left => code::GUEST_LEFT,
        ShareGoneReason::Ended | ShareGoneReason::GameEnded | ShareGoneReason::Unknown => {
            code::HOST_STOPPED
        }
    }
}

fn reason_name(reason: ShareGoneReason) -> &'static str {
    match reason {
        ShareGoneReason::Ended | ShareGoneReason::Unknown => "ended",
        ShareGoneReason::Declined => "declined",
        ShareGoneReason::Left => "left",
        ShareGoneReason::GameEnded => "game_ended",
        ShareGoneReason::Blocked => "blocked",
        ShareGoneReason::Banned => "banned",
        ShareGoneReason::Timeout => "timeout",
        ShareGoneReason::Quota => "quota",
    }
}

impl Service {
    // The hub

    /// Brings the hub in line with the tunnels: it exists while the player is signed in with
    /// tunnels up, and a share the cloud holds is bound to it.
    pub(super) fn share_sync(&mut self) {
        if self.me.is_none() || self.tunnels.is_none() {
            if self.share.hub.take().is_some() {
                self.share.generation += 1;
                self.share.view = None;
                self.share_changed();
            }
            return;
        }
        if self.share.hub.is_some() {
            return;
        }
        let Some(tunnels) = self.tunnels.clone() else {
            return;
        };
        self.share.generation += 1;
        let generation = self.share.generation;
        let (reports, mut rx) = mpsc::unbounded_channel();
        let inbox = self.inbox.clone();
        tokio::spawn(async move {
            while let Some(output) = rx.recv().await {
                if inbox.send(Input::Play(generation, output)).is_err() {
                    return;
                }
            }
        });
        self.share.hub = Some(play::Hub::start(
            &tunnels,
            self.config.workers.shared(),
            reports,
        ));
        self.share_bind();
        self.share_changed();
    }

    /// Forgets everything of the account, as after a sign-out.
    pub(super) fn share_reset(&mut self) {
        self.share.hub = None;
        self.share.generation += 1;
        self.share.share = None;
        self.share.invitations.clear();
        self.share.view = None;
        self.share.enabled = None;
        self.share_changed();
    }

    /// Whether `account` is a member of the player's party.
    fn share_in_party(&self, account: AccountId) -> bool {
        self.party
            .as_ref()
            .is_some_and(|party| party.members.iter().any(|m| m.account.id == account))
    }

    /// The other side of the share, as the cloud's copy names it.
    fn share_peer(&self) -> Option<(Role, &Account)> {
        let share = self.share.share.as_ref()?;
        let me = self.me.as_ref()?.id;
        if share.host.id == me {
            Some((Role::Host, &share.guest))
        } else if share.guest.id == me {
            Some((Role::Guest, &share.host))
        } else {
            None
        }
    }

    /// Binds the hub to the active share, when both ends are known and the other side is in
    /// the party. A share whose other side left the party ends here at once.
    pub(super) fn share_bind(&mut self) {
        let Some(share) = self.share.share.clone() else {
            return;
        };
        if share.state != ShareState::Active {
            return;
        }
        let Some((role, peer)) = self.share_peer().map(|(r, a)| (r, a.clone())) else {
            return;
        };
        if self.party.is_some() && !self.share_in_party(peer.id) {
            self.share_leave("not_in_party");
            return;
        }
        // Without the party the sign-in has not finished loading it: wait for it.
        if self.party.is_none() {
            return;
        }
        let (endpoint, relay) = match role {
            Role::Host => (&share.guest_endpoint, &share.guest_relay),
            Role::Guest => (&share.host_endpoint, &share.host_relay),
        };
        let Ok(peer_key) = endpoint.parse::<EndpointId>() else {
            return;
        };
        if peer_key == self.secret.public() {
            return;
        }
        let Some(hub) = &self.share.hub else {
            return;
        };
        hub.bind(Bind {
            id: share.id.clone(),
            role,
            peer: peer_key,
            relay: relay.as_deref().and_then(|r| r.parse::<RelayUrl>().ok()),
            title: share.title.clone(),
            mode: share.mode,
        });
    }

    /// The party changed or went: a share with someone who is no longer in it ends.
    pub(super) fn share_party_changed(&mut self) {
        if let Some((_, peer)) = self.share_peer().map(|(r, a)| (r, a.id))
            && !self.share_in_party(peer)
            && self
                .share
                .share
                .as_ref()
                .is_some_and(|s| s.state == ShareState::Active)
        {
            self.share_leave("not_in_party");
            return;
        }
        self.share_bind();
        self.share_changed();
    }

    /// The player's side of the share ends: the hub closes, the cloud is told, the launcher
    /// hears.
    fn share_leave(&mut self, reason: &str) {
        let Some(share) = self.share.share.take() else {
            return;
        };
        let host = self.me.as_ref().is_some_and(|me| share.host.id == me.id);
        let close = if host {
            code::HOST_STOPPED
        } else {
            code::GUEST_LEFT
        };
        if let Some(hub) = &self.share.hub {
            hub.unbind(close, reason);
        }
        self.share_end_remote();
        self.share.view = None;
        self.share_changed();
    }

    /// Tells the cloud that the share is over (or declined, or left).
    fn share_end_remote(&self) {
        let Some(cloud) = self.cloud.clone().filter(|_| self.me.is_some()) else {
            return;
        };
        self.spawn(
            async move { cloud.end_share().await },
            |_, result| match result {
                Ok(()) => {}
                Err(CloudError::Api(api)) if api.error == ErrorCode::NotFound => {}
                Err(error) => tracing::info!("share: ending it: {error}"),
            },
        );
    }

    /// The game ended: the host's share ends with it.
    pub(super) fn share_game_ended(&mut self) {
        let host = self
            .share_peer()
            .is_some_and(|(role, _)| role == Role::Host);
        if host {
            self.share_leave("game_ended");
        }
    }

    /// The bridge to the game went away.
    pub(super) fn share_bridge_lost(&mut self) {
        if let Some(hub) = &self.share.hub {
            hub.bridge_lost();
        }
    }

    /// A share frame from the game.
    pub(super) fn share_from_game(&mut self, frame: Frame) {
        if let Some(hub) = &self.share.hub {
            hub.bridge(frame);
        }
    }

    // What the hub reports

    pub(super) fn share_output(&mut self, output: Output) {
        match output {
            Output::View(view) => {
                self.share.view = view;
                self.share_changed();
            }
            Output::Bridge(frame) => match &self.bridge {
                Some(bridge) => {
                    let _ = bridge.frames.send(frame);
                }
                // The game is not there to take it: the share cannot run.
                None => self.share_bridge_lost(),
            },
            Output::Mode(mode) => {
                self.share_update_remote(UpdateShare {
                    mode: Some(mode),
                    path: None,
                });
                if let Some(share) = &mut self.share.share {
                    share.mode = mode;
                }
                self.share_changed();
            }
            Output::Request(mode) => {
                self.event(control::Event::ShareRequest { mode });
            }
            Output::Path(path) => self.share_update_remote(UpdateShare {
                mode: None,
                path: Some(path),
            }),
            Output::Ended { id, reason } => {
                if self.share.share.as_ref().is_some_and(|s| s.id == id) {
                    tracing::info!("share ended here: {reason}");
                    self.share.share = None;
                    self.share.view = None;
                    self.share_end_remote();
                    self.share_changed();
                }
            }
            Output::Notice { code, message } => {
                self.event(control::Event::Notice {
                    code: code.into(),
                    message,
                });
            }
        }
    }

    /// Tells the cloud what the share shows now. The host has applied it already; the cloud's
    /// copy is for the launcher and the record, so a failure is only logged.
    fn share_update_remote(&self, body: UpdateShare) {
        let Some(cloud) = self.cloud.clone().filter(|_| self.me.is_some()) else {
            return;
        };
        self.spawn(
            async move { cloud.update_share(&body).await },
            |_, result| {
                if let Err(error) = result {
                    tracing::info!("share: telling the cloud: {error}");
                }
            },
        );
    }

    // What the cloud says

    /// Asks the cloud what it holds, after sign-in and after every reconnect.
    pub(super) fn share_fetch(&mut self) {
        let Some(cloud) = self.cloud.clone().filter(|_| self.me.is_some()) else {
            return;
        };
        self.spawn(
            async move {
                (
                    cloud.info().await,
                    cloud.share().await,
                    cloud.share_invitations().await,
                )
            },
            |service, (info, share, invitations)| {
                if let Ok(info) = info {
                    service.share.enabled = Some(info.share.is_some_and(|s| s.enabled));
                }
                match share {
                    Ok(Some(share)) => service.share_cloud_changed(share),
                    // The cloud holds none: one the player had ended while they were away.
                    Ok(None) => {
                        if service.share.share.is_some() {
                            service.share_gone_here(ShareGoneReason::Ended);
                        }
                    }
                    Err(error) => tracing::info!("share: asking the cloud: {error}"),
                }
                if let Ok(list) = invitations {
                    let held: Vec<String> = service.share.invitations.keys().cloned().collect();
                    for id in held {
                        if !list.invitations.iter().any(|i| i.id == id) {
                            service.share.invitations.remove(&id);
                            service.event(control::Event::ShareInvitationGone { id });
                        }
                    }
                    for invitation in list.invitations {
                        service.share_invitation(invitation);
                    }
                }
                service.share_changed();
            },
        );
    }

    fn share_invitation(&mut self, invitation: ShareInvitation) {
        if self
            .share
            .invitations
            .insert(invitation.id.clone(), invitation.clone())
            .is_none()
        {
            self.event(control::Event::ShareInvitation { invitation });
        }
    }

    /// A share event of the cloud, or an answer that carries the share.
    pub(super) fn share_cloud_event(&mut self, event: anyplay_api::Event) {
        use anyplay_api::Event;
        match event {
            Event::ShareInvitation { invitation } => {
                self.share_invitation(invitation);
                self.share_changed();
            }
            Event::ShareInvitationGone { id } => {
                if self.share.invitations.remove(&id).is_some() {
                    self.event(control::Event::ShareInvitationGone { id });
                }
                self.share_changed();
            }
            Event::Share { share } => self.share_cloud_changed(*share),
            Event::ShareGone { id, reason }
                if self.share.share.as_ref().is_some_and(|s| s.id == id) =>
            {
                self.share_gone_here(reason);
            }
            _ => {}
        }
    }

    /// Takes a share the cloud sent unless a newer copy of it is held already.
    fn share_cloud_changed(&mut self, share: Share) {
        if let Some(known) = &self.share.share
            && known.id == share.id
            && is_older(share.revision, known.revision)
        {
            return;
        }
        // The host's own mode is the truth on the host's PC; the guest follows the host's
        // setup on the share connection. The cloud's copy only names who and where.
        self.share.share = Some(share);
        self.share_bind();
        self.share_changed();
    }

    /// The cloud says this account is no longer in the share.
    fn share_gone_here(&mut self, reason: ShareGoneReason) {
        let Some(share) = self.share.share.take() else {
            return;
        };
        if let Some(hub) = &self.share.hub {
            hub.unbind(close_code(reason), reason_name(reason));
        }
        let _ = share;
        self.share.view = None;
        self.event(control::Event::Notice {
            code: format!("share_{}", reason_name(reason)),
            message: match reason {
                ShareGoneReason::Quota => "The relayed time for share play today is used.".into(),
                ShareGoneReason::Blocked | ShareGoneReason::Banned => "Share play ended.".into(),
                ShareGoneReason::Timeout => "Share play timed out.".into(),
                ShareGoneReason::GameEnded => "The game ended, and so did the share.".into(),
                _ => "Share play ended.".into(),
            },
        });
        self.share_changed();
    }

    // What the launcher sees

    /// Whether the player could start a share now.
    fn share_can_start(&self) -> bool {
        let others = self
            .party
            .as_ref()
            .is_some_and(|party| party.members.len() > 1);
        self.share.hub.is_some()
            && self.share.enabled == Some(true)
            && self.share.share.is_none()
            && self.game.is_some()
            && self.bridge.is_some()
            && others
            && self
                .config
                .workers
                .capabilities()
                .is_some_and(|c| !c.encode.is_empty())
    }

    fn share_view(&self) -> Option<ShareView> {
        let share = self.share.share.as_ref()?;
        let (role, peer) = self.share_peer()?;
        let view = self.share.view.as_ref();
        let phase = if share.state == ShareState::Invited {
            SharePhase::Invited
        } else {
            match view.map(|v| v.phase) {
                Some(play::Phase::Live) => SharePhase::Live,
                Some(play::Phase::Stalled) => SharePhase::Stalled,
                _ => SharePhase::Connecting,
            }
        };
        Some(ShareView {
            id: share.id.clone(),
            role: match role {
                Role::Host => ShareRole::Host,
                Role::Guest => ShareRole::Guest,
            },
            peer: peer.clone(),
            title: share.title.clone(),
            mode: view.map_or(share.mode, |v| v.mode),
            phase,
            link: view.map(|v| v.link),
            codec: view.and_then(|v| v.codec.clone()),
            stats: view.map(|v| v.stats).unwrap_or_default(),
            requested: view.and_then(|v| v.requested),
        })
    }

    /// Tells the launcher when what it shows differs from what it was last told.
    pub(super) fn share_changed(&mut self) {
        let now = (self.share_can_start(), self.share_view());
        if self.share.told.as_ref() == Some(&now) {
            return;
        }
        self.share.told = Some(now.clone());
        self.event(control::Event::Share {
            can_start: now.0,
            share: now.1,
        });
    }

    // Commands

    pub(super) fn share_start(&mut self, id: u64, account: AccountId, mode: ShareMode) {
        let Some(cloud) = self.signed_in_cloud(id) else {
            return;
        };
        if self.share.enabled == Some(false) {
            self.fail(
                id,
                Failure::new(
                    Failure::NOT_ALLOWED,
                    "This cloud does not offer share play.",
                ),
            );
            return;
        }
        if self.share.share.is_some() {
            self.fail(
                id,
                Failure::new(Failure::BUSY, "A share is running already."),
            );
            return;
        }
        if self.game.is_none() || self.bridge.is_none() {
            self.fail(
                id,
                Failure::new(
                    Failure::NO_GAME,
                    "Share play needs a game that runs through AnyPS5.",
                ),
            );
            return;
        }
        if mode == ShareMode::Unknown
            || self
                .me
                .as_ref()
                .is_none_or(|me| me.id == account || !self.share_in_party(account))
        {
            self.fail(
                id,
                Failure::new(
                    Failure::NOT_IN_PARTY,
                    "Share play goes to a member of your party.",
                ),
            );
            return;
        }
        if self
            .config
            .workers
            .capabilities()
            .is_none_or(|c| c.encode.is_empty())
        {
            self.fail(
                id,
                Failure::new(Failure::NO_ENCODER, "This PC cannot share a game yet."),
            );
            return;
        }
        let (Some(_), Some((endpoint, relay))) = (&self.tunnels, self.endpoint()) else {
            self.fail(
                id,
                Failure::new(Failure::OFFLINE, "The network is not ready yet."),
            );
            return;
        };
        let body = CreateShare {
            to: account,
            mode,
            endpoint,
            relay,
        };
        self.spawn(
            async move { cloud.create_share(&body).await },
            move |service, result| match result {
                Ok(share) => {
                    service.share_cloud_changed(share);
                    service.ok(id, Done {});
                }
                Err(error) => {
                    let mut failure = service.failure(&error);
                    // The cloud has no activity for the host: the game is not the cloud's news
                    // yet.
                    if matches!(&error, CloudError::Api(api) if api.error == ErrorCode::NotFound) {
                        failure = Failure::new(Failure::NO_GAME, "No game is running.");
                    }
                    if matches!(&error, CloudError::Api(api) if api.error == ErrorCode::Limit) {
                        failure = Failure::new(Failure::BUSY, "A share is running already.");
                    }
                    service.fail(id, failure);
                }
            },
        );
    }

    pub(super) fn share_answer(&mut self, id: u64, invitation: String, accept: bool) {
        let Some(cloud) = self.signed_in_cloud(id) else {
            return;
        };
        if !self.share.invitations.contains_key(&invitation) {
            self.fail(
                id,
                Failure::new(
                    ErrorCode::NotFound.as_str(),
                    "That invitation is not there any more.",
                ),
            );
            return;
        }
        if !accept {
            self.share.invitations.remove(&invitation);
            self.event(control::Event::ShareInvitationGone { id: invitation });
            self.share_end_remote();
            self.ok(id, Done {});
            return;
        }
        if self.share.share.is_some() {
            self.fail(
                id,
                Failure::new(Failure::BUSY, "A share is running already."),
            );
            return;
        }
        if self
            .config
            .workers
            .capabilities()
            .is_none_or(|c| c.decode.is_empty())
        {
            self.fail(
                id,
                Failure::new(
                    Failure::NO_DECODER,
                    "This PC cannot show a shared game yet.",
                ),
            );
            return;
        }
        let (Some(_), Some((endpoint, relay))) = (&self.tunnels, self.endpoint()) else {
            self.fail(
                id,
                Failure::new(Failure::OFFLINE, "The network is not ready yet."),
            );
            return;
        };
        let body = AcceptShare { endpoint, relay };
        self.spawn(
            async move { cloud.accept_share(&body).await },
            move |service, result| match result {
                Ok(share) => {
                    if service.share.invitations.remove(&invitation).is_some() {
                        service.event(control::Event::ShareInvitationGone { id: invitation });
                    }
                    service.share_cloud_changed(share);
                    service.ok(id, Done {});
                }
                Err(error) => {
                    let failure = service.failure(&error);
                    service.fail(id, failure);
                }
            },
        );
    }

    pub(super) fn share_mode(&mut self, id: u64, mode: ShareMode) {
        let host_of_active = self.share.share.as_ref().is_some_and(|s| {
            s.state == ShareState::Active && self.me.as_ref().is_some_and(|m| m.id == s.host.id)
        });
        match (&self.share.hub, host_of_active, mode) {
            (Some(hub), true, ShareMode::Watch | ShareMode::HandOver | ShareMode::Together) => {
                hub.set_mode(mode);
                self.share_update_remote(UpdateShare {
                    mode: Some(mode),
                    path: None,
                });
                if let Some(share) = &mut self.share.share {
                    share.mode = mode;
                }
                self.ok(id, Done {});
            }
            _ => self.fail(
                id,
                Failure::new(
                    Failure::BAD_STATE,
                    "Only the host of a running share changes its mode.",
                ),
            ),
        }
    }

    pub(super) fn share_request(&mut self, id: u64, mode: ShareMode) {
        let guest_of_active = self.share.share.as_ref().is_some_and(|s| {
            s.state == ShareState::Active && self.me.as_ref().is_some_and(|m| m.id == s.guest.id)
        });
        match (&self.share.hub, guest_of_active, mode) {
            (Some(hub), true, ShareMode::Watch | ShareMode::HandOver | ShareMode::Together) => {
                hub.request(mode);
                self.ok(id, Done {});
            }
            _ => self.fail(
                id,
                Failure::new(
                    Failure::BAD_STATE,
                    "Only the guest of a running share asks for a mode.",
                ),
            ),
        }
    }

    /// The host limits the picture of the share: the hub keeps the limits for the running share
    /// and the next ones, and the rate ladder stays under them.
    pub(super) fn share_quality(
        &mut self,
        id: u64,
        max_kbps: Option<u32>,
        max_height: Option<u32>,
    ) {
        match &self.share.hub {
            Some(hub) => {
                hub.quality(max_kbps, max_height);
                self.ok(id, Done {});
            }
            None => self.fail(
                id,
                Failure::new(Failure::OFFLINE, "The network is not ready yet."),
            ),
        }
    }

    pub(super) fn share_stop(&mut self, id: u64) {
        if self.share.share.is_some() {
            let host = self
                .share_peer()
                .is_some_and(|(role, _)| role == Role::Host);
            self.share_leave(if host { "ended" } else { "left" });
        }
        self.ok(id, Done {});
    }
}
