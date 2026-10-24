//! Where a device keeps its sign-in between runs: the system's credential store, or a file when
//! there is none (a Linux desktop without a Secret Service, or a test).

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// The part of a sign-in worth keeping: the device and its refresh token.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Saved {
    pub device: u64,
    pub refresh: String,
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
        let text = match self {
            Store::System { user } => keyring::Entry::new(SERVICE, user)
                .and_then(|entry| entry.get_password())
                .map_err(|error| {
                    if !matches!(error, keyring::Error::NoEntry) {
                        tracing::warn!("credential store: {error}");
                    }
                })
                .ok()?,
            Store::File(path) => std::fs::read_to_string(path).ok()?,
        };
        serde_json::from_str(&text).ok()
    }

    /// Keeps `saved`, or removes what was kept when it is `None`.
    pub fn save(&self, saved: Option<&Saved>) {
        let result = match (self, saved) {
            (Store::System { user }, Some(saved)) => keyring::Entry::new(SERVICE, user)
                .and_then(|entry| entry.set_password(&to_json(saved)))
                .map_err(|e| e.to_string()),
            (Store::System { user }, None) => keyring::Entry::new(SERVICE, user)
                .and_then(|entry| entry.delete_credential())
                .or_else(|error| match error {
                    keyring::Error::NoEntry => Ok(()),
                    other => Err(other),
                })
                .map_err(|e| e.to_string()),
            (Store::File(path), Some(saved)) => write_private(path, &to_json(saved)),
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

fn to_json(saved: &Saved) -> String {
    serde_json::to_string(saved).unwrap_or_default()
}

/// Writes `text` to `path` through a temporary file, readable only by the owner on Unix. On
/// Windows the file inherits the per-user data folder's access rules.
fn write_private(path: &std::path::Path, text: &str) -> Result<(), String> {
    use std::io::Write;
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
        };
        store.save(Some(&saved));
        assert_eq!(store.load(), Some(saved));
        store.save(None);
        assert_eq!(store.load(), None);
        store.save(None);
    }

    #[test]
    fn every_cloud_has_its_own_file() {
        let dir = tempfile::tempdir().unwrap();
        let a = Store::open(dir.path(), "https://a.example", true);
        let b = Store::open(dir.path(), "https://b.example", true);
        a.save(Some(&Saved {
            device: 1,
            refresh: "a".into(),
        }));
        assert_eq!(b.load(), None);
    }
}
