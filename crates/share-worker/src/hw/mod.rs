//! Hardware H.264 encoders on Linux, loaded from the system while the worker runs (ADR 0037).
//!
//! Two backends sit behind [`anyplay_share::codec::VideoEncoder`]: NVENC for NVIDIA (the driver's
//! `libnvidia-encode.so.1`, with `libcuda.so.1`) and VA-API for Intel and AMD (`libva.so.2` and
//! `libva-drm.so.2`, with the driver Mesa or Intel ships). Nothing of either is built into the
//! worker and no codec is shipped: the libraries are opened with `dlopen` when the worker starts,
//! and a PC without them simply has no backend. [`Backend::open`] is the whole probe: a backend
//! is available when it can open an encoder, so a library without a device, a device without an
//! H.264 encoder, and a driver that refuses all come out the same way, with a reason.
//!
//! The order is NVENC, then VA-API; `ANYPLAY_HWENC` narrows it (`nvenc`, `vaapi`) or turns the
//! hardware off (`off`). The caller falls back to OpenH264 when [`open_first`] finds nothing.
//! This module is plain Rust and tested everywhere with fake backends; the two backends are
//! compiled on Linux only.

use anyplay_share::codec::VideoEncoder;

use crate::limits::EncoderSetup;

pub mod h264bits;
#[cfg(target_os = "linux")]
pub mod nvenc;
#[cfg(target_os = "linux")]
pub mod vaapi;

/// The environment variable that narrows the backends or turns them off.
pub const ENV: &str = "ANYPLAY_HWENC";

/// One way to reach a GPU's H.264 encoder.
pub trait Backend {
    /// The name shown in the log and accepted by [`ENV`].
    fn name(&self) -> &'static str;
    /// Opens an encoder for `setup` (already lowered to the hardware limits), or says why not.
    fn open(&self, setup: EncoderSetup) -> Result<Box<dyn VideoEncoder>, String>;
}

/// The backends this system has code for, in the order they are tried, after [`ENV`].
pub fn backends() -> Vec<Box<dyn Backend>> {
    select(std::env::var(ENV).ok().as_deref(), all())
}

#[cfg(target_os = "linux")]
fn all() -> Vec<Box<dyn Backend>> {
    vec![Box::new(nvenc::Nvenc), Box::new(vaapi::Vaapi)]
}

#[cfg(not(target_os = "linux"))]
fn all() -> Vec<Box<dyn Backend>> {
    Vec::new()
}

/// `all` narrowed by the value of [`ENV`]: unset, empty or `auto` keeps all; `off` none; a
/// backend's name only that one (an unknown name keeps none, so a typo does not silently pick
/// a different GPU).
pub fn select(value: Option<&str>, all: Vec<Box<dyn Backend>>) -> Vec<Box<dyn Backend>> {
    match value.map(str::trim) {
        None | Some("") | Some("auto") => all,
        Some(name) => all
            .into_iter()
            .filter(|backend| backend.name().eq_ignore_ascii_case(name))
            .collect(),
    }
}

/// The first backend that opens an encoder for `setup`, with its name; else every backend's
/// reason, in one line.
pub fn open_first(
    backends: &[Box<dyn Backend>],
    setup: EncoderSetup,
) -> Result<(Box<dyn VideoEncoder>, &'static str), String> {
    if backends.is_empty() {
        return Err("no hardware encoder backend is enabled".into());
    }
    let mut why = Vec::new();
    for backend in backends {
        match backend.open(setup) {
            Ok(encoder) => return Ok((encoder, backend.name())),
            Err(reason) => why.push(format!("{}: {reason}", backend.name())),
        }
    }
    Err(why.join("; "))
}

/// The encoder for a stream start on a PC that may have a GPU: the first of `backends` that
/// opens at the hardware limits (1080p60), else `software` at the software ones (720p30). Lowers
/// `start` to what the encoder gives. A hardware failure is logged and never ends the stream:
/// the software encoder is the floor. `software` is not called when hardware opened.
pub fn open_h264(
    start: &mut anyplay_share::link::Start,
    backends: &[Box<dyn Backend>],
    software: impl FnOnce(EncoderSetup) -> Result<Box<dyn VideoEncoder>, String>,
) -> Result<(Box<dyn VideoEncoder>, &'static str), String> {
    use crate::limits::Limits;
    let mut setup = Limits::HARDWARE.clamp(start);
    let mut why = None;
    if !backends.is_empty() {
        match open_first(backends, setup) {
            Ok((encoder, name)) => {
                start.width = setup.width;
                start.height = setup.height;
                start.fps = setup.fps;
                return Ok((encoder, name));
            }
            Err(reason) => {
                eprintln!("share worker: hardware H.264 failed ({reason}); using software");
                why = Some(reason);
            }
        }
    }
    setup = Limits::SOFTWARE.clamp(start);
    let encoder = software(setup).map_err(|software| match why {
        Some(hardware) => format!("hardware: {hardware}; software: {software}"),
        None => software,
    })?;
    start.width = setup.width;
    start.height = setup.height;
    start.fps = setup.fps;
    Ok((encoder, "software"))
}

/// Held while a GPU driver starts: one at a time in a process. NVIDIA's VA-API driver calls
/// `cuInit` while the loader loads it, and `cuInit` loads libraries itself, so NVENC and VA-API
/// starting at once wait on each other's lock (CUDA's and the loader's) for good.
pub(crate) fn starting_a_driver() -> std::sync::MutexGuard<'static, ()> {
    static STARTING: std::sync::Mutex<()> = std::sync::Mutex::new(());
    STARTING
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// A small setup that every hardware encoder opens, for the start-up probe.
pub const PROBE: EncoderSetup = EncoderSetup {
    width: 640,
    height: 360,
    fps: 30,
    kbps: 2000,
};

/// The name of the first backend that works on this PC, found once per process by opening (and
/// dropping) a small encoder. `None` when there is no hardware encoder; the reasons go to the
/// log, once.
pub fn available() -> Option<&'static str> {
    static FOUND: std::sync::OnceLock<Option<&'static str>> = std::sync::OnceLock::new();
    *FOUND.get_or_init(|| match open_first(&backends(), PROBE) {
        Ok((_, name)) => Some(name),
        Err(why) => {
            eprintln!("share worker: no hardware H.264 encoder ({why})");
            None
        }
    })
}

#[cfg(test)]
pub(crate) mod fake {
    use super::*;
    use anyplay_share::codec::{CodecError, EncodedFrame, Frame};
    use std::cell::RefCell;
    use std::rc::Rc;

    /// An encoder that writes the width it was opened with into every frame.
    pub struct FakeEncoder(pub EncoderSetup);

    impl VideoEncoder for FakeEncoder {
        fn codec(&self) -> &str {
            "h264"
        }
        fn encode(&mut self, frame: &Frame, force_key: bool) -> Result<EncodedFrame, CodecError> {
            Ok(EncodedFrame {
                key: force_key,
                timestamp_us: frame.timestamp_us,
                data: vec![self.0.width as u8],
            })
        }
    }

    /// A backend that works or fails with a reason, and records its attempts.
    pub struct FakeBackend {
        pub name: &'static str,
        pub result: Result<(), &'static str>,
        pub attempts: Rc<RefCell<Vec<&'static str>>>,
    }

    impl Backend for FakeBackend {
        fn name(&self) -> &'static str {
            self.name
        }
        fn open(&self, setup: EncoderSetup) -> Result<Box<dyn VideoEncoder>, String> {
            self.attempts.borrow_mut().push(self.name);
            self.result
                .map(|()| Box::new(FakeEncoder(setup)) as Box<dyn VideoEncoder>)
                .map_err(str::to_owned)
        }
    }

    pub fn backend(
        name: &'static str,
        result: Result<(), &'static str>,
        attempts: &Rc<RefCell<Vec<&'static str>>>,
    ) -> Box<dyn Backend> {
        Box::new(FakeBackend {
            name,
            result,
            attempts: attempts.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::fake::backend;
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    fn names(list: &[Box<dyn Backend>]) -> Vec<&'static str> {
        list.iter().map(|b| b.name()).collect()
    }

    fn two(log: &Rc<RefCell<Vec<&'static str>>>) -> Vec<Box<dyn Backend>> {
        vec![backend("nvenc", Ok(()), log), backend("vaapi", Ok(()), log)]
    }

    #[test]
    fn the_variable_narrows_the_backends_and_a_typo_picks_none() {
        let log = Rc::default();
        assert_eq!(names(&select(None, two(&log))), ["nvenc", "vaapi"]);
        assert_eq!(names(&select(Some(""), two(&log))), ["nvenc", "vaapi"]);
        assert_eq!(names(&select(Some("auto"), two(&log))), ["nvenc", "vaapi"]);
        assert_eq!(names(&select(Some("VAAPI"), two(&log))), ["vaapi"]);
        assert_eq!(names(&select(Some(" nvenc "), two(&log))), ["nvenc"]);
        assert!(select(Some("off"), two(&log)).is_empty());
        assert!(select(Some("vaapo"), two(&log)).is_empty());
    }

    #[test]
    fn the_first_backend_that_opens_wins_and_later_ones_are_not_tried() {
        let log = Rc::default();
        let list = vec![
            backend("nvenc", Err("no libnvidia-encode.so.1"), &log),
            backend("vaapi", Ok(()), &log),
            backend("other", Ok(()), &log),
        ];
        let (encoder, name) = open_first(&list, PROBE).unwrap();
        assert_eq!(name, "vaapi");
        assert_eq!(encoder.codec(), "h264");
        assert_eq!(*log.borrow(), ["nvenc", "vaapi"]);
    }

    fn start(width: u32, height: u32, fps: u32) -> anyplay_share::link::Start {
        anyplay_share::link::Start {
            epoch: 0,
            codec: "h264".into(),
            width,
            height,
            fps,
            kbps: 6000,
            parity_percent: 0,
            max_payload: 0,
        }
    }

    fn software(setup: EncoderSetup) -> Result<Box<dyn VideoEncoder>, String> {
        Ok(Box::new(fake::FakeEncoder(setup)))
    }

    #[test]
    fn hardware_gets_1080p60_and_software_is_not_asked() {
        let log = Rc::default();
        let mut s = start(1920, 1080, 60);
        let (_, name) =
            open_h264(&mut s, &two(&log), |_| panic!("software must not open")).unwrap();
        assert_eq!(name, "nvenc");
        assert_eq!((s.width, s.height, s.fps), (1920, 1080, 60));
    }

    #[test]
    fn a_failing_gpu_falls_back_to_software_at_720p30() {
        let log = Rc::default();
        let list = vec![
            backend("nvenc", Err("session refused"), &log),
            backend("vaapi", Err("no render node"), &log),
        ];
        let mut s = start(1920, 1080, 60);
        let (_, name) = open_h264(&mut s, &list, software).unwrap();
        assert_eq!(name, "software");
        assert_eq!((s.width, s.height, s.fps), (1280, 720, 30));
        assert_eq!(*log.borrow(), ["nvenc", "vaapi"]);
    }

    #[test]
    fn without_a_backend_software_is_the_only_try_and_its_error_stands_alone() {
        let mut s = start(1920, 1080, 60);
        let (_, name) = open_h264(&mut s, &[], software).unwrap();
        assert_eq!(name, "software");
        let err = open_h264(&mut s, &[], |_| Err("library missing".into()))
            .err()
            .unwrap();
        assert_eq!(err, "library missing");
        let log = Rc::default();
        let list = vec![backend("vaapi", Err("no device"), &log)];
        let err = open_h264(&mut s, &list, |_| Err("library missing".into()))
            .err()
            .unwrap();
        assert_eq!(err, "hardware: vaapi: no device; software: library missing");
    }

    #[test]
    fn when_nothing_opens_every_reason_is_named() {
        let log = Rc::default();
        let list = vec![
            backend("nvenc", Err("no driver"), &log),
            backend("vaapi", Err("no render node"), &log),
        ];
        let why = open_first(&list, PROBE).err().unwrap();
        assert_eq!(why, "nvenc: no driver; vaapi: no render node");
        assert!(open_first(&[], PROBE).is_err());
    }

    /// Set in the child process of `nvenc_and_vaapi_open_at_the_same_time`.
    #[cfg(target_os = "linux")]
    const RACE_CHILD: &str = "ANYPLAY_HW_RACE_CHILD";

    #[cfg(target_os = "linux")]
    #[test]
    fn nvenc_and_vaapi_open_at_the_same_time() {
        use std::time::{Duration, Instant};
        if std::env::var_os(RACE_CHILD).is_some() {
            // Both drivers start at once, as two streams or two tests would start them.
            let start = std::sync::Arc::new(std::sync::Barrier::new(2));
            let va = start.clone();
            let vaapi = std::thread::spawn(move || {
                va.wait();
                let _ = vaapi::VaEncoder::open(PROBE);
            });
            start.wait();
            let _ = nvenc::NvEncoder::open(PROBE);
            vaapi.join().unwrap();
            return;
        }
        // In a child process: a deadlock there can be ended, here it would hang every test.
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "hw::tests::nvenc_and_vaapi_open_at_the_same_time",
            ])
            .env(RACE_CHILD, "1")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline {
            if let Some(status) = child.try_wait().unwrap() {
                assert!(status.success(), "{status}");
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = child.kill();
        let _ = child.wait();
        panic!("opening NVENC and VA-API at the same time never ended");
    }
}
