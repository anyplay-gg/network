//! The viewer's window, through minifb: the picture, scaled to the window, and nothing else.
//! Closing it (or Escape) leaves the stream.

use anyplay_share::codec::Frame;
use minifb::{Key, Scale, Window, WindowOptions};

use crate::viewer::FrameSink;

pub struct WindowSink {
    title: String,
    window: Option<Window>,
    buffer: Vec<u32>,
    closed: bool,
}

impl WindowSink {
    pub fn new(title: &str) -> WindowSink {
        WindowSink {
            title: title.to_owned(),
            window: None,
            buffer: Vec::new(),
            closed: false,
        }
    }
}

impl FrameSink for WindowSink {
    fn show(&mut self, frame: &Frame) {
        let (width, height) = (frame.width as usize, frame.height as usize);
        if self.window.is_none() {
            let options = WindowOptions {
                resize: true,
                scale: Scale::X1,
                ..WindowOptions::default()
            };
            match Window::new(&self.title, width, height, options) {
                Ok(window) => self.window = Some(window),
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
        if let Some(window) = &mut self.window
            && (window
                .update_with_buffer(&self.buffer, width, height)
                .is_err()
                || !window.is_open()
                || window.is_key_down(Key::Escape))
        {
            self.closed = true;
        }
    }

    fn close(&mut self) {
        self.window = None;
    }

    fn closed(&self) -> bool {
        self.closed
    }
}
