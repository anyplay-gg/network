//! macOS: ScreenCaptureKit for the game's window and sound, VideoToolbox for H.264, and the
//! Screen Recording permission. All of it ships with the system (ADR 0036).

pub mod access;
pub mod sck;
pub mod vt;
