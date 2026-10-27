//! The permission to capture the screen. Windows and Linux need none from the system (Linux
//! asks per window in the portal's own dialog, [`crate::portal`]); macOS keeps a switch for it
//! in System Settings, Privacy & Security, Screen Recording, and shows nothing of the game's
//! window to a program that has not been switched on.
//!
//! The worker never prompts on its own: [`state`] only looks. The launcher asks for the
//! prompt with `--screen-access request` and sends the player to System Settings with
//! `--open-screen-settings`, both described in spec/screen-share.md#screen-recording-permission.

/// Where the permission stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    /// The system needs no permission for this (Windows, Linux).
    NotNeeded,
    /// The player has switched Screen Recording on for the program that runs the worker.
    Granted,
    /// Not switched on, or never asked.
    Denied,
}

impl Access {
    /// True when capture may start.
    pub fn allows_capture(self) -> bool {
        self != Access::Denied
    }

    /// The word for the JSON answer of `--screen-access`.
    pub fn word(self) -> &'static str {
        match self {
            Access::NotNeeded => "not_needed",
            Access::Granted => "granted",
            Access::Denied => "denied",
        }
    }

    /// The one line `--screen-access` prints.
    pub fn to_line(self) -> String {
        serde_json::json!({ "screen_access": self.word() }).to_string()
    }
}

/// The reason a worker in clips or broadcast mode ends with when [`Access::Denied`].
pub const NO_PERMISSION: &str = "no_permission";

/// The `ended` reason for a window source that did not open: the permission when the system
/// said so (it can refuse after a preflight that passed), else `no_window`.
pub fn open_failure_reason(why: &str) -> &'static str {
    #[cfg(target_os = "macos")]
    if crate::mac::sck::is_permission_error(why) {
        return NO_PERMISSION;
    }
    let _ = why;
    "no_window"
}

/// Looks at the permission without asking.
pub fn state() -> Access {
    #[cfg(target_os = "macos")]
    {
        if crate::mac::access::granted() {
            Access::Granted
        } else {
            Access::Denied
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        Access::NotNeeded
    }
}

/// Asks the system for the permission: on macOS the first call shows its prompt (the system
/// shows it once per program) and later calls only look. Returns where it stands.
pub fn request() -> Access {
    #[cfg(target_os = "macos")]
    {
        if crate::mac::access::request() {
            Access::Granted
        } else {
            Access::Denied
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        Access::NotNeeded
    }
}

/// Opens the page of System Settings where the player switches Screen Recording on. Where the
/// system has no such page this does nothing and says so.
pub fn open_settings() -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        crate::mac::access::open_settings()
    }
    #[cfg(not(target_os = "macos"))]
    {
        Err("this system has no Screen Recording switch".into())
    }
}

/// The words the worker puts on standard error and in `ended` details when capture is refused.
pub const EXPLANATION: &str = "Screen Recording is not switched on for AnyPlay. Open System Settings, Privacy & Security, Screen Recording, switch AnyPlay on and start the game again.";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_answer_is_one_json_line() {
        assert_eq!(Access::Granted.to_line(), r#"{"screen_access":"granted"}"#);
        assert_eq!(Access::Denied.to_line(), r#"{"screen_access":"denied"}"#);
        assert_eq!(
            Access::NotNeeded.to_line(),
            r#"{"screen_access":"not_needed"}"#
        );
    }

    #[test]
    fn only_a_refusal_stops_capture() {
        assert!(Access::Granted.allows_capture());
        assert!(Access::NotNeeded.allows_capture());
        assert!(!Access::Denied.allows_capture());
    }

    #[test]
    fn looking_never_panics() {
        // On a runner without a screen this is Denied or Granted; on Windows and Linux NotNeeded.
        let _ = state();
    }
}
