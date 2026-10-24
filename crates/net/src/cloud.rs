//! The cloud API as a client: one method per request, access tokens refreshed on the way.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyplay_api::{
    AccountId, AccountTrophies, AddToConversation, Added, ApiError, Cards, CodeAnswer, CodeRequest,
    Conversation, Conversations, CreateConversation, CreateSession, Device, EmailAnswer,
    EmailRequest, ErrorCode, FriendLists, FriendRequest, FriendRequestAnswer, Info, Invite,
    Invited, JoinSession, MarkRead, Me, Message, Messages, MoveParty, Party, PartyInvitations,
    PlaytimeAdded, PlaytimeEntry, PlaytimeUpload, PresenceRequest, Privacy, Profile, Rarity,
    RefreshRequest, Relays, Report, SendMessage, Session, SignedIn, SignupRequest, TicketRequest,
    Tokens, TrophiesAdded, Trophy, TrophyUpload, UpdateParty, UpdateSession, path,
};
use reqwest::{Method, StatusCode};
use serde::Serialize;
use serde::de::DeserializeOwned;

/// Why a request to the cloud failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CloudError {
    /// The cloud answered with an error.
    Api(ApiError),
    /// The cloud could not be reached or answered something unreadable.
    Offline(String),
    /// There are no tokens, or the cloud refused them for good.
    SignedOut,
}

impl std::fmt::Display for CloudError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CloudError::Api(error) => error.fmt(f),
            CloudError::Offline(reason) => write!(f, "cloud unreachable: {reason}"),
            CloudError::SignedOut => f.write_str("not signed in"),
        }
    }
}

impl CloudError {
    pub fn is(&self, code: ErrorCode) -> bool {
        matches!(self, CloudError::Api(error) if error.error == code)
    }
}

/// Called with the new tokens after every sign-in and refresh, and with `None` when the cloud
/// signed the device out, so they can be kept across restarts.
pub type TokenSink = Arc<dyn Fn(Option<&Tokens>) + Send + Sync>;

/// A cloud at one base URL. Cheap to clone; clones share their tokens.
#[derive(Clone)]
pub struct Cloud {
    base: String,
    http: reqwest::Client,
    tokens: Arc<Mutex<Option<Tokens>>>,
    refreshing: Arc<tokio::sync::Mutex<()>>,
    sink: TokenSink,
}

impl Cloud {
    pub fn new(base: &str, sink: TokenSink) -> Self {
        let http = reqwest::Client::builder()
            .user_agent(concat!("anyplay-net/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30))
            .build()
            .unwrap_or_default();
        Self {
            base: base.trim_end_matches('/').to_owned(),
            http,
            tokens: Arc::new(Mutex::new(None)),
            refreshing: Arc::new(tokio::sync::Mutex::new(())),
            sink,
        }
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    /// The URL of the event connection: the base with `ws` in place of `http`.
    pub fn events_url(&self) -> String {
        let base = if let Some(rest) = self.base.strip_prefix("https://") {
            format!("wss://{rest}")
        } else if let Some(rest) = self.base.strip_prefix("http://") {
            format!("ws://{rest}")
        } else {
            self.base.clone()
        };
        format!("{base}{}", path::EVENTS)
    }

    /// Takes tokens kept from an earlier run. The access token may be stale; the first request
    /// refreshes it.
    pub fn resume(&self, device: u64, refresh: String) {
        *self.lock() = Some(Tokens {
            access: String::new(),
            access_expires: 0,
            refresh,
            device,
        });
    }

    pub fn is_signed_in(&self) -> bool {
        self.lock().is_some()
    }

    /// The current access token, refreshed first when it is about to expire.
    pub async fn access(&self) -> Result<String, CloudError> {
        let fresh = {
            let tokens = self.lock();
            let tokens = tokens.as_ref().ok_or(CloudError::SignedOut)?;
            (tokens.access_expires > now() + 30 && !tokens.access.is_empty())
                .then(|| tokens.access.clone())
        };
        match fresh {
            Some(access) => Ok(access),
            None => self.refresh().await,
        }
    }

    /// Forgets the tokens, as after a sign-out.
    pub fn forget(&self) {
        *self.lock() = None;
        (self.sink)(None);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Option<Tokens>> {
        self.tokens.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn adopt(&self, tokens: Tokens) {
        (self.sink)(Some(&tokens));
        *self.lock() = Some(tokens);
    }

    /// Replaces the access token. Only one refresh runs at a time; callers that waited behind
    /// it use its result.
    pub async fn refresh(&self) -> Result<String, CloudError> {
        let before = self.lock().as_ref().map(|t| t.refresh.clone());
        let _guard = self.refreshing.lock().await;
        let (refresh, current) = {
            let tokens = self.lock();
            let tokens = tokens.as_ref().ok_or(CloudError::SignedOut)?;
            (tokens.refresh.clone(), tokens.access.clone())
        };
        if before.as_deref() != Some(refresh.as_str()) && !current.is_empty() {
            return Ok(current);
        }
        let answer: Result<Tokens, CloudError> = self
            .send(
                Method::POST,
                path::AUTH_REFRESH,
                Some(&RefreshRequest { refresh }),
                None,
            )
            .await;
        match answer {
            Ok(tokens) => {
                let access = tokens.access.clone();
                self.adopt(tokens);
                Ok(access)
            }
            Err(CloudError::Api(error))
                if matches!(error.error, ErrorCode::Unauthorized | ErrorCode::Banned) =>
            {
                self.forget();
                Err(if error.error == ErrorCode::Banned {
                    CloudError::Api(error)
                } else {
                    CloudError::SignedOut
                })
            }
            Err(error) => Err(error),
        }
    }

    async fn send<B: Serialize, R: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        body: Option<&B>,
        access: Option<&str>,
    ) -> Result<R, CloudError> {
        let mut request = self.http.request(method, format!("{}{path}", self.base));
        if let Some(access) = access {
            request = request.bearer_auth(access);
        }
        if let Some(body) = body {
            request = request.json(body);
        }
        let response = request
            .send()
            .await
            .map_err(|e| CloudError::Offline(e.without_url().to_string()))?;
        let status = response.status();
        let bytes = response
            .bytes()
            .await
            .map_err(|e| CloudError::Offline(e.without_url().to_string()))?;
        if status.is_success() {
            let bytes = if status == StatusCode::NO_CONTENT || bytes.is_empty() {
                &b"null"[..]
            } else {
                &bytes[..]
            };
            return serde_json::from_slice(bytes)
                .map_err(|e| CloudError::Offline(format!("unreadable answer: {e}")));
        }
        match serde_json::from_slice::<ApiError>(&bytes) {
            Ok(error) => Err(CloudError::Api(error)),
            Err(_) => Err(CloudError::Offline(format!("status {status}"))),
        }
    }

    /// A request on behalf of the signed-in account. An expired access token is refreshed once.
    async fn call<B: Serialize, R: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        body: Option<&B>,
    ) -> Result<R, CloudError> {
        let access = self.access().await?;
        match self.send(method.clone(), path, body, Some(&access)).await {
            Err(CloudError::Api(error)) if error.error == ErrorCode::Unauthorized => {
                if let Some(tokens) = self.lock().as_mut() {
                    tokens.access.clear();
                }
                let access = self.refresh().await?;
                self.send(method, path, body, Some(&access)).await
            }
            other => other,
        }
    }

    pub async fn info(&self) -> Result<Info, CloudError> {
        self.send::<(), _>(Method::GET, path::INFO, None, None)
            .await
    }

    pub async fn email(&self, email: &str) -> Result<EmailAnswer, CloudError> {
        let body = EmailRequest {
            email: email.to_owned(),
        };
        self.send(Method::POST, path::AUTH_EMAIL, Some(&body), None)
            .await
    }

    /// Sends the code. A known account is signed in right away.
    pub async fn code(
        &self,
        challenge: &str,
        code: &str,
        device: Device,
    ) -> Result<CodeAnswer, CloudError> {
        let body = CodeRequest {
            challenge: challenge.to_owned(),
            code: code.to_owned(),
            device,
        };
        let answer: CodeAnswer = self
            .send(Method::POST, path::AUTH_CODE, Some(&body), None)
            .await?;
        if let CodeAnswer::SignedIn { tokens, .. } = &answer {
            self.adopt(tokens.clone());
        }
        Ok(answer)
    }

    pub async fn signup(
        &self,
        signup: &str,
        handle: &str,
        device: Device,
    ) -> Result<SignedIn, CloudError> {
        let body = SignupRequest {
            signup: signup.to_owned(),
            handle: handle.to_owned(),
            age_confirmed: true,
            device,
        };
        let answer: SignedIn = self
            .send(Method::POST, path::AUTH_SIGNUP, Some(&body), None)
            .await?;
        self.adopt(answer.tokens.clone());
        Ok(answer)
    }

    /// Signs this device out. The tokens are forgotten even when the cloud cannot be reached.
    pub async fn signout(&self) -> Result<(), CloudError> {
        let result = self
            .call::<(), serde_json::Value>(Method::POST, path::AUTH_SIGNOUT, None)
            .await
            .map(drop);
        self.forget();
        result
    }

    pub async fn me(&self) -> Result<Me, CloudError> {
        self.call::<(), _>(Method::GET, path::ME, None).await
    }

    pub async fn delete_me(&self) -> Result<(), CloudError> {
        self.call::<(), serde_json::Value>(Method::DELETE, path::ME, None)
            .await?;
        self.forget();
        Ok(())
    }

    pub async fn friends(&self) -> Result<FriendLists, CloudError> {
        self.call::<(), _>(Method::GET, path::FRIENDS, None).await
    }

    pub async fn add_friend(&self, handle: &str) -> Result<FriendRequestAnswer, CloudError> {
        let body = FriendRequest {
            handle: handle.to_owned(),
        };
        self.call(Method::POST, path::FRIEND_REQUESTS, Some(&body))
            .await
    }

    pub async fn accept(&self, account: AccountId) -> Result<(), CloudError> {
        self.empty(
            Method::POST,
            &path::fill(path::FRIEND_REQUEST_ACCEPT, account),
        )
        .await
    }

    pub async fn decline(&self, account: AccountId) -> Result<(), CloudError> {
        self.empty(Method::DELETE, &path::fill(path::FRIEND_REQUEST, account))
            .await
    }

    pub async fn remove_friend(&self, account: AccountId) -> Result<(), CloudError> {
        self.empty(Method::DELETE, &path::fill(path::FRIEND, account))
            .await
    }

    pub async fn block(&self, account: AccountId) -> Result<(), CloudError> {
        self.empty(Method::PUT, &path::fill(path::BLOCK, account))
            .await
    }

    pub async fn unblock(&self, account: AccountId) -> Result<(), CloudError> {
        self.empty(Method::DELETE, &path::fill(path::BLOCK, account))
            .await
    }

    pub async fn report(&self, report: &Report) -> Result<(), CloudError> {
        self.call::<_, serde_json::Value>(Method::POST, path::REPORTS, Some(report))
            .await
            .map(drop)
    }

    pub async fn presence(&self, body: &PresenceRequest) -> Result<(), CloudError> {
        self.call::<_, serde_json::Value>(Method::PUT, path::PRESENCE, Some(body))
            .await
            .map(drop)
    }

    pub async fn create_session(&self, body: &CreateSession) -> Result<Session, CloudError> {
        self.call(Method::POST, path::SESSIONS, Some(body)).await
    }

    pub async fn session(&self, id: &str) -> Result<Session, CloudError> {
        self.call::<(), _>(Method::GET, &path::fill(path::SESSION, id), None)
            .await
    }

    pub async fn join(&self, id: &str, body: &JoinSession) -> Result<Session, CloudError> {
        self.call(
            Method::POST,
            &path::fill(path::SESSION_JOIN, id),
            Some(body),
        )
        .await
    }

    pub async fn leave(&self, id: &str) -> Result<(), CloudError> {
        self.empty(Method::POST, &path::fill(path::SESSION_LEAVE, id))
            .await
    }

    pub async fn update_session(
        &self,
        id: &str,
        body: &UpdateSession,
    ) -> Result<Session, CloudError> {
        self.call(Method::PATCH, &path::fill(path::SESSION, id), Some(body))
            .await
    }

    /// Records trophies the player earned in a title. `title_id` goes into the path, so it must
    /// pass [`anyplay_api::is_title_id`].
    pub async fn upload_trophies(
        &self,
        title_id: &str,
        trophies: Vec<Trophy>,
    ) -> Result<TrophiesAdded, CloudError> {
        self.call(
            Method::PUT,
            &path::fill(path::MY_TROPHIES, title_id),
            Some(&TrophyUpload { trophies }),
        )
        .await
    }

    pub async fn trophies(&self, account: AccountId) -> Result<AccountTrophies, CloudError> {
        self.call::<(), _>(
            Method::GET,
            &path::fill(path::ACCOUNT_TROPHIES, account),
            None,
        )
        .await
    }

    /// How many players earned each trophy of a title. `title_id` must pass
    /// [`anyplay_api::is_title_id`].
    pub async fn rarity(&self, title_id: &str) -> Result<Rarity, CloudError> {
        self.call::<(), _>(Method::GET, &path::fill(path::RARITY, title_id), None)
            .await
    }

    pub async fn cards(&self, accounts: &[AccountId]) -> Result<Cards, CloudError> {
        let list: Vec<String> = accounts.iter().map(|a| a.to_string()).collect();
        let path = format!("{}?accounts={}", path::CARDS, list.join(","));
        self.call::<(), _>(Method::GET, &path, None).await
    }

    pub async fn privacy(&self) -> Result<Privacy, CloudError> {
        self.call::<(), _>(Method::GET, path::PRIVACY, None).await
    }

    pub async fn set_privacy(&self, privacy: &Privacy) -> Result<Privacy, CloudError> {
        self.call(Method::PUT, path::PRIVACY, Some(privacy)).await
    }

    /// The profile of an account, with the parts the caller may see.
    pub async fn profile(&self, account: AccountId) -> Result<Profile, CloudError> {
        self.call::<(), _>(
            Method::GET,
            &path::fill(path::ACCOUNT_PROFILE, account),
            None,
        )
        .await
    }

    /// Sends minutes played per title and day. The cloud keeps the highest number it has seen for
    /// a day, so sending the same entries again is safe.
    pub async fn upload_playtime(
        &self,
        entries: Vec<PlaytimeEntry>,
    ) -> Result<PlaytimeAdded, CloudError> {
        self.call(
            Method::PUT,
            path::MY_PLAYTIME,
            Some(&PlaytimeUpload { entries }),
        )
        .await
    }

    pub async fn invite(&self, id: &str, to: Vec<AccountId>) -> Result<Invited, CloudError> {
        self.call(
            Method::POST,
            &path::fill(path::SESSION_INVITATIONS, id),
            Some(&Invite { to }),
        )
        .await
    }

    pub async fn decline_invitation(&self, id: &str) -> Result<(), CloudError> {
        self.empty(Method::DELETE, &path::fill(path::INVITATION, id))
            .await
    }

    pub async fn relay_tickets(&self, endpoint: &str) -> Result<Relays, CloudError> {
        let body = TicketRequest {
            endpoint: endpoint.to_owned(),
        };
        self.call(Method::POST, path::RELAY_TICKETS, Some(&body))
            .await
    }

    // Parties

    /// The party the player is in, or none.
    pub async fn party(&self) -> Result<Option<Party>, CloudError> {
        match self.call::<(), _>(Method::GET, path::PARTY, None).await {
            Ok(party) => Ok(Some(party)),
            Err(error) if error.is(ErrorCode::NotFound) => Ok(None),
            Err(error) => Err(error),
        }
    }

    pub async fn create_party(&self) -> Result<Party, CloudError> {
        self.call::<(), _>(Method::POST, path::PARTY, None).await
    }

    /// Invites friends; the cloud makes a party when the player is in none.
    pub async fn invite_to_party(&self, to: Vec<AccountId>) -> Result<Invited, CloudError> {
        self.call(Method::POST, path::PARTY_INVITATIONS, Some(&Invite { to }))
            .await
    }

    pub async fn party_invitations(&self) -> Result<PartyInvitations, CloudError> {
        self.call::<(), _>(Method::GET, path::PARTY_INVITATIONS, None)
            .await
    }

    pub async fn accept_party_invitation(&self, id: &str) -> Result<Party, CloudError> {
        self.call::<(), _>(
            Method::POST,
            &path::fill(path::PARTY_INVITATION_ACCEPT, id),
            None,
        )
        .await
    }

    pub async fn decline_party_invitation(&self, id: &str) -> Result<(), CloudError> {
        self.empty(Method::DELETE, &path::fill(path::PARTY_INVITATION, id))
            .await
    }

    pub async fn leave_party(&self) -> Result<(), CloudError> {
        self.empty(Method::POST, path::PARTY_LEAVE).await
    }

    pub async fn kick_from_party(&self, account: AccountId) -> Result<(), CloudError> {
        self.empty(Method::DELETE, &path::fill(path::PARTY_MEMBER, account))
            .await
    }

    pub async fn update_party(&self, body: &UpdateParty) -> Result<Party, CloudError> {
        self.call(Method::PATCH, path::PARTY, Some(body)).await
    }

    /// Invites the party into a session of the leader's.
    pub async fn move_party(&self, session: &str) -> Result<Invited, CloudError> {
        let body = MoveParty {
            session: session.to_owned(),
        };
        self.call(Method::POST, path::PARTY_SESSION, Some(&body))
            .await
    }

    // Conversations and messages

    pub async fn conversations(&self) -> Result<Conversations, CloudError> {
        self.call::<(), _>(Method::GET, path::CONVERSATIONS, None)
            .await
    }

    pub async fn open_conversation(
        &self,
        accounts: Vec<AccountId>,
        name: Option<String>,
    ) -> Result<Conversation, CloudError> {
        self.call(
            Method::POST,
            path::CONVERSATIONS,
            Some(&CreateConversation { accounts, name }),
        )
        .await
    }

    /// Messages after `after`, before `before` or the newest. `id` is a conversation ID from the
    /// cloud, which only holds URL-safe characters.
    pub async fn messages(
        &self,
        id: &str,
        after: Option<u64>,
        before: Option<u64>,
        limit: Option<u32>,
    ) -> Result<Messages, CloudError> {
        let mut query = Vec::new();
        if let Some(after) = after {
            query.push(format!("after={after}"));
        }
        if let Some(before) = before {
            query.push(format!("before={before}"));
        }
        if let Some(limit) = limit {
            query.push(format!("limit={limit}"));
        }
        let mut path = path::fill(path::CONVERSATION_MESSAGES, id);
        if !query.is_empty() {
            path = format!("{path}?{}", query.join("&"));
        }
        self.call::<(), _>(Method::GET, &path, None).await
    }

    pub async fn send_message(&self, id: &str, text: &str) -> Result<Message, CloudError> {
        let body = SendMessage {
            text: text.to_owned(),
        };
        self.call(
            Method::POST,
            &path::fill(path::CONVERSATION_MESSAGES, id),
            Some(&body),
        )
        .await
    }

    pub async fn mark_read(&self, id: &str, seq: u64) -> Result<(), CloudError> {
        self.call::<_, serde_json::Value>(
            Method::PUT,
            &path::fill(path::CONVERSATION_READ, id),
            Some(&MarkRead { seq }),
        )
        .await
        .map(drop)
    }

    pub async fn accept_conversation(&self, id: &str) -> Result<Conversation, CloudError> {
        self.call::<(), _>(
            Method::POST,
            &path::fill(path::CONVERSATION_ACCEPT, id),
            None,
        )
        .await
    }

    pub async fn leave_conversation(&self, id: &str) -> Result<(), CloudError> {
        self.empty(Method::DELETE, &path::fill(path::CONVERSATION, id))
            .await
    }

    pub async fn add_to_conversation(
        &self,
        id: &str,
        accounts: Vec<AccountId>,
    ) -> Result<Added, CloudError> {
        self.call(
            Method::POST,
            &path::fill(path::CONVERSATION_MEMBERS, id),
            Some(&AddToConversation { accounts }),
        )
        .await
    }

    async fn empty(&self, method: Method, path: &str) -> Result<(), CloudError> {
        self.call::<(), serde_json::Value>(method, path, None)
            .await
            .map(drop)
    }
}

/// Unix seconds now.
pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
