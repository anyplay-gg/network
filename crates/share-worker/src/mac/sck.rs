//! The game's window and sound through ScreenCaptureKit (macOS 12.3 and later).
//!
//! The window is found by the processes the launcher started, never by a guess at its title (the
//! rule is [`crate::pick`]). One window is captured, nothing else on the screen. The capture
//! keeps only the newest picture, so a slow encoder drops pictures instead of queueing them, and
//! a window that does not change gives its last picture again, so the stream keeps its pace;
//! this is how the Windows source behaves too.
//!
//! The game's sound is a second stream of its own that carries only audio, filtered to the
//! game's applications, so the rest of the Mac (messages, music, the launcher) is not heard.
//!
//! Everything starts with the Screen Recording permission ([`crate::access`]): without it the
//! system lists no windows of other programs and refuses to start a stream.

use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use anyplay_share::codec::Frame;
use block2::RcBlock;
use dispatch2::DispatchQueue;
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{AnyThread, DefinedClass, define_class, msg_send};
use objc2_core_audio_types::{AudioBuffer, AudioBufferList};
use objc2_core_foundation::CFRetained;
use objc2_core_media::{CMBlockBuffer, CMSampleBuffer, CMTime, CMTimeFlags};
use objc2_core_video::{
    CVPixelBufferGetBaseAddress, CVPixelBufferGetBytesPerRow, CVPixelBufferGetHeight,
    CVPixelBufferGetWidth, CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags,
    CVPixelBufferUnlockBaseAddress, kCVPixelFormatType_32BGRA,
};
use objc2_foundation::{NSArray, NSError, NSObject, NSObjectProtocol};
use objc2_screen_capture_kit::{
    SCContentFilter, SCShareableContent, SCStream, SCStreamConfiguration, SCStreamDelegate,
    SCStreamOutput, SCStreamOutputType, SCWindow,
};

use crate::capture::{PixelOrder, WindowTarget, pack_bgra};
use crate::host::{FrameSource, Next};
use crate::pick::{self, Seen};
use crate::sound::SoundSource;

/// How long the system gets to answer a question (the list of windows, the start of a stream).
const ANSWER: Duration = Duration::from_secs(10);
/// How long [`find_window`] keeps looking: a game that has just started has no window yet.
const LOOK_FOR: Duration = Duration::from_secs(8);
/// The most pixels a captured picture has per side; the encoder scales down to its setup anyway,
/// and a Retina window of 5K would only cost copying.
const MAX_WIDTH: f64 = 1920.0;
const MAX_HEIGHT: f64 = 1080.0;
/// The error code of ScreenCaptureKit when the player has not allowed screen recording.
const USER_DECLINED: isize = -3801;

fn describe(error: *mut NSError) -> String {
    // SAFETY: the system hands a valid error or null.
    match unsafe { error.as_ref() } {
        Some(error) => format!("{} (code {})", error.localizedDescription(), error.code()),
        None => "no reason given".into(),
    }
}

/// True for the error that means the permission is missing.
fn is_declined(error: *mut NSError) -> bool {
    // SAFETY: as above.
    unsafe { error.as_ref() }.is_some_and(|e| e.code() == USER_DECLINED)
}

/// A reason string that says the permission is missing; the callers recognise it with
/// [`is_permission_error`].
const DECLINED: &str = "screen recording is not allowed";

/// Whether an error from this module means that Screen Recording is not allowed.
pub fn is_permission_error(error: &str) -> bool {
    error.starts_with(DECLINED)
}

/// ScreenCaptureKit and CoreGraphics need the connection to the window server, which a program
/// without an application object does not make by itself (the first call would abort with
/// "CGS_REQUIRE_INIT"). Asking for the main display makes it. Once per process.
fn connect_to_window_server() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let _ = objc2_core_graphics::CGMainDisplayID();
    });
}

/// Asks the system for what can be shared, and reads it with `read` inside its callback.
fn shareable<T: 'static>(
    on_screen_only: bool,
    read: impl Fn(&SCShareableContent) -> T + 'static,
) -> Result<T, String> {
    connect_to_window_server();
    let (tx, rx) = mpsc::channel::<Result<T, String>>();
    let block = RcBlock::new(
        move |content: *mut SCShareableContent, error: *mut NSError| {
            // SAFETY: the system hands a valid object or null.
            let result = match unsafe { content.as_ref() } {
                Some(content) => Ok(read(content)),
                None if is_declined(error) => Err(format!("{DECLINED} ({})", describe(error))),
                None => Err(format!("the system listed nothing: {}", describe(error))),
            };
            let _ = tx.send(result);
        },
    );
    // SAFETY: the block lives through the call and the system copies it.
    unsafe {
        SCShareableContent::getShareableContentExcludingDesktopWindows_onScreenWindowsOnly_completionHandler(
            true,
            on_screen_only,
            &block,
        );
    }
    rx.recv_timeout(ANSWER)
        .map_err(|_| "the system did not answer in time".to_owned())?
}

/// A window the system listed, with the numbers [`pick`] needs.
struct Listed {
    window: Retained<SCWindow>,
    seen: Seen,
}

fn list_windows() -> Result<Vec<Listed>, String> {
    shareable(true, |content| {
        // SAFETY: plain accessors on objects the system listed.
        unsafe { content.windows() }
            .to_vec()
            .into_iter()
            .map(|window| {
                // SAFETY: as above.
                let (id, owner, frame, layer, title) = unsafe {
                    (
                        window.windowID(),
                        window.owningApplication().map(|a| a.processID()),
                        window.frame(),
                        window.windowLayer(),
                        window.title().map(|t| t.to_string()),
                    )
                };
                let seen = Seen {
                    id,
                    pid: owner.unwrap_or(-1).max(0) as u32,
                    width: frame.size.width,
                    height: frame.size.height,
                    layer: layer as i64,
                    title: title.unwrap_or_default(),
                };
                Listed { window, seen }
            })
            .collect()
    })
}

/// Finds the window to capture (the rule is [`pick::pick`]), looking for a few seconds.
pub fn find_window(target: &WindowTarget) -> Result<Retained<SCWindow>, String> {
    let deadline = Instant::now() + LOOK_FOR;
    loop {
        let listed = list_windows()?;
        let seen: Vec<Seen> = listed.iter().map(|l| l.seen.clone()).collect();
        if let Some(index) = pick::pick(&seen, target) {
            return Ok(listed[index].window.clone());
        }
        if Instant::now() >= deadline {
            return Err("the game's window was not found".into());
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// The reason when the system never answered the start of a stream.
const NOT_IN_TIME: &str = "the system did not start the capture in time";

/// How often a start the system never answered is tried again. On a loaded machine (a CI
/// runner right after another capture ended) the first start can be lost; a later one works.
const START_TRIES: usize = 3;

/// Starts a stream and waits until the system says it runs.
fn start(stream: &SCStream) -> Result<(), String> {
    connect_to_window_server();
    let (tx, rx) = mpsc::channel::<Option<String>>();
    let block = RcBlock::new(move |error: *mut NSError| {
        let reason = (!error.is_null()).then(|| {
            if is_declined(error) {
                format!("{DECLINED} ({})", describe(error))
            } else {
                describe(error)
            }
        });
        let _ = tx.send(reason);
    });
    // SAFETY: the block lives through the call and the system copies it.
    unsafe { stream.startCaptureWithCompletionHandler(Some(&block)) };
    match rx.recv_timeout(ANSWER) {
        Ok(None) => Ok(()),
        Ok(Some(why)) => Err(why),
        Err(_) => Err(NOT_IN_TIME.into()),
    }
}

fn stop(stream: &SCStream) {
    // SAFETY: a stream that was started, or never was; stopping either is allowed.
    unsafe { stream.stopCaptureWithCompletionHandler(None) };
}

/// A frame interval of 1/`fps` seconds.
fn interval(fps: i64) -> CMTime {
    CMTime {
        value: 1,
        timescale: fps as i32,
        flags: CMTimeFlags::Valid,
        epoch: 0,
    }
}

// --- what the callbacks keep -----------------------------------------------------------------

/// The newest picture, and whether the capture has ended.
#[derive(Default)]
struct Slot {
    frame: Option<Frame>,
    ended: bool,
}

type Pictures = Arc<(Mutex<Slot>, Condvar)>;

/// Captured sound between the callback and [`SoundSource::read`].
#[derive(Default)]
struct Heard {
    samples: Vec<f32>,
    ended: bool,
}

type Sound = Arc<(Mutex<Heard>, Condvar)>;

/// The most sound kept when nobody reads: 1 second of 48 kHz stereo.
const MAX_KEPT: usize = 48_000 * 2;

/// What the output object keeps.
struct Ivars {
    pictures: Pictures,
    sound: Sound,
    started: Instant,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements; the class has no Drop of its own.
    #[unsafe(super(NSObject))]
    #[name = "AnyPlayCaptureOutput"]
    #[ivars = Ivars]
    struct Output;

    unsafe impl NSObjectProtocol for Output {}

    unsafe impl SCStreamOutput for Output {
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        fn did_output(
            &self,
            _stream: &SCStream,
            buffer: &CMSampleBuffer,
            kind: SCStreamOutputType,
        ) {
            if kind == SCStreamOutputType::Screen {
                self.picture(buffer);
            } else if kind == SCStreamOutputType::Audio {
                self.sound(buffer);
            }
        }
    }

    unsafe impl SCStreamDelegate for Output {
        #[unsafe(method(stream:didStopWithError:))]
        fn did_stop(&self, _stream: &SCStream, _error: &NSError) {
            let ivars = self.ivars();
            let (lock, ready) = &*ivars.pictures;
            lock.lock().unwrap_or_else(|e| e.into_inner()).ended = true;
            ready.notify_all();
            let (lock, ready) = &*ivars.sound;
            lock.lock().unwrap_or_else(|e| e.into_inner()).ended = true;
            ready.notify_all();
        }
    }
);

impl Output {
    fn new(pictures: Pictures, sound: Sound) -> Retained<Output> {
        let this = Self::alloc().set_ivars(Ivars {
            pictures,
            sound,
            started: Instant::now(),
        });
        // SAFETY: NSObject's init.
        unsafe { msg_send![super(this), init] }
    }

    /// A picture arrived: copy it out and keep it as the newest.
    fn picture(&self, buffer: &CMSampleBuffer) {
        // A frame with nothing new (the window is idle) carries no image.
        // SAFETY: plain accessor on a valid sample.
        let Some(image) = (unsafe { buffer.image_buffer() }) else {
            return;
        };
        let read = CVPixelBufferLockFlags::ReadOnly;
        // SAFETY: the buffer is locked while its memory is read and unlocked after; the slice
        // covers `stride * height` bytes, which is what the buffer holds.
        let frame = unsafe {
            if CVPixelBufferLockBaseAddress(&image, read) != 0 {
                return;
            }
            let (width, height) = (
                CVPixelBufferGetWidth(&image),
                CVPixelBufferGetHeight(&image),
            );
            let stride = CVPixelBufferGetBytesPerRow(&image);
            let base = CVPixelBufferGetBaseAddress(&image) as *const u8;
            let packed = if base.is_null() || width == 0 || height == 0 {
                None
            } else {
                let bytes = std::slice::from_raw_parts(base, stride * height);
                pack_bgra(bytes, width as u32, height as u32, stride, PixelOrder::Bgr)
            };
            CVPixelBufferUnlockBaseAddress(&image, read);
            packed.map(|bgra| Frame {
                width: width as u32,
                height: height as u32,
                timestamp_us: self.ivars().started.elapsed().as_micros() as u32,
                bgra,
            })
        };
        if let Some(frame) = frame {
            let (lock, ready) = &*self.ivars().pictures;
            lock.lock().unwrap_or_else(|e| e.into_inner()).frame = Some(frame);
            ready.notify_one();
        }
    }

    /// Sound arrived: 48 kHz, 32-bit float, one buffer per channel (or one interleaved).
    fn sound(&self, buffer: &CMSampleBuffer) {
        // The list for two channels; ScreenCaptureKit gives what was asked for.
        #[repr(C)]
        struct Stereo {
            count: u32,
            buffers: [AudioBuffer; 2],
        }
        let mut list = Stereo {
            count: 0,
            buffers: [AudioBuffer {
                mNumberChannels: 0,
                mDataByteSize: 0,
                mData: std::ptr::null_mut(),
            }; 2],
        };
        let mut needed = 0usize;
        let mut block: *mut CMBlockBuffer = std::ptr::null_mut();
        // SAFETY: `list` has the layout of an AudioBufferList with two buffers and the size is
        // passed; the block buffer that keeps the samples alive comes back retained and is
        // released below.
        let status = unsafe {
            buffer.audio_buffer_list_with_retained_block_buffer(
                &mut needed,
                (&mut list as *mut Stereo).cast::<AudioBufferList>(),
                std::mem::size_of::<Stereo>(),
                None,
                None,
                0,
                &mut block,
            )
        };
        if status != 0 {
            return;
        }
        // Released when it goes out of scope, after the samples were copied.
        // SAFETY: the call returned it retained (+1).
        let _keep: Option<CFRetained<CMBlockBuffer>> =
            std::ptr::NonNull::new(block).map(|b| unsafe { CFRetained::from_raw(b) });

        let floats = |b: &AudioBuffer| -> Vec<f32> {
            if b.mData.is_null() {
                return Vec::new();
            }
            let count = b.mDataByteSize as usize / 4;
            let mut out = vec![0f32; count];
            // SAFETY: `mData` holds `mDataByteSize` bytes while `_keep` lives; copied byte-wise
            // because the data may not be aligned for f32.
            unsafe {
                std::ptr::copy_nonoverlapping(
                    b.mData as *const u8,
                    out.as_mut_ptr() as *mut u8,
                    count * 4,
                );
            }
            out
        };
        let interleaved: Vec<f32> = match list.count {
            0 => return,
            1 if list.buffers[0].mNumberChannels >= 2 => floats(&list.buffers[0]),
            1 => floats(&list.buffers[0])
                .into_iter()
                .flat_map(|s| [s, s])
                .collect(),
            _ => {
                let (left, right) = (floats(&list.buffers[0]), floats(&list.buffers[1]));
                left.iter()
                    .zip(&right)
                    .flat_map(|(l, r)| [*l, *r])
                    .collect()
            }
        };
        let (lock, ready) = &*self.ivars().sound;
        let mut heard = lock.lock().unwrap_or_else(|e| e.into_inner());
        heard.samples.extend_from_slice(&interleaved);
        if heard.samples.len() > MAX_KEPT {
            let drop = heard.samples.len() - MAX_KEPT;
            heard.samples.drain(..drop);
        }
        ready.notify_one();
    }
}

// --- the window as a picture source ---------------------------------------------------------

/// The game's window as a [`FrameSource`].
pub struct SckSource {
    pictures: Pictures,
    stream: Retained<SCStream>,
    // The stream holds its output weakly: keep them for as long as the stream runs.
    _output: Retained<Output>,
    _queue: dispatch2::DispatchRetained<DispatchQueue>,
    last: Option<Frame>,
    started: Instant,
}

impl SckSource {
    /// Starts capturing `window`. Fails with a reason that [`is_permission_error`] recognises
    /// when Screen Recording is not allowed.
    pub fn open(window: &SCWindow) -> Result<SckSource, String> {
        let mut tries = 1;
        loop {
            match Self::open_once(window) {
                Err(why) if why == NOT_IN_TIME && tries < START_TRIES => tries += 1,
                other => return other,
            }
        }
    }

    fn open_once(window: &SCWindow) -> Result<SckSource, String> {
        // SAFETY: plain construction and configuration of ScreenCaptureKit objects, and calls
        // that retain what they are given.
        unsafe {
            let filter =
                SCContentFilter::initWithDesktopIndependentWindow(SCContentFilter::alloc(), window);
            let config = SCStreamConfiguration::new();
            // The window's size in pixels, at most 1080p.
            let frame = window.frame();
            let scale = f64::from(filter.pointPixelScale()).max(1.0);
            let (mut w, mut h) = (frame.size.width * scale, frame.size.height * scale);
            if w > MAX_WIDTH || h > MAX_HEIGHT {
                let shrink = (MAX_WIDTH / w).min(MAX_HEIGHT / h);
                (w, h) = (w * shrink, h * shrink);
            }
            config.setWidth(((w as usize) & !1).max(2));
            config.setHeight(((h as usize) & !1).max(2));
            config.setPixelFormat(kCVPixelFormatType_32BGRA);
            config.setShowsCursor(false);
            config.setCapturesAudio(false);
            config.setQueueDepth(3);
            config.setMinimumFrameInterval(interval(60));

            let pictures: Pictures = Arc::default();
            let output = Output::new(pictures.clone(), Sound::default());
            let stream = SCStream::initWithFilter_configuration_delegate(
                SCStream::alloc(),
                &filter,
                &config,
                Some(ProtocolObject::from_ref(&*output)),
            );
            let queue = DispatchQueue::new("net.anyplay.share.capture", None);
            stream
                .addStreamOutput_type_sampleHandlerQueue_error(
                    ProtocolObject::from_ref(&*output),
                    SCStreamOutputType::Screen,
                    Some(&queue),
                )
                .map_err(|e| {
                    format!(
                        "the capture could not take its output: {}",
                        e.localizedDescription()
                    )
                })?;
            start(&stream)?;
            Ok(SckSource {
                pictures,
                stream,
                _output: output,
                _queue: queue,
                last: None,
                started: Instant::now(),
            })
        }
    }
}

impl FrameSource for SckSource {
    fn next_frame(&mut self) -> Next {
        let (lock, ready) = &*self.pictures;
        let mut slot = lock.lock().unwrap_or_else(|e| e.into_inner());
        if slot.frame.is_none() && !slot.ended && self.last.is_none() {
            // Nothing yet: wait a little for the first picture, then let the caller take its
            // messages.
            slot = ready
                .wait_timeout(slot, Duration::from_millis(250))
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
        if let Some(frame) = slot.frame.take() {
            self.last = Some(frame.clone());
            return Next::Picture(frame);
        }
        if slot.ended {
            return Next::Ended;
        }
        // Nothing new: the window did not change. The last picture goes again, with the time
        // of now, so that the stream does not stall.
        match &mut self.last {
            Some(last) => {
                last.timestamp_us = self.started.elapsed().as_micros() as u32;
                Next::Picture(last.clone())
            }
            None => Next::NotYet,
        }
    }
}

impl Drop for SckSource {
    fn drop(&mut self) {
        stop(&self.stream);
    }
}

// --- the game's sound -----------------------------------------------------------------------

/// The sound of the game's applications, as 48 kHz stereo float.
pub struct SckSound {
    sound: Sound,
    stream: Retained<SCStream>,
    _output: Retained<Output>,
    _queue: dispatch2::DispatchRetained<DispatchQueue>,
}

impl SckSound {
    /// Starts listening to the applications that own any of `pids`. Fails when none of them is an
    /// application the system lists (a game's helper processes are not).
    pub fn open(pids: &[u32]) -> Result<SckSound, String> {
        let wanted = pids.to_vec();
        let (display, applications) = shareable(false, move |content| {
            // SAFETY: plain accessors on objects the system listed.
            unsafe {
                let display = content.displays().to_vec().into_iter().next();
                let applications: Vec<_> = content
                    .applications()
                    .to_vec()
                    .into_iter()
                    .filter(|a| wanted.contains(&(a.processID().max(0) as u32)))
                    .collect();
                (display, applications)
            }
        })?;
        let display = display.ok_or("the system lists no screen")?;
        if applications.is_empty() {
            return Err("the game is not an application the system can listen to".into());
        }
        // SAFETY: plain construction and configuration of ScreenCaptureKit objects.
        unsafe {
            let filter = SCContentFilter::initWithDisplay_includingApplications_exceptingWindows(
                SCContentFilter::alloc(),
                &display,
                &NSArray::from_retained_slice(&applications),
                &NSArray::new(),
            );
            let config = SCStreamConfiguration::new();
            // The stream must have a picture size; it is asked for as seldom and as small as it
            // can be and nobody reads it.
            config.setWidth(2);
            config.setHeight(2);
            config.setMinimumFrameInterval(interval(1));
            config.setCapturesAudio(true);
            config.setExcludesCurrentProcessAudio(true);
            config.setSampleRate(48_000);
            config.setChannelCount(2);

            let sound = Sound::default();
            let output = Output::new(Pictures::default(), sound.clone());
            let stream = SCStream::initWithFilter_configuration_delegate(
                SCStream::alloc(),
                &filter,
                &config,
                Some(ProtocolObject::from_ref(&*output)),
            );
            let queue = DispatchQueue::new("net.anyplay.share.sound", None);
            stream
                .addStreamOutput_type_sampleHandlerQueue_error(
                    ProtocolObject::from_ref(&*output),
                    SCStreamOutputType::Audio,
                    Some(&queue),
                )
                .map_err(|e| {
                    format!(
                        "the sound capture could not take its output: {}",
                        e.localizedDescription()
                    )
                })?;
            start(&stream)?;
            Ok(SckSound {
                sound,
                stream,
                _output: output,
                _queue: queue,
            })
        }
    }
}

// SAFETY: the stream and its output are only touched by `read` (which uses the shared buffer) and
// by `drop` (which stops the stream); ScreenCaptureKit objects may be used from any thread.
unsafe impl Send for SckSound {}

impl SoundSource for SckSound {
    fn read(&mut self, out: &mut Vec<f32>) -> Result<bool, String> {
        let (lock, ready) = &*self.sound;
        let mut heard = lock.lock().unwrap_or_else(|e| e.into_inner());
        if heard.samples.is_empty() && !heard.ended {
            heard = ready
                .wait_timeout(heard, Duration::from_millis(20))
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
        out.append(&mut heard.samples);
        Ok(!heard.ended)
    }
}

impl Drop for SckSound {
    fn drop(&mut self) {
        stop(&self.stream);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Without the permission (a CI runner has none) the system either refuses to list or lists
    /// no window of the game; the search ends with a reason and never hangs. With it, a process
    /// that does not exist has no window either.
    #[test]
    fn a_process_without_a_window_is_not_found_and_the_search_ends() {
        let target = WindowTarget {
            window: None,
            pids: vec![u32::MAX - 5],
            title_contains: String::new(),
        };
        let started = Instant::now();
        let error = match find_window(&target) {
            Ok(_) => panic!("a window for a process that does not exist"),
            Err(error) => error,
        };
        assert!(
            is_permission_error(&error)
                || error.contains("not found")
                || error.contains("listed nothing"),
            "{error}"
        );
        assert!(started.elapsed() < Duration::from_secs(60));
    }

    #[test]
    fn the_permission_error_is_recognised_by_its_words() {
        assert!(is_permission_error(&format!("{DECLINED} (code -3801)")));
        assert!(!is_permission_error("the game's window was not found"));
    }
}
