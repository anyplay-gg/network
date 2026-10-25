//! Discord's Social SDK behind a seam ([ADR 0022](../../../docs/adr/0022-discord-social-sdk.md)).
//!
//! The net service talks to [`Social`] and never to the SDK. There are three things behind it:
//!
//! - [`Off`]: no Discord. This is what a build without the SDK, or a machine where the library
//!   is missing or refuses to load, runs. The state is [`State::Unavailable`] and nothing else
//!   changes.
//! - [`Fake`]: what CI and every test use. It talks to nobody.
//! - `Sdk` (feature `sdk`): the real library, loaded at run time from an absolute path. Its
//!   bindings are made at build time from a header that is not in this repository.
//!
//! Everything the SDK does happens on one thread of its own, and what it reports comes back as
//! [`Event`]s on a channel, so a caller never waits on Discord and never runs the SDK's callbacks.
//!
//! A [`Social`] holds no token of its own beyond the access token it was given for the length of
//! the connection. Keeping the refresh token, and asking the cloud for tokens, is the caller's job.

mod fake;
#[cfg(feature = "sdk")]
mod sdk;

use std::fmt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::UnboundedSender;

pub use fake::{Fake, FakeScript, Seen};
#[cfg(feature = "sdk")]
pub use sdk::Sdk;

/// The Discord application of AnyPlay. The ID is public.
pub const APPLICATION_ID: u64 = 1_563_714_917_998_133_288;

/// The scopes the net service asks for: Discord's own for presence and the friend list, and
/// `identify`, which the cloud needs to check that the account is the one that was linked.
pub const SCOPES: &str = "openid sdk.social_layer_presence identify";

/// Where the player is with Discord.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    /// There is no SDK in this build or it could not be loaded.
    Unavailable,
    /// The SDK is there and the player has not connected (or has disconnected).
    SignedOut,
    /// Authorising, trading the code, or connecting.
    Connecting,
    /// The SDK reports ready: the current user and the friend list are known.
    Connected,
}

/// The player's Discord account, as the SDK shows it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct User {
    /// The ID as a decimal string: a Discord ID does not fit the 53 bits a JSON number keeps.
    pub id: String,
    /// The display name, not the username.
    pub name: String,
}

/// What can be known about the connection at one moment.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    pub state: State,
    /// The player, while connected.
    pub user: Option<User>,
    /// The number of Discord friends, while connected. The friends themselves are not read in
    /// phase 0.
    pub friends: Option<u32>,
}

impl Snapshot {
    pub fn new(state: State) -> Self {
        Self {
            state,
            user: None,
            friends: None,
        }
    }
}

/// What the SDK's authorisation gave: the one-time code, and the PKCE verifier of the challenge
/// the authorisation was started with. The cloud trades both for tokens.
#[derive(Clone, PartialEq, Eq)]
pub struct Authorization {
    pub code: String,
    pub verifier: String,
    pub redirect_uri: String,
}

impl fmt::Debug for Authorization {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Authorization")
            .field("redirect_uri", &self.redirect_uri)
            .finish_non_exhaustive()
    }
}

/// Something the SDK reported.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// The snapshot changed.
    Changed(Snapshot),
    /// The player agreed in Discord's overlay or browser.
    Authorized(Authorization),
    /// The authorisation ended without a code: the player closed it, or it failed. Never holds
    /// a secret.
    AuthorizationFailed(String),
    /// Discord refused the access token, or the connection failed. Never holds a secret.
    Failed(String),
}

/// Why a call on [`Social`] failed at once. Asynchronous failures come as [`Event`]s.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// This is [`Off`], or the library did not load.
    Unavailable,
    /// The call does not fit the state, such as connecting twice.
    Busy,
    /// The library failed in a way it describes.
    Sdk(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Unavailable => f.write_str("Discord is not available in this build."),
            Error::Busy => f.write_str("Discord is busy with an earlier request."),
            Error::Sdk(why) => write!(f, "Discord's library failed: {why}"),
        }
    }
}

impl std::error::Error for Error {}

/// Discord's Social SDK as the net service uses it.
///
/// A `Social` is made with the sender its [`Event`]s go to. The calls return at once; results
/// arrive as events.
pub trait Social: Send {
    /// What is known now.
    fn snapshot(&self) -> Snapshot;

    /// Starts the SDK's authorisation with [`SCOPES`] and a fresh PKCE challenge. Discord asks
    /// the player (the overlay, the app or the browser) and [`Event::Authorized`] follows, or
    /// [`Event::AuthorizationFailed`]. The state becomes [`State::Connecting`].
    fn authorize(&mut self) -> Result<(), Error>;

    /// Gives up an authorisation that waits for the player.
    fn abort_authorize(&mut self);

    /// Connects with an access token the cloud traded for. The state is [`State::Connecting`]
    /// until the SDK is ready, and [`Event::Failed`] follows if Discord refuses.
    fn connect(&mut self, access_token: &str) -> Result<(), Error>;

    /// Ends the connection and forgets what was known about the player and the friends.
    fn disconnect(&mut self);
}

/// No Discord: [`State::Unavailable`] and an error for every request.
#[derive(Debug, Default)]
pub struct Off;

impl Social for Off {
    fn snapshot(&self) -> Snapshot {
        Snapshot::new(State::Unavailable)
    }

    fn authorize(&mut self) -> Result<(), Error> {
        Err(Error::Unavailable)
    }

    fn abort_authorize(&mut self) {}

    fn connect(&mut self, _: &str) -> Result<(), Error> {
        Err(Error::Unavailable)
    }

    fn disconnect(&mut self) {}
}

/// How a net service gets its [`Social`].
#[derive(Clone, Debug, Default)]
pub enum Backend {
    /// No Discord.
    #[default]
    Off,
    /// The fake, for tests.
    Fake(FakeScript),
    /// The real library at this absolute path. Without the `sdk` feature this is [`Off`].
    Sdk { library: PathBuf },
}

impl Backend {
    /// Makes the [`Social`] this backend names, which reports to `events`. A library that cannot
    /// be loaded gives [`Off`], and the reason is logged.
    pub fn open(&self, events: UnboundedSender<Event>) -> Box<dyn Social> {
        match self {
            Backend::Off => Box::new(Off),
            Backend::Fake(script) => Box::new(Fake::new(script.clone(), events)),
            #[cfg(feature = "sdk")]
            Backend::Sdk { library } => match Sdk::load(library, APPLICATION_ID, events) {
                Ok(sdk) => Box::new(sdk),
                Err(error) => {
                    tracing::warn!("Discord is off: {error}");
                    Box::new(Off)
                }
            },
            #[cfg(not(feature = "sdk"))]
            Backend::Sdk { library } => {
                tracing::info!(
                    "Discord is off: this build has no Discord SDK support ({})",
                    library.display()
                );
                Box::new(Off)
            }
        }
    }
}

/// The file name of the SDK's shared library on this platform.
pub fn library_name() -> &'static str {
    if cfg!(windows) {
        "discord_partner_sdk.dll"
    } else if cfg!(target_os = "macos") {
        "libdiscord_partner_sdk.dylib"
    } else {
        "libdiscord_partner_sdk.so"
    }
}

/// Where the library sits in the SDK's own folder layout (`bin/release` on Windows,
/// `lib/release` elsewhere). For development; a release has it next to the net service.
pub fn library_in_sdk_dir(sdk_dir: &Path) -> PathBuf {
    let folder = if cfg!(windows) { "bin" } else { "lib" };
    sdk_dir.join(folder).join("release").join(library_name())
}

/// The library next to the executable, which is the only place a release loads it from.
pub fn library_next_to(executable: &Path) -> Option<PathBuf> {
    Some(executable.parent()?.join(library_name()))
}

/// Removes what must not reach a log from a line the SDK wrote: long numbers (Discord IDs) and
/// long runs of token characters.
pub fn scrub(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut run = String::new();
    let flush = |run: &mut String, out: &mut String| {
        if run.len() >= 16 {
            out.push_str("<hidden>");
        } else {
            out.push_str(run);
        }
        run.clear();
    };
    for c in line.chars() {
        if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '=') {
            run.push(c);
        } else {
            flush(&mut run, &mut out);
            out.push(c);
        }
    }
    flush(&mut run, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn off_is_unavailable_and_refuses_everything() {
        let mut off = Off;
        assert_eq!(off.snapshot(), Snapshot::new(State::Unavailable));
        assert_eq!(off.authorize(), Err(Error::Unavailable));
        assert_eq!(off.connect("token"), Err(Error::Unavailable));
        off.disconnect();
        assert_eq!(off.snapshot().state, State::Unavailable);
    }

    #[test]
    fn the_state_has_the_names_of_the_protocol() {
        let names: Vec<String> = [
            State::Unavailable,
            State::SignedOut,
            State::Connecting,
            State::Connected,
        ]
        .iter()
        .map(|s| serde_json::to_string(s).unwrap())
        .collect();
        assert_eq!(
            names,
            [
                "\"unavailable\"",
                "\"signed_out\"",
                "\"connecting\"",
                "\"connected\""
            ]
        );
    }

    #[test]
    fn an_authorization_never_prints_its_secrets() {
        let text = format!(
            "{:?}",
            Authorization {
                code: "code-secret".into(),
                verifier: "verifier-secret".into(),
                redirect_uri: "http://127.0.0.1/callback".into(),
            }
        );
        assert!(!text.contains("secret"), "{text}");
    }

    #[test]
    fn the_scrub_hides_ids_and_tokens_but_not_words() {
        let line =
            "user 1563714917998133288 logged in with MTIzNDU2Nzg5MDEyMzQ1Njc4.token_part, ok";
        let clean = scrub(line);
        assert_eq!(clean, "user <hidden> logged in with <hidden>, ok");
        assert_eq!(
            scrub("connection failed (code 4)"),
            "connection failed (code 4)"
        );
    }

    #[test]
    fn a_backend_without_the_library_opens_off() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let social = Backend::Sdk {
            library: PathBuf::from("/nonexistent/lib"),
        }
        .open(tx);
        assert_eq!(social.snapshot().state, State::Unavailable);
    }
}
