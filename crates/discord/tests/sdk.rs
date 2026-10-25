//! Loads the real library. Needs the `sdk` feature and DISCORD_SDK_DIR at run time too; without
//! the variable the tests end at once, so a machine without the SDK stays green.
#![cfg(feature = "sdk")]

use std::path::PathBuf;
use std::time::Duration;

use anyplay_discord::{APPLICATION_ID, Error, Event, Sdk, Snapshot, Social, State};
use tokio::sync::mpsc::unbounded_channel;

fn library() -> Option<PathBuf> {
    let dir = PathBuf::from(std::env::var_os("DISCORD_SDK_DIR")?);
    Some(anyplay_discord::library_in_sdk_dir(&dir))
}

#[test]
fn the_library_loads_with_every_symbol_and_starts_signed_out() {
    let Some(library) = library() else {
        eprintln!("DISCORD_SDK_DIR is not set; skipped");
        return;
    };
    let (tx, _rx) = unbounded_channel();
    let sdk = Sdk::load(&library, APPLICATION_ID, tx).expect("the library loads");
    assert_eq!(sdk.snapshot(), Snapshot::new(State::SignedOut));
}

#[test]
fn a_missing_or_relative_path_is_refused_without_a_search() {
    let (tx, _rx) = unbounded_channel();
    let missing = Sdk::load(
        &std::env::temp_dir().join("no-such-discord-library.dll"),
        APPLICATION_ID,
        tx.clone(),
    );
    assert!(matches!(missing, Err(Error::Sdk(_))));
    let relative = Sdk::load(
        std::path::Path::new("discord_partner_sdk.dll"),
        APPLICATION_ID,
        tx,
    );
    assert!(matches!(relative, Err(Error::Sdk(_))));
}

/// Talks to discord.com with a token that cannot be valid, to see that calls go out and the
/// callbacks come back through the pump. `cargo test -- --ignored`.
#[test]
#[ignore = "talks to discord.com"]
fn a_bogus_token_is_refused_by_discord_and_reported() {
    let Some(library) = library() else { return };
    let (tx, mut rx) = unbounded_channel();
    let mut sdk = Sdk::load(&library, APPLICATION_ID, tx).unwrap();
    sdk.connect("not-a-real-token").unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    let mut failed = false;
    while std::time::Instant::now() < deadline && !failed {
        while let Ok(event) = rx.try_recv() {
            eprintln!("event: {event:?}");
            failed |= matches!(event, Event::Failed(_));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(failed, "no failure reported");
    assert_eq!(sdk.snapshot().state, State::SignedOut);
}
