//! Window capture: the game's window as a [`FrameSource`], and nothing else on the screen.
//!
//! On Windows this is Windows Graphics Capture of one window (the `windows-capture` crate, MIT).
//! The capture runs on a thread of its own and keeps only the newest picture, so a slow encoder
//! drops pictures instead of queueing them. On Linux the source will be the desktop portal's
//! window picker (the player confirms the window each time); it is not built yet.

use crate::host::FrameSource;

/// Which window to share.
#[derive(Clone, Debug)]
pub struct WindowTarget {
    /// Part of the window's title, as AnyPS5 sets it for the game.
    pub title_contains: String,
}

/// Opens the source for `target`.
#[cfg(windows)]
pub fn open(target: &WindowTarget) -> Result<Box<dyn FrameSource>, String> {
    windows::WindowSource::open(target).map(|source| Box::new(source) as Box<dyn FrameSource>)
}

/// Opens the source for `target`.
#[cfg(not(windows))]
pub fn open(_target: &WindowTarget) -> Result<Box<dyn FrameSource>, String> {
    Err("sharing a window is not built for this system yet".into())
}

#[cfg(windows)]
mod windows {
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

    use super::*;

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

    pub struct WindowSource {
        shared: Shared,
        control: Option<
            windows_capture::capture::CaptureControl<
                Handler,
                Box<dyn std::error::Error + Send + Sync>,
            >,
        >,
    }

    impl WindowSource {
        pub fn open(target: &WindowTarget) -> Result<WindowSource, String> {
            let window = Window::from_contains_name(&target.title_contains)
                .map_err(|_| "the game's window was not found".to_owned())?;
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
            })
        }
    }

    impl FrameSource for WindowSource {
        fn next_frame(&mut self) -> Option<Frame> {
            let (lock, ready) = &*self.shared;
            let mut slot = lock.lock().unwrap_or_else(|e| e.into_inner());
            loop {
                if let Some(frame) = slot.frame.take() {
                    return Some(frame);
                }
                if slot.ended {
                    return None;
                }
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
