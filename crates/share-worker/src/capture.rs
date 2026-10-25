//! Window capture: the game's window as a [`FrameSource`], and nothing else on the screen.
//!
//! On Windows this is Windows Graphics Capture of one window (the `windows-capture` crate, MIT).
//! The window is found by the processes the launcher started, never by a guess at its title
//! (see [`find_window`]). The capture runs on a thread of its own and keeps only the newest
//! picture, so a slow encoder drops pictures instead of queueing them; a window that does not
//! change (or is minimised) gives its last picture again, so the stream keeps its pace and the
//! viewer sees a live stream rather than a stalled one. On Linux the source will be the desktop
//! portal's window picker (the player confirms the window each time); it is not built yet.

use crate::host::FrameSource;

/// Which window to share.
#[derive(Clone, Debug, Default)]
pub struct WindowTarget {
    /// The window the launcher found for the game (an `HWND` as a number), if it has one.
    pub window: Option<i64>,
    /// The processes of the game: the one the launcher started and what it started.
    pub pids: Vec<u32>,
    /// Part of a window's title: used only when neither of the above finds a window.
    pub title_contains: String,
}

/// Opens the source for `target`.
#[cfg(windows)]
pub fn open(target: &WindowTarget) -> Result<Box<dyn FrameSource>, String> {
    let window = find_window(target)?;
    windows_source::WindowSource::open(window)
        .map(|source| Box::new(source) as Box<dyn FrameSource>)
}

/// Opens the source for `target`.
#[cfg(not(windows))]
pub fn open(_target: &WindowTarget) -> Result<Box<dyn FrameSource>, String> {
    Err("sharing a window is not built for this system yet".into())
}

/// How large a window must be to count as the game's (the launcher's rule for the game window).
#[cfg(windows)]
const MIN_WIDTH: i32 = 200;
#[cfg(windows)]
const MIN_HEIGHT: i32 = 150;

/// How long [`find_window`] keeps looking: a game that has just started has no window yet.
#[cfg(windows)]
const LOOK_FOR: std::time::Duration = std::time::Duration::from_secs(8);

/// Finds the window to capture, in this order:
///
/// 1. the window the launcher named, if it still exists and can be captured;
/// 2. the largest window of the game's processes that is visible, not a tool window and large
///    enough (the launcher's own rule for "the game's window"), looking for a few seconds;
/// 3. only when no process is known: a window whose title contains the title given.
///
/// A window of another process is never taken when processes are known.
#[cfg(windows)]
pub fn find_window(target: &WindowTarget) -> Result<windows_capture::window::Window, String> {
    use std::time::Instant;
    use windows_capture::window::Window;

    let deadline = Instant::now() + LOOK_FOR;
    loop {
        if let Some(hwnd) = target.window.filter(|&h| h != 0) {
            let window = Window::from_raw_hwnd(hwnd as isize as *mut std::ffi::c_void);
            if window.is_valid() && big_enough(&window) {
                // A window the launcher named belongs to the game; if processes are given as
                // well, believe them: a handle can be reused by another window.
                let owner = window.process_id().ok();
                if target.pids.is_empty() || owner.is_some_and(|p| target.pids.contains(&p)) {
                    return Ok(window);
                }
            }
        }
        if !target.pids.is_empty() {
            let best = Window::enumerate()
                .unwrap_or_default()
                .into_iter()
                .filter(|w| w.process_id().is_ok_and(|p| target.pids.contains(&p)))
                .filter(big_enough)
                .max_by_key(area);
            if let Some(window) = best {
                return Ok(window);
            }
        } else if !target.title_contains.is_empty()
            && let Ok(window) = Window::from_contains_name(&target.title_contains)
        {
            return Ok(window);
        }
        if Instant::now() >= deadline {
            return Err("the game's window was not found".into());
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
}

#[cfg(windows)]
fn big_enough(window: &windows_capture::window::Window) -> bool {
    window
        .rect()
        .is_ok_and(|r| r.right - r.left >= MIN_WIDTH && r.bottom - r.top >= MIN_HEIGHT)
}

#[cfg(windows)]
fn area(window: &windows_capture::window::Window) -> i64 {
    window.rect().map_or(0, |r| {
        i64::from(r.right - r.left) * i64::from(r.bottom - r.top)
    })
}

#[cfg(windows)]
mod windows_source {
    use std::sync::{Arc, Condvar, Mutex};
    use std::time::{Duration, Instant};

    use anyplay_share::codec::Frame;
    use windows_capture::capture::{Context, GraphicsCaptureApiHandler};
    use windows_capture::frame::Frame as CapturedFrame;
    use windows_capture::graphics_capture_api::InternalCaptureControl;
    use windows_capture::settings::{
        ColorFormat, CursorCaptureSettings, DirtyRegionSettings, DrawBorderSettings,
        MinimumUpdateIntervalSettings, SecondaryWindowSettings, Settings,
    };
    use windows_capture::window::Window;

    use crate::host::FrameSource;

    /// The newest picture, and whether the capture has ended.
    #[derive(Default)]
    struct Slot {
        frame: Option<Frame>,
        ended: bool,
    }

    type Shared = Arc<(Mutex<Slot>, Condvar)>;

    struct Handler {
        shared: Shared,
        started: Instant,
        scratch: Vec<u8>,
    }

    impl GraphicsCaptureApiHandler for Handler {
        type Flags = Shared;
        type Error = Box<dyn std::error::Error + Send + Sync>;

        fn new(ctx: Context<Self::Flags>) -> Result<Self, Self::Error> {
            Ok(Handler {
                shared: ctx.flags,
                started: Instant::now(),
                scratch: Vec::new(),
            })
        }

        fn on_frame_arrived(
            &mut self,
            frame: &mut CapturedFrame,
            _control: InternalCaptureControl,
        ) -> Result<(), Self::Error> {
            let (width, height) = (frame.width(), frame.height());
            let buffer = frame.buffer()?;
            let bgra = buffer.as_nopadding_buffer(&mut self.scratch).to_vec();
            let picture = Frame {
                width,
                height,
                timestamp_us: self.started.elapsed().as_micros() as u32,
                bgra,
            };
            let (lock, ready) = &*self.shared;
            lock.lock().unwrap_or_else(|e| e.into_inner()).frame = Some(picture);
            ready.notify_one();
            Ok(())
        }

        fn on_closed(&mut self) -> Result<(), Self::Error> {
            let (lock, ready) = &*self.shared;
            lock.lock().unwrap_or_else(|e| e.into_inner()).ended = true;
            ready.notify_all();
            Ok(())
        }
    }

    type Control =
        windows_capture::capture::CaptureControl<Handler, Box<dyn std::error::Error + Send + Sync>>;

    pub struct WindowSource {
        shared: Shared,
        control: Option<Control>,
        last: Option<Frame>,
        started: Instant,
    }

    impl WindowSource {
        pub fn open(window: Window) -> Result<WindowSource, String> {
            let shared: Shared = Arc::default();
            let settings = Settings::new(
                window,
                CursorCaptureSettings::WithoutCursor,
                DrawBorderSettings::WithoutBorder,
                SecondaryWindowSettings::Default,
                MinimumUpdateIntervalSettings::Default,
                DirtyRegionSettings::Default,
                ColorFormat::Bgra8,
                shared.clone(),
            );
            let control = Handler::start_free_threaded(settings)
                .map_err(|e| format!("window capture could not start: {e}"))?;
            Ok(WindowSource {
                shared,
                control: Some(control),
                last: None,
                started: Instant::now(),
            })
        }
    }

    impl FrameSource for WindowSource {
        fn next_frame(&mut self) -> Option<Frame> {
            let (lock, ready) = &*self.shared;
            let mut slot = lock.lock().unwrap_or_else(|e| e.into_inner());
            loop {
                if let Some(frame) = slot.frame.take() {
                    self.last = Some(frame.clone());
                    return Some(frame);
                }
                if slot.ended {
                    return None;
                }
                // Nothing new: the window did not change. The last picture goes again, with the
                // time of now, so that the stream does not stall.
                if let Some(last) = &mut self.last {
                    last.timestamp_us = self.started.elapsed().as_micros() as u32;
                    return Some(last.clone());
                }
                // Nothing yet: wait for the first picture.
                slot = ready
                    .wait_timeout(slot, Duration::from_millis(250))
                    .unwrap_or_else(|e| e.into_inner())
                    .0;
            }
        }
    }

    impl Drop for WindowSource {
        fn drop(&mut self) {
            if let Some(control) = self.control.take() {
                let _ = control.stop();
            }
        }
    }
}
