//! Where a device keeps its sign-in between runs: the system's credential store, or a file when
//! there is none (a Linux desktop without a Secret Service, or a test).

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// The part of a sign-in worth keeping: the device, its refresh token and the device key that
/// the refresh token is bound to. The key sits in the same entry, so it has the same protection
/// as the token: the system credential store, or an owner-only file.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Saved {
    pub device: u64,
    pub refresh: String,
    /// The secret of the device key (see `anyplay-proof`). Absent in sign-ins saved before
    /// device binding existed; the first refresh then makes one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
}

impl std::fmt::Debug for Saved {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Saved")
            .field("device", &self.device)
            .finish_non_exhaustive()
    }
}

/// What the net service keeps of a Discord connection: the refresh token, and whose it is. A
/// token kept for one AnyPlay account is never used for another.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedDiscord {
    pub account: anyplay_api::AccountId,
    pub refresh: String,
}

impl std::fmt::Debug for SavedDiscord {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SavedDiscord")
            .field("account", &self.account)
            .finish_non_exhaustive()
    }
}

/// One cloud's saved sign-in.
#[derive(Clone, Debug)]
pub enum Store {
    /// The system's credential store: Credential Manager on Windows, the Secret Service on
    /// Linux.
    System { user: String },
    /// A file that only its owner may read. On macOS `keychain` names the keychain item that
    /// older versions kept the sign-in in, which is read once to move it into the file.
    File {
        path: PathBuf,
        keychain: Option<String>,
    },
}

const SERVICE: &str = "AnyPlay";

/// Why no sign-in was found, for `network.log`.
#[derive(Debug, PartialEq, Eq)]
enum Missing {
    /// Nothing was ever kept (or a sign-out removed it).
    Nothing,
    /// The store could not be read: a keychain that denies this program, a locked Secret
    /// Service, a file that cannot be opened.
    Unreadable(String),
    /// Something was kept, but it is not a sign-in this version understands.
    Garbled(String),
}

impl Store {
    /// The store for the cloud at `cloud`: the system's when it works, else a file in `data`.
    ///
    /// On macOS it is always the file. A keychain item can be read only by the program that
    /// made it, and the net service is a new program (a new ad-hoc signature) with every
    /// launcher version, so the keychain would hand the sign-in to the version that saved it and
    /// to no later one: the player would sign in again after every update.
    pub fn open(data: &std::path::Path, cloud: &str, prefer_file: bool) -> Self {
        let store = Self::choose(data, cloud, prefer_file);
        match &store {
            Store::System { .. } => tracing::info!("the sign-in is kept in the system's store"),
            Store::File { path, .. } => {
                tracing::info!("the sign-in is kept in {}", path.display());
            }
        }
        store
    }

    fn choose(data: &std::path::Path, cloud: &str, prefer_file: bool) -> Self {
        let macos = cfg!(target_os = "macos");
        if !prefer_file && !macos {
            match keyring::Entry::store_status() {
                Ok(()) => {
                    return Store::System {
                        user: cloud.to_owned(),
                    };
                }
                Err(error) => {
                    tracing::warn!("no system credential store, using a file: {error}");
                }
            }
        }
        let name: String = cloud
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
            .take(80)
            .collect();
        Store::File {
            path: data.join(format!("signin-{name}.json")),
            keychain: (macos && !prefer_file).then(|| cloud.to_owned()),
        }
    }

    /// The kept sign-in. When there is none, `network.log` says why.
    pub fn load(&self) -> Option<Saved> {
        match self.read() {
            Ok(text) => match serde_json::from_str(&text) {
                Ok(saved) => Some(saved),
                Err(error) => {
                    self.report(&Missing::Garbled(error.to_string()));
                    None
                }
            },
            Err(missing) => {
                self.report(&missing);
                None
            }
        }
    }

    fn report(&self, missing: &Missing) {
        let place = match self {
            Store::System { .. } => "the system's credential store".to_owned(),
            Store::File { path, .. } => path.display().to_string(),
        };
        match missing {
            Missing::Nothing => tracing::info!("no saved sign-in in {place}"),
            Missing::Unreadable(why) => {
                tracing::warn!("the saved sign-in in {place} cannot be read: {why}");
            }
            Missing::Garbled(why) => {
                tracing::warn!("the saved sign-in in {place} is not usable: {why}");
            }
        }
    }

    fn read(&self) -> Result<String, Missing> {
        match self {
            Store::System { user } => read_system(user),
            Store::File { path, keychain } => match std::fs::read_to_string(path) {
                Ok(text) => Ok(text),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    let Some(user) = keychain else {
                        return Err(Missing::Nothing);
                    };
                    // A sign-in kept by an older version sits in the keychain. It is read once,
                    // and when this program may read it, moved to the file.
                    let text = read_system(user)?;
                    match write_private(path, &text) {
                        Ok(()) => tracing::info!("moved the saved sign-in out of the keychain"),
                        Err(error) => tracing::warn!("could not move the sign-in: {error}"),
                    }
                    Ok(text)
                }
                Err(error) => Err(Missing::Unreadable(error.to_string())),
            },
        }
    }

    /// The store of the Discord connection beside this one: its own entry or file, so a
    /// sign-in and a Discord refresh token never overwrite each other.
    pub fn discord(&self) -> Store {
        match self {
            Store::System { user } => Store::System {
                user: format!("discord:{user}"),
            },
            Store::File { path, keychain } => {
                let name = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("signin");
                let name = name.strip_prefix("signin-").unwrap_or(name);
                Store::File {
                    path: path.with_file_name(format!("discord-{name}")),
                    keychain: keychain.as_ref().map(|user| format!("discord:{user}")),
                }
            }
        }
    }

    pub fn load_discord(&self) -> Option<SavedDiscord> {
        match self.read() {
            Ok(text) => serde_json::from_str(&text).ok(),
            Err(Missing::Nothing) => None,
            Err(missing) => {
                self.report(&missing);
                None
            }
        }
    }

    pub fn save_discord(&self, saved: Option<&SavedDiscord>) {
        self.write(saved.map(|s| serde_json::to_string(s).unwrap_or_default()));
    }

    /// Keeps `saved`, or removes what was kept when it is `None`.
    pub fn save(&self, saved: Option<&Saved>) {
        self.write(saved.map(to_json));
    }

    fn write(&self, text: Option<String>) {
        let result = match (self, text) {
            (Store::System { user }, Some(text)) => keyring::Entry::new(SERVICE, user)
                .and_then(|entry| entry.set_password(&text))
                .map_err(|e| e.to_string()),
            (Store::System { user }, None) => forget_system(user),
            (Store::File { path, .. }, Some(text)) => write_private(path, &text),
            (Store::File { path, keychain }, None) => {
                let removed = match std::fs::remove_file(path) {
                    Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                        Err(error.to_string())
                    }
                    _ => Ok(()),
                };
                // A sign-out also removes what an older version left in the keychain, or the
                // next start would move it back into the file. A keychain that denies this
                // program is no failure here.
                if let Some(user) = keychain
                    && let Err(error) = forget_system(user)
                {
                    tracing::info!("the old keychain item stays: {error}");
                }
                removed
            }
        };
        if let Err(error) = result {
            tracing::warn!("could not keep the sign-in: {error}");
        }
    }
}

fn read_system(user: &str) -> Result<String, Missing> {
    match keyring::Entry::new(SERVICE, user).and_then(|entry| entry.get_password()) {
        Ok(text) => Ok(text),
        Err(keyring::Error::NoEntry) => Err(Missing::Nothing),
        Err(error) => Err(Missing::Unreadable(error.to_string())),
    }
}

fn forget_system(user: &str) -> Result<(), String> {
    keyring::Entry::new(SERVICE, user)
        .and_then(|entry| entry.delete_credential())
        .or_else(|error| match error {
            keyring::Error::NoEntry => Ok(()),
            other => Err(other),
        })
        .map_err(|e| e.to_string())
}

/// One thread applies the saves of a sign-in in the order they were queued, so a sign-out can
/// never be undone by an earlier save that happens to finish later.
#[derive(Clone)]
pub struct SaveQueue {
    jobs: std::sync::mpsc::Sender<Job>,
}

enum Job {
    Save(Option<Saved>),
    #[cfg(test)]
    Flush(std::sync::mpsc::Sender<()>),
}

/// Saves queued and not yet written, in every queue of the process.
static UNWRITTEN: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Waits until every queued save is written, at most `limit`. The service calls it before the
/// process ends: a refresh token that the cloud already replaced but that never reached the
/// store would sign the player out at the next start, and the launcher ends this process
/// right before it starts the updated one. Returns whether everything was written.
pub fn wait_saved(limit: std::time::Duration) -> bool {
    let end = std::time::Instant::now() + limit;
    while UNWRITTEN.load(std::sync::atomic::Ordering::SeqCst) > 0 {
        if std::time::Instant::now() >= end {
            tracing::warn!("a sign-in save did not finish before the service ended");
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    true
}

impl SaveQueue {
    pub fn new(store: Store) -> Self {
        let (jobs, rx) = std::sync::mpsc::channel::<Job>();
        std::thread::spawn(move || {
            for job in rx {
                match job {
                    Job::Save(saved) => {
                        store.save(saved.as_ref());
                        UNWRITTEN.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
                    }
                    #[cfg(test)]
                    Job::Flush(done) => {
                        let _ = done.send(());
                    }
                }
            }
        });
        SaveQueue { jobs }
    }

    pub fn save(&self, saved: Option<Saved>) {
        UNWRITTEN.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if self.jobs.send(Job::Save(saved)).is_err() {
            UNWRITTEN.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
        }
    }

    /// Returns once everything queued before this call is written.
    #[cfg(test)]
    pub fn flush(&self) {
        let (done, wait) = std::sync::mpsc::channel();
        if self.jobs.send(Job::Flush(done)).is_ok() {
            let _ = wait.recv();
        }
    }
}

fn to_json(saved: &Saved) -> String {
    serde_json::to_string(saved).unwrap_or_default()
}

/// Writes `text` to `path` through a temporary file, readable only by the owner on Unix. On
/// Windows the file inherits the per-user data folder's access rules.
pub(crate) fn write_private(path: &std::path::Path, text: &str) -> Result<(), String> {
    use std::io::Write;
    // Writers share the temporary name per path, so one at a time.
    static WRITING: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _writing = WRITING.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let partial = path.with_extension("partial");
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&partial).map_err(|e| e.to_string())?;
    file.write_all(text.as_bytes())
        .and_then(|()| file.sync_all())
        .map_err(|e| e.to_string())?;
    drop(file);
    std::fs::rename(&partial, path).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_store_keeps_and_forgets_a_sign_in() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path(), "https://cloud.example/", true);
        assert_eq!(store.load(), None);
        let saved = Saved {
            device: 9,
            refresh: "r1".into(),
            key: Some("k1".into()),
        };
        store.save(Some(&saved));
        assert_eq!(store.load(), Some(saved));
        store.save(None);
        assert_eq!(store.load(), None);
        store.save(None);
    }

    #[test]
    fn a_sign_in_saved_by_one_version_is_found_by_the_next() {
        let dir = tempfile::tempdir().unwrap();
        let cloud = "http://127.0.0.1:18080";
        // The old version refreshes right before the launcher ends it for the update.
        let old = Store::open(dir.path(), cloud, true);
        let queue = SaveQueue::new(old);
        queue.save(Some(Saved {
            device: 5,
            refresh: "rotated".into(),
            key: Some("k".into()),
        }));
        assert!(wait_saved(std::time::Duration::from_secs(5)));
        drop(queue);
        // The updated version opens the store again, from another program, with nothing but
        // the data folder and the cloud address in common.
        let new = Store::open(dir.path(), cloud, true);
        let found = new.load().expect("the sign-in is still there");
        assert_eq!((found.device, found.refresh.as_str()), (5, "rotated"));
    }

    #[test]
    fn a_missing_sign_in_is_told_apart_from_an_unusable_one() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path(), "https://a.example", true);
        assert_eq!(store.read().unwrap_err(), Missing::Nothing);
        let Store::File { path, .. } = &store else {
            panic!("a file store")
        };
        std::fs::write(path, "{nonsense").unwrap();
        assert!(store.read().is_ok());
        assert_eq!(store.load(), None);
        std::fs::remove_file(path).unwrap();
        std::fs::create_dir(path).unwrap();
        assert!(matches!(store.read(), Err(Missing::Unreadable(_))));
    }

    #[test]
    fn queued_saves_land_in_order_and_a_sign_out_stays() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path(), "https://cloud.example/", true);
        for round in 0..50 {
            let queue = SaveQueue::new(store.clone());
            for n in 0..20 {
                queue.save(Some(Saved {
                    device: n,
                    refresh: format!("r{round}-{n}"),
                    key: None,
                }));
            }
            queue.save(None);
            queue.flush();
            assert_eq!(store.load(), None, "round {round}");
            queue.save(Some(Saved {
                device: 1,
                refresh: "last".into(),
                key: None,
            }));
            queue.flush();
            assert_eq!(store.load().map(|s| s.refresh), Some("last".to_owned()));
            queue.save(None);
            queue.flush();
        }
    }

    #[test]
    fn concurrent_private_writes_never_fail_or_mix() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.json");
        let threads: Vec<_> = (0..8)
            .map(|t| {
                let path = path.clone();
                std::thread::spawn(move || {
                    for n in 0..30 {
                        write_private(&path, &format!("{t}-{n}")).unwrap();
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains('-') && text.len() <= 4, "{text}");
    }

    #[test]
    fn a_sign_in_saved_before_device_keys_still_loads() {
        let saved: Saved = serde_json::from_str(r#"{"device":9,"refresh":"r1"}"#).unwrap();
        assert_eq!(saved.key, None);
    }

    #[test]
    fn debug_output_never_shows_secrets() {
        let saved = Saved {
            device: 1,
            refresh: "secret-refresh".into(),
            key: Some("secret-key".into()),
        };
        let text = format!("{saved:?}");
        assert!(!text.contains("secret"), "{text}");
    }

    #[test]
    fn the_discord_token_has_its_own_slot_and_never_prints() {
        let dir = tempfile::tempdir().unwrap();
        let signin = Store::open(dir.path(), "https://a.example", true);
        let discord = signin.discord();
        assert_eq!(discord.load_discord(), None);
        let saved = SavedDiscord {
            account: anyplay_api::AccountId(7),
            refresh: "secret-refresh".into(),
        };
        discord.save_discord(Some(&saved));
        assert_eq!(discord.load_discord(), Some(saved.clone()));
        assert_eq!(signin.load(), None, "the sign-in slot is untouched");
        assert!(!format!("{saved:?}").contains("secret"));
        discord.save_discord(None);
        assert_eq!(discord.load_discord(), None);
    }

    #[test]
    fn every_cloud_has_its_own_file() {
        let dir = tempfile::tempdir().unwrap();
        let a = Store::open(dir.path(), "https://a.example", true);
        let b = Store::open(dir.path(), "https://b.example", true);
        a.save(Some(&Saved {
            device: 1,
            refresh: "a".into(),
            key: None,
        }));
        assert_eq!(b.load(), None);
    }
}
