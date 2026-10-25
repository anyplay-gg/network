//! Which client sends a request: the `X-AnyPlay-Client` header and the version order the cloud
//! uses to retire old launchers.

use std::cmp::Ordering;
use std::fmt;
use std::str::FromStr;

/// The request header that names the client, `<product>/<version>`, such as `launcher/0.3.0`.
pub const CLIENT_HEADER: &str = "X-AnyPlay-Client";

/// The product the cloud gates today.
pub const PRODUCT_LAUNCHER: &str = "launcher";

/// The longest version a client may send.
pub const MAX_VERSION_LEN: usize = 32;

/// The longest product name a client may send.
pub const MAX_PRODUCT_LEN: usize = 32;

/// A plain semantic version, `major.minor.patch` with an optional `-pre.release`. Build metadata
/// (`+...`) is not accepted. A pre-release sorts below its release (`0.3.0-beta < 0.3.0`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Version {
    major: u64,
    minor: u64,
    patch: u64,
    pre: Vec<Identifier>,
    text: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Identifier {
    Numeric(u64),
    Text(String),
}

impl Version {
    /// The version as it was written.
    pub fn as_str(&self) -> &str {
        &self.text
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.text)
    }
}

impl FromStr for Version {
    type Err = ();

    fn from_str(text: &str) -> Result<Self, ()> {
        if text.is_empty() || text.len() > MAX_VERSION_LEN {
            return Err(());
        }
        let (core, pre) = match text.split_once('-') {
            Some((core, pre)) => (core, Some(pre)),
            None => (text, None),
        };
        let parts: Vec<&str> = core.split('.').collect();
        let [major, minor, patch] = parts[..] else {
            return Err(());
        };
        let mut identifiers = Vec::new();
        if let Some(pre) = pre {
            for part in pre.split('.') {
                if part.is_empty() || !part.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
                {
                    return Err(());
                }
                identifiers.push(if part.bytes().all(|b| b.is_ascii_digit()) {
                    Identifier::Numeric(number(part)?)
                } else {
                    Identifier::Text(part.to_owned())
                });
            }
        }
        Ok(Self {
            major: number(major)?,
            minor: number(minor)?,
            patch: number(patch)?,
            pre: identifiers,
            text: text.to_owned(),
        })
    }
}

/// A number without a sign, spaces or a leading zero.
fn number(part: &str) -> Result<u64, ()> {
    if part.is_empty()
        || !part.bytes().all(|b| b.is_ascii_digit())
        || (part.len() > 1 && part.starts_with('0'))
    {
        return Err(());
    }
    part.parse().map_err(|_| ())
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        (self.major, self.minor, self.patch)
            .cmp(&(other.major, other.minor, other.patch))
            .then_with(|| match (self.pre.is_empty(), other.pre.is_empty()) {
                (true, true) => Ordering::Equal,
                (true, false) => Ordering::Greater,
                (false, true) => Ordering::Less,
                (false, false) => self.pre.cmp(&other.pre),
            })
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Identifier {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self, other) {
            (Identifier::Numeric(a), Identifier::Numeric(b)) => a.cmp(b),
            (Identifier::Numeric(_), Identifier::Text(_)) => Ordering::Less,
            (Identifier::Text(_), Identifier::Numeric(_)) => Ordering::Greater,
            (Identifier::Text(a), Identifier::Text(b)) => a.cmp(b),
        }
    }
}

impl PartialOrd for Identifier {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// A parsed `X-AnyPlay-Client` value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClientId {
    pub product: String,
    pub version: Version,
}

impl ClientId {
    /// Reads `<product>/<version>`; `None` for anything else.
    pub fn parse(value: &str) -> Option<Self> {
        let (product, version) = value.split_once('/')?;
        if product.is_empty()
            || product.len() > MAX_PRODUCT_LEN
            || !product
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        {
            return None;
        }
        Some(Self {
            product: product.to_owned(),
            version: version.parse().ok()?,
        })
    }
}

impl fmt::Display for ClientId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.product, self.version)
    }
}

/// True when a client of this value is a launcher older than `minimum`. A missing, unreadable or
/// other-product value is never too old: the gate fails open.
pub fn launcher_too_old(header: Option<&str>, minimum: &Version) -> bool {
    header
        .and_then(ClientId::parse)
        .is_some_and(|c| c.product == PRODUCT_LAUNCHER && c.version < *minimum)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(text: &str) -> Version {
        text.parse().unwrap()
    }

    #[test]
    fn versions_order_like_semver() {
        let ordered = [
            "0.2.9",
            "0.3.0-alpha",
            "0.3.0-alpha.1",
            "0.3.0-alpha.beta",
            "0.3.0-beta",
            "0.3.0-beta.2",
            "0.3.0-beta.11",
            "0.3.0-rc.1",
            "0.3.0",
            "0.3.1",
            "0.10.0",
            "1.0.0",
        ];
        for pair in ordered.windows(2) {
            assert!(v(pair[0]) < v(pair[1]), "{} < {}", pair[0], pair[1]);
        }
        assert_eq!(v("1.2.3"), v("1.2.3"));
        assert_eq!(v("1.2.3").as_str(), "1.2.3");
    }

    #[test]
    fn versions_reject_what_is_not_plain_semver() {
        for bad in [
            "",
            "1",
            "1.2",
            "1.2.3.4",
            "01.2.3",
            "1.2.x",
            "v1.2.3",
            "1.2.3+build",
            "1.2.3-",
            "1.2.3-a..b",
            "1.2.3-\u{e9}",
            " 1.2.3",
            "-1.2.3",
            "99999999999999999999.0.0",
            "1.2.3-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        ] {
            assert!(bad.parse::<Version>().is_err(), "{bad:?}");
        }
    }

    #[test]
    fn client_ids_parse() {
        let id = ClientId::parse("launcher/0.3.0-beta.1").unwrap();
        assert_eq!(id.product, "launcher");
        assert_eq!(id.to_string(), "launcher/0.3.0-beta.1");
        assert!(ClientId::parse("net/0.1.0").is_some());
        for bad in [
            "",
            "launcher",
            "launcher/",
            "/1.0.0",
            "Launcher/1.0.0",
            "a_b/1.0.0",
            "launcher/1.0",
        ] {
            assert!(ClientId::parse(bad).is_none(), "{bad:?}");
        }
    }

    #[test]
    fn only_a_launcher_below_the_minimum_is_too_old() {
        let min = v("0.3.0");
        assert!(launcher_too_old(Some("launcher/0.2.9"), &min));
        assert!(launcher_too_old(Some("launcher/0.3.0-beta"), &min));
        assert!(!launcher_too_old(Some("launcher/0.3.0"), &min));
        assert!(!launcher_too_old(Some("launcher/0.3.1"), &min));
        assert!(!launcher_too_old(Some("launcher/1.0.0"), &min));
        assert!(!launcher_too_old(Some("net/0.0.1"), &min));
        assert!(!launcher_too_old(Some("launcher/garbage"), &min));
        assert!(!launcher_too_old(Some("garbage"), &min));
        assert!(!launcher_too_old(None, &min));
    }
}
