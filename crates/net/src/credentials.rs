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
    /// Linux, the keychain on macOS.
    System { user: String },
    /// A file that only its owner may read.
    File(PathBuf),
}

const SERVICE: &str = "AnyPlay";

impl Store {
    /// The store for the cloud at `cloud`: the system's when it works, else a file in `data`.
    pub fn open(data: &std::path::Path, cloud: &str, prefer_file: bool) -> Self {
        if !prefer_file && keyring::Entry::store_status().is_ok() {
            return Store::System {
                user: cloud.to_owned(),
            };
        }
        let name: String = cloud
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
            .take(80)
            .collect();
        Store::File(data.join(format!("signin-{name}.json")))
    }

    pub fn load(&self) -> Option<Saved> {
        serde_json::from_str(&self.read()?).ok()
    }

    fn read(&self) -> Option<String> {
        match self {
            Store::System { user } => keyring::Entry::new(SERVICE, user)
                .and_then(|entry| entry.get_password())
                .map_err(|error| {
                    if !matches!(error, keyring::Error::NoEntry) {
                        tracing::warn!("credential store: {error}");
                    }
                })
                .ok(),
            Store::File(path) => std::fs::read_to_string(path).ok(),
        }
    }

    /// The store of the Discord connection beside this one: its own entry or file, so a
    /// sign-in and a Discord refresh token never overwrite each other.
    pub fn discord(&self) -> Store {
        match self {
            Store::System { user } => Store::System {
                user: format!("discord:{user}"),
            },
            Store::File(path) => {
                let name = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("signin");
                let name = name.strip_prefix("signin-").unwrap_or(name);
                Store::File(path.with_file_name(format!("discord-{name}")))
            }
        }
    }

    pub fn load_discord(&self) -> Option<SavedDiscord> {
        serde_json::from_str(&self.read()?).ok()
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
            (Store::System { user }, None) => keyring::Entry::new(SERVICE, user)
                .and_then(|entry| entry.delete_credential())
                .or_else(|error| match error {
                    keyring::Error::NoEntry => Ok(()),
                    other => Err(other),
                })
                .map_err(|e| e.to_string()),
            (Store::File(path), Some(text)) => write_private(path, &text),
            (Store::File(path), None) => match std::fs::remove_file(path) {
                Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                    Err(error.to_string())
                }
                _ => Ok(()),
            },
        };
        if let Err(error) = result {
            tracing::warn!("could not keep the sign-in: {error}");
        }
    }
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

impl SaveQueue {
    pub fn new(store: Store) -> Self {
        let (jobs, rx) = std::sync::mpsc::channel::<Job>();
        std::thread::spawn(move || {
            for job in rx {
                match job {
                    Job::Save(saved) => store.save(saved.as_ref()),
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
        let _ = self.jobs.send(Job::Save(saved));
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
