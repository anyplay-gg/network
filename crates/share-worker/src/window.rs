//! The viewer's window, through minifb (MIT or Apache-2.0): the picture, scaled to the window
//! with its shape kept, and nothing else. Closing it (or Escape) leaves the stream.

use anyplay_share::codec::Frame;
use anyplay_share::pad::PadState;
use minifb::{Key, ScaleMode, Window, WindowOptions};

use crate::input::{PadKey, pad_from_keys};
use crate::viewer::FrameSink;

/// The window opens at the picture's size, but never larger than this, so that a 1080p stream
/// fits a laptop screen; the viewer can resize or maximise it.
const OPEN_MAX: (usize, usize) = (1280, 720);

pub struct WindowSink {
    title: String,
    window: Option<Window>,
    buffer: Vec<u32>,
    size: (usize, usize),
    closed: bool,
    pad: bool,
}

impl WindowSink {
    pub fn new(title: &str) -> WindowSink {
        WindowSink {
            title: title.to_owned(),
            window: None,
            buffer: Vec::new(),
            size: (0, 0),
            closed: false,
            pad: false,
        }
    }

    /// The window also reads the guest's pad from the keyboard (share play, not share screen).
    pub fn with_pad(mut self) -> WindowSink {
        self.pad = true;
        self
    }

    /// The window's own events; with `picture`, the last picture again.
    fn present(&mut self, picture: bool) {
        let (width, height) = self.size;
        let Some(window) = &mut self.window else {
            return;
        };
        let failed = if self.buffer.is_empty() || !picture {
            window.update();
            false
        } else {
            window
                .update_with_buffer(&self.buffer, width, height)
                .is_err()
        };
        if failed || !window.is_open() || window.is_key_down(Key::Escape) {
            self.closed = true;
        }
    }
}

impl FrameSink for WindowSink {
    fn show(&mut self, frame: &Frame) {
        let (width, height) = (frame.width as usize, frame.height as usize);
        if width == 0 || height == 0 || !frame.is_well_formed() {
            return;
        }
        if self.window.is_none() {
            let (open_w, open_h) = if width > OPEN_MAX.0 || height > OPEN_MAX.1 {
                let scale =
                    (OPEN_MAX.0 as f64 / width as f64).min(OPEN_MAX.1 as f64 / height as f64);
                (
                    ((width as f64 * scale) as usize).max(160),
                    ((height as f64 * scale) as usize).max(90),
                )
            } else {
                (width, height)
            };
            let options = WindowOptions {
                resize: true,
                scale_mode: ScaleMode::AspectRatioStretch,
                ..WindowOptions::default()
            };
            match Window::new(&self.title, open_w, open_h, options) {
                Ok(mut window) => {
                    window.set_target_fps(0);
                    self.window = Some(window);
                }
                Err(_) => {
                    self.closed = true;
                    return;
                }
            }
        }
        // BGRA bytes to 0RGB words.
        self.buffer.clear();
        let (pixels, _) = frame.bgra.as_chunks::<4>();
        self.buffer.extend(
            pixels
                .iter()
                .map(|p| u32::from(p[2]) << 16 | u32::from(p[1]) << 8 | u32::from(p[0])),
        );
        self.size = (width, height);
        self.present(true);
    }

    fn idle(&mut self) {
        self.present(false);
    }

    fn close(&mut self) {
        self.window = None;
    }

    fn closed(&self) -> bool {
        self.closed
    }

    fn pad(&mut self) -> Option<PadState> {
        if !self.pad {
            return None;
        }
        let window = self.window.as_ref().filter(|w| w.is_active())?;
        // Only the keys of the pad are asked about; a key of the window's own (Escape) is not.
        Some(pad_from_keys(|key| window.is_key_down(minifb_key(key))))
    }
}

fn minifb_key(key: PadKey) -> Key {
    match key {
        PadKey::W => Key::W,
        PadKey::A => Key::A,
        PadKey::S => Key::S,
        PadKey::D => Key::D,
        PadKey::I => Key::I,
        PadKey::J => Key::J,
        PadKey::K => Key::K,
        PadKey::L => Key::L,
        PadKey::Up => Key::Up,
        PadKey::Down => Key::Down,
        PadKey::Left => Key::Left,
        PadKey::Right => Key::Right,
        PadKey::Z => Key::Z,
        PadKey::X => Key::X,
        PadKey::C => Key::C,
        PadKey::V => Key::V,
        PadKey::Q => Key::Q,
        PadKey::E => Key::E,
        PadKey::Num1 => Key::Key1,
        PadKey::Num3 => Key::Key3,
        PadKey::F => Key::F,
        PadKey::G => Key::G,
        PadKey::Enter => Key::Enter,
        PadKey::Tab => Key::Tab,
        PadKey::Backspace => Key::Backspace,
    }
}
