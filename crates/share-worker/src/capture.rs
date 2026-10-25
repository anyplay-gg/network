//! Window capture: the game's window as a [`FrameSource`], and nothing else on the screen.
//!
//! On Windows this is Windows Graphics Capture of one window (the `windows-capture` crate, MIT).
//! The window is found by the processes the launcher started, never by a guess at its title
//! (see [`find_window`]). The capture runs on a thread of its own and keeps only the newest
//! picture, so a slow encoder drops pictures instead of queueing them; a window that does not
//! change (or is minimised) gives its last picture again, so the stream keeps its pace and the
//! viewer sees a live stream rather than a stalled one. On Linux the player picks the window in
//! the desktop portal's dialog (the player confirms the window each time; [`crate::portal`]) and
//! its pictures come from PipeWire, with the same newest-picture rule.

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

/// Opens the source: the window the player picks in the portal's dialog. The portal cannot be
/// told which window is the game's, so `target` is not used here.
#[cfg(target_os = "linux")]
pub fn open(_target: &WindowTarget) -> Result<Box<dyn FrameSource>, String> {
    let window = crate::portal::pick_window()?;
    pipewire_source::PipeWireSource::open(window)
        .map(|source| Box::new(source) as Box<dyn FrameSource>)
}

/// Opens the source for `target`.
#[cfg(not(any(windows, target_os = "linux")))]
pub fn open(_target: &WindowTarget) -> Result<Box<dyn FrameSource>, String> {
    Err("sharing a window is not built for this system yet".into())
}

/// The order of the colour bytes of a 32-bit picture from the capture.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PixelOrder {
    /// B, G, R, then alpha or padding.
    Bgr,
    /// R, G, B, then alpha or padding.
    Rgb,
}

/// One picture of `width` by `height` 32-bit pixels, rows `stride` bytes apart, as tight BGRA
/// with opaque alpha (the fourth byte of a captured window is padding or undefined). `None` when
/// `source` is shorter than the picture.
pub fn pack_bgra(
    source: &[u8],
    width: u32,
    height: u32,
    stride: usize,
    order: PixelOrder,
) -> Option<Vec<u8>> {
    let row = width as usize * 4;
    if width == 0 || height == 0 || stride < row {
        return None;
    }
    let needed = stride.checked_mul(height as usize - 1)?.checked_add(row)?;
    if source.len() < needed {
        return None;
    }
    let mut out = Vec::with_capacity(row * height as usize);
    for y in 0..height as usize {
        let line = &source[y * stride..y * stride + row];
        let (pixels, _) = line.as_chunks::<4>();
        for p in pixels {
            match order {
                PixelOrder::Bgr => out.extend_from_slice(&[p[0], p[1], p[2], 255]),
                PixelOrder::Rgb => out.extend_from_slice(&[p[2], p[1], p[0], 255]),
            }
        }
    }
    Some(out)
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

#[cfg(target_os = "linux")]
mod pipewire_source {
    use std::os::fd::OwnedFd;
    use std::sync::{Arc, Condvar, Mutex, mpsc};
    use std::thread::JoinHandle;
    use std::time::{Duration, Instant};

    use anyplay_share::codec::Frame;
    use pipewire as pw;
    use pw::properties::properties;
    use pw::spa;
    use pw::spa::param::video::{VideoFormat, VideoInfoRaw};
    use pw::spa::pod::Pod;

    use super::{PixelOrder, pack_bgra};
    use crate::host::FrameSource;
    use crate::portal::PortalWindow;

    /// The newest picture, and whether the stream has ended.
    #[derive(Default)]
    struct Slot {
        frame: Option<Frame>,
        ended: bool,
    }

    type Shared = Arc<(Mutex<Slot>, Condvar)>;

    fn end(shared: &Shared) {
        let (lock, ready) = &**shared;
        lock.lock().unwrap_or_else(|e| e.into_inner()).ended = true;
        ready.notify_all();
    }

    pub struct PipeWireSource {
        shared: Shared,
        stop: Option<pw::channel::Sender<()>>,
        thread: Option<JoinHandle<()>>,
        last: Option<Frame>,
        started: Instant,
        // Last: the portal session closes after the stream has stopped.
        _window: Option<PortalWindow>,
    }

    impl PipeWireSource {
        pub fn open(mut window: PortalWindow) -> Result<PipeWireSource, String> {
            let fd = window
                .fd
                .take()
                .ok_or("the portal gave no PipeWire remote")?;
            let node = window.node_id;
            Self::start(Some(fd), node, Some(window))
        }

        /// A node on the user's own PipeWire, without the portal (tests).
        #[cfg(test)]
        pub fn open_local(node: u32) -> Result<PipeWireSource, String> {
            Self::start(None, node, None)
        }

        fn start(
            fd: Option<OwnedFd>,
            node: u32,
            window: Option<PortalWindow>,
        ) -> Result<PipeWireSource, String> {
            let shared: Shared = Arc::default();
            let (stop, stop_rx) = pw::channel::channel::<()>();
            let (ready_tx, ready_rx) = mpsc::channel::<Result<(), String>>();
            let thread_shared = shared.clone();
            let thread = std::thread::Builder::new()
                .name("pipewire capture".into())
                .spawn(move || {
                    if let Err(why) = run(fd, node, thread_shared.clone(), stop_rx, &ready_tx) {
                        let _ = ready_tx.send(Err(why));
                    }
                    end(&thread_shared);
                })
                .map_err(|e| format!("the capture thread could not start: {e}"))?;
            match ready_rx.recv() {
                Ok(Ok(())) => {}
                Ok(Err(why)) => return Err(why),
                Err(_) => return Err("the capture thread ended before it connected".into()),
            }
            Ok(PipeWireSource {
                shared,
                stop: Some(stop),
                thread: Some(thread),
                last: None,
                started: Instant::now(),
                _window: window,
            })
        }
    }

    /// What the stream's callbacks keep.
    struct Data {
        format: VideoInfoRaw,
        shared: Shared,
        started: Instant,
    }

    /// The capture thread: connects to the portal's PipeWire remote, asks for the window's stream
    /// as 32-bit RGB in shared memory and keeps the newest picture until told to stop.
    fn run(
        fd: Option<OwnedFd>,
        node: u32,
        shared: Shared,
        stop: pw::channel::Receiver<()>,
        ready: &mpsc::Sender<Result<(), String>>,
    ) -> Result<(), String> {
        let failed = |what: &str, e: pw::Error| format!("PipeWire: {what}: {e}");
        pw::init();
        let mainloop = pw::main_loop::MainLoopRc::new(None).map_err(|e| failed("loop", e))?;
        let context =
            pw::context::ContextRc::new(&mainloop, None).map_err(|e| failed("context", e))?;
        let core = match fd {
            Some(fd) => context.connect_fd_rc(fd, None),
            None => context.connect_rc(None),
        }
        .map_err(|e| failed("connect", e))?;
        let stream = pw::stream::StreamBox::new(
            &core,
            "anyplay-share",
            properties! {
                *pw::keys::MEDIA_TYPE => "Video",
                *pw::keys::MEDIA_CATEGORY => "Capture",
                *pw::keys::MEDIA_ROLE => "Screen",
            },
        )
        .map_err(|e| failed("stream", e))?;
        let quit_on_error = mainloop.clone();
        let data = Data {
            format: VideoInfoRaw::default(),
            shared: shared.clone(),
            started: Instant::now(),
        };
        let _listener = stream
            .add_local_listener_with_user_data(data)
            .state_changed(move |_, data, _, new| match new {
                pw::stream::StreamState::Error(why) => {
                    eprintln!("share worker: the PipeWire stream failed: {why}");
                    end(&data.shared);
                    quit_on_error.quit();
                }
                // The connection to PipeWire is gone.
                pw::stream::StreamState::Unconnected => {
                    end(&data.shared);
                    quit_on_error.quit();
                }
                _ => {}
            })
            .param_changed(|_, data, id, param| {
                let Some(param) = param else { return };
                if id != spa::param::ParamType::Format.as_raw() {
                    return;
                }
                let _ = data.format.parse(param);
            })
            .process(|stream, data| {
                let Some(mut buffer) = stream.dequeue_buffer() else {
                    return;
                };
                let order = match data.format.format() {
                    VideoFormat::BGRx | VideoFormat::BGRA => PixelOrder::Bgr,
                    VideoFormat::RGBx | VideoFormat::RGBA => PixelOrder::Rgb,
                    _ => return,
                };
                let size = data.format.size();
                let Some(plane) = buffer.datas_mut().first_mut() else {
                    return;
                };
                let chunk = plane.chunk();
                let (offset, length, stride) = (
                    chunk.offset() as usize,
                    chunk.size() as usize,
                    chunk.stride(),
                );
                let corrupted = chunk.flags().contains(spa::buffer::ChunkFlags::CORRUPTED);
                // An empty chunk: the window did not change.
                if corrupted || length == 0 || stride <= 0 {
                    return;
                }
                let Some(bytes) = plane.data() else { return };
                let Some(bytes) = bytes.get(offset..offset.saturating_add(length).min(bytes.len()))
                else {
                    return;
                };
                let Some(bgra) = pack_bgra(bytes, size.width, size.height, stride as usize, order)
                else {
                    return;
                };
                let picture = Frame {
                    width: size.width,
                    height: size.height,
                    timestamp_us: data.started.elapsed().as_micros() as u32,
                    bgra,
                };
                let (lock, ready) = &*data.shared;
                lock.lock().unwrap_or_else(|e| e.into_inner()).frame = Some(picture);
                ready.notify_one();
            })
            .register()
            .map_err(|e| failed("listener", e))?;

        let format = format_param().ok_or("PipeWire: the format could not be written")?;
        let mut params = [Pod::from_bytes(&format).ok_or("PipeWire: the format is not a pod")?];
        stream
            .connect(
                spa::utils::Direction::Input,
                Some(node),
                pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
                &mut params,
            )
            .map_err(|e| failed("stream connect", e))?;
        // The window's node goes away when the window closes or the player stops sharing in the
        // desktop's own controls. The stream itself only pauses then, so the registry tells.
        let registry = core.get_registry_rc().map_err(|e| failed("registry", e))?;
        let (gone, quit_on_gone) = (shared.clone(), mainloop.clone());
        let _gone = registry
            .add_listener_local()
            .global_remove(move |id| {
                if id == node {
                    end(&gone);
                    quit_on_gone.quit();
                }
            })
            .register();
        let quit = mainloop.clone();
        let _stop = stop.attach(mainloop.loop_(), move |()| quit.quit());
        let _ = ready.send(Ok(()));
        mainloop.run();
        Ok(())
    }

    /// The formats the worker takes: 32-bit RGB in either order, any size, at most 60 pictures a
    /// second (the most a stream takes; more would only be copied and dropped). No DMA-BUF
    /// modifiers are offered, so the compositor sends pictures in shared memory.
    fn format_param() -> Option<Vec<u8>> {
        let object = pw::spa::pod::object!(
            pw::spa::utils::SpaTypes::ObjectParamFormat,
            pw::spa::param::ParamType::EnumFormat,
            pw::spa::pod::property!(
                pw::spa::param::format::FormatProperties::MediaType,
                Id,
                pw::spa::param::format::MediaType::Video
            ),
            pw::spa::pod::property!(
                pw::spa::param::format::FormatProperties::MediaSubtype,
                Id,
                pw::spa::param::format::MediaSubtype::Raw
            ),
            pw::spa::pod::property!(
                pw::spa::param::format::FormatProperties::VideoFormat,
                Choice,
                Enum,
                Id,
                VideoFormat::BGRx,
                VideoFormat::BGRx,
                VideoFormat::BGRA,
                VideoFormat::RGBx,
                VideoFormat::RGBA
            ),
            pw::spa::pod::property!(
                pw::spa::param::format::FormatProperties::VideoSize,
                Choice,
                Range,
                Rectangle,
                pw::spa::utils::Rectangle {
                    width: 1280,
                    height: 720
                },
                pw::spa::utils::Rectangle {
                    width: 1,
                    height: 1
                },
                pw::spa::utils::Rectangle {
                    width: 8192,
                    height: 8192
                }
            ),
            pw::spa::pod::property!(
                pw::spa::param::format::FormatProperties::VideoFramerate,
                Choice,
                Range,
                Fraction,
                pw::spa::utils::Fraction { num: 60, denom: 1 },
                pw::spa::utils::Fraction { num: 0, denom: 1 },
                pw::spa::utils::Fraction { num: 60, denom: 1 }
            ),
        );
        pw::spa::pod::serialize::PodSerializer::serialize(
            std::io::Cursor::new(Vec::new()),
            &pw::spa::pod::Value::Object(object),
        )
        .ok()
        .map(|(cursor, _)| cursor.into_inner())
    }

    impl FrameSource for PipeWireSource {
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
                slot = ready
                    .wait_timeout(slot, Duration::from_millis(250))
                    .unwrap_or_else(|e| e.into_inner())
                    .0;
            }
        }
    }

    impl Drop for PipeWireSource {
        fn drop(&mut self) {
            if let Some(stop) = self.stop.take() {
                let _ = stop.send(());
            }
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{PixelOrder, pack_bgra};

    /// The id of the PipeWire node named `name`, from `pw-dump`.
    #[cfg(target_os = "linux")]
    fn node_id(name: &str) -> Option<u32> {
        let out = std::process::Command::new("pw-dump").output().ok()?;
        let objects: Vec<serde_json::Value> = serde_json::from_slice(&out.stdout).ok()?;
        objects.iter().find_map(|object| {
            let named = object["info"]["props"]["node.name"].as_str() == Some(name);
            let node = object["type"].as_str() == Some("PipeWire:Interface:Node");
            (named && node)
                .then(|| object["id"].as_u64())
                .flatten()
                .map(|id| id as u32)
        })
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "needs a running PipeWire, pw-dump and gst-launch-1.0 with its PipeWire plugin"]
    fn the_capture_ends_when_its_stream_goes_away() {
        use std::time::{Duration, Instant};

        use crate::host::FrameSource;

        // A live test picture published as a PipeWire node, as a window's stream is.
        let name = format!("anyplay-capture-test-{}", std::process::id());
        let mut publisher = std::process::Command::new("gst-launch-1.0")
            .args([
                "-q",
                "videotestsrc",
                "is-live=true",
                "!",
                "video/x-raw,format=BGRx,width=320,height=240,framerate=30/1",
                "!",
                "pipewiresink",
                "mode=provide",
                &format!("stream-properties=props,node.name={name}"),
            ])
            .spawn()
            .expect("gst-launch-1.0");
        let node = (0..50).find_map(|_| {
            std::thread::sleep(Duration::from_millis(100));
            node_id(&name)
        });
        let Some(node) = node else {
            let _ = publisher.kill();
            panic!("the test picture's node did not appear");
        };
        let mut source = super::pipewire_source::PipeWireSource::open_local(node).unwrap();
        let first = source.next_frame().expect("a picture");
        assert_eq!((first.width, first.height), (320, 240));

        // The window closes: its node goes away, and so must the capture.
        publisher.kill().unwrap();
        let _ = publisher.wait();
        let deadline = Instant::now() + Duration::from_secs(3);
        while source.next_frame().is_some() {
            assert!(
                Instant::now() < deadline,
                "the capture went on after its stream went away"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn rows_with_padding_are_packed_tight_with_opaque_alpha() {
        // 2 by 2, stride 12 (4 bytes of padding at the end of each row).
        let source = [
            1, 2, 3, 0, 4, 5, 6, 0, 99, 99, 99, 99, //
            7, 8, 9, 0, 10, 11, 12, 0, 99, 99, 99, 99,
        ];
        let packed = pack_bgra(&source, 2, 2, 12, PixelOrder::Bgr).unwrap();
        assert_eq!(
            packed,
            [1, 2, 3, 255, 4, 5, 6, 255, 7, 8, 9, 255, 10, 11, 12, 255]
        );
    }

    #[test]
    fn rgb_order_is_swapped_to_bgra() {
        let packed = pack_bgra(&[10, 20, 30, 0], 1, 1, 4, PixelOrder::Rgb).unwrap();
        assert_eq!(packed, [30, 20, 10, 255]);
    }

    #[test]
    fn a_buffer_shorter_than_the_picture_is_refused() {
        // The last row needs 8 bytes from 12; only 19 of 20 are there.
        assert!(pack_bgra(&[0; 19], 2, 2, 12, PixelOrder::Bgr).is_none());
        // The last row may end without its padding.
        assert!(pack_bgra(&[0; 20], 2, 2, 12, PixelOrder::Bgr).is_some());
        assert!(pack_bgra(&[0; 16], 2, 2, 4, PixelOrder::Bgr).is_none());
        assert!(pack_bgra(&[], 0, 0, 0, PixelOrder::Bgr).is_none());
    }
}
