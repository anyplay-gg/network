//! The Screen Recording switch of macOS, through CoreGraphics.

use std::process::Command;

use objc2_core_graphics::{CGPreflightScreenCaptureAccess, CGRequestScreenCaptureAccess};

/// The page of System Settings with the Screen Recording list (macOS 13 and later answer to the
/// old Preferences URL as well).
pub const SETTINGS_URL: &str =
    "x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture";

/// True when the program that runs the worker may capture the screen. Looks only; no prompt.
pub fn granted() -> bool {
    CGPreflightScreenCaptureAccess()
}

/// Shows the system's prompt the first time; returns whether the permission is there.
pub fn request() -> bool {
    CGRequestScreenCaptureAccess()
}

/// Opens System Settings at the Screen Recording page.
pub fn open_settings() -> Result<(), String> {
    let status = Command::new("/usr/bin/open")
        .arg(SETTINGS_URL)
        .status()
        .map_err(|e| format!("System Settings did not open: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("System Settings did not open: {status}"))
    }
}
