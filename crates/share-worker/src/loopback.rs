//! The game's sound on Windows: process loopback capture of the game's process tree (WASAPI,
//! Windows 10 build 20348 and later), as 48 kHz stereo float. Only the game is heard, not the
//! rest of the PC (voice, music, notifications).

use std::mem::ManuallyDrop;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::thread::JoinHandle;
use std::time::Duration;

use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Media::Audio::{
    AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
    AUDCLNT_STREAMFLAGS_EVENTCALLBACK, AUDCLNT_STREAMFLAGS_LOOPBACK, AUDIOCLIENT_ACTIVATION_PARAMS,
    AUDIOCLIENT_ACTIVATION_PARAMS_0, AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
    AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS, ActivateAudioInterfaceAsync,
    IActivateAudioInterfaceAsyncOperation, IActivateAudioInterfaceCompletionHandler,
    IActivateAudioInterfaceCompletionHandler_Impl, IAudioCaptureClient, IAudioClient,
    PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE, VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
    WAVEFORMATEX,
};
use windows::Win32::Media::Multimedia::WAVE_FORMAT_IEEE_FLOAT;
use windows::Win32::System::Com::StructuredStorage::{
    PROPVARIANT, PROPVARIANT_0, PROPVARIANT_0_0, PROPVARIANT_0_0_0,
};
use windows::Win32::System::Com::{BLOB, COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
use windows::Win32::System::Variant::VT_BLOB;
use windows::core::{HRESULT, IUnknown, Interface, Ref, implement};

use crate::sound::SoundSource;

const RATE: u32 = 48_000;
const CHANNELS: u16 = 2;
/// The capture buffer: 20 ms, in 100 ns units.
const BUFFER: i64 = 200_000;
/// The most the source keeps when nobody reads: 1 second.
const MAX_KEPT: usize = RATE as usize * CHANNELS as usize;
/// How long the system may take to hand out the capture client.
const ACTIVATION: Duration = Duration::from_secs(5);

/// Captured sound between the capture thread and [`SoundSource::read`].
#[derive(Default)]
struct Captured {
    samples: Vec<f32>,
    /// Why capture stopped, once it did.
    failed: Option<String>,
}

#[derive(Default)]
struct Shared {
    captured: Mutex<Captured>,
    arrived: Condvar,
}

/// The sound of one process and every process it started.
pub struct LoopbackSource {
    shared: Arc<Shared>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl LoopbackSource {
    /// Starts capturing the sound of `pid` and its child processes.
    pub fn open(pid: u32) -> Result<LoopbackSource, String> {
        let shared = Arc::new(Shared::default());
        let stop = Arc::new(AtomicBool::new(false));
        let (started_tx, started) = mpsc::channel();
        let (s, st) = (shared.clone(), stop.clone());
        let thread = std::thread::Builder::new()
            .name("game-sound".into())
            .spawn(move || {
                // SAFETY: COM for this thread only, undone before it ends.
                let com = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
                if let Err(why) = capture(pid, &s, &st, &started_tx) {
                    let _ = started_tx.send(Err(why.clone()));
                    if let Ok(mut captured) = s.captured.lock() {
                        captured.failed = Some(why);
                    }
                    s.arrived.notify_all();
                }
                if com.is_ok() {
                    // SAFETY: pairs with the successful CoInitializeEx above.
                    unsafe { CoUninitialize() };
                }
            })
            .map_err(|e| format!("the sound thread: {e}"))?;
        let source = LoopbackSource {
            shared,
            stop,
            thread: Some(thread),
        };
        match started.recv_timeout(ACTIVATION * 2) {
            Ok(Ok(())) => Ok(source),
            Ok(Err(why)) => Err(why),
            Err(_) => {
                source.stop.store(true, Ordering::SeqCst);
                Err("the game's sound did not start in time".into())
            }
        }
    }
}

impl SoundSource for LoopbackSource {
    fn read(&mut self, out: &mut Vec<f32>) -> Result<bool, String> {
        let Ok(mut captured) = self.shared.captured.lock() else {
            return Err("the sound thread panicked".into());
        };
        if captured.samples.is_empty() && captured.failed.is_none() {
            captured = match self
                .shared
                .arrived
                .wait_timeout(captured, Duration::from_millis(20))
            {
                Ok((captured, _)) => captured,
                Err(_) => return Err("the sound thread panicked".into()),
            };
        }
        out.append(&mut captured.samples);
        match &captured.failed {
            Some(why) => Err(why.clone()),
            None => Ok(true),
        }
    }
}

impl Drop for LoopbackSource {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn text(error: windows::core::Error) -> String {
    format!("{} ({:#010x})", error.message(), error.code().0)
}

/// Tells the waiting thread that the system answered the activation.
#[implement(IActivateAudioInterfaceCompletionHandler)]
struct Activated(Mutex<Option<mpsc::Sender<()>>>);

impl IActivateAudioInterfaceCompletionHandler_Impl for Activated_Impl {
    fn ActivateCompleted(
        &self,
        _operation: Ref<IActivateAudioInterfaceAsyncOperation>,
    ) -> windows::core::Result<()> {
        if let Some(done) = self.0.lock().ok().and_then(|mut done| done.take()) {
            let _ = done.send(());
        }
        Ok(())
    }
}

/// The capture client for the sound of `pid`'s process tree.
fn activate(pid: u32) -> Result<IAudioClient, String> {
    let mut params = AUDIOCLIENT_ACTIVATION_PARAMS {
        ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
        Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
            ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                TargetProcessId: pid,
                ProcessLoopbackMode: PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE,
            },
        },
    };
    let blob = PROPVARIANT {
        Anonymous: PROPVARIANT_0 {
            Anonymous: ManuallyDrop::new(PROPVARIANT_0_0 {
                vt: VT_BLOB,
                wReserved1: 0,
                wReserved2: 0,
                wReserved3: 0,
                Anonymous: PROPVARIANT_0_0_0 {
                    blob: BLOB {
                        cbSize: size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>() as u32,
                        pBlobData: (&raw mut params).cast(),
                    },
                },
            }),
        },
    };
    let (done_tx, done) = mpsc::channel();
    let handler: IActivateAudioInterfaceCompletionHandler =
        Activated(Mutex::new(Some(done_tx))).into();
    // SAFETY: `blob` and `params` outlive the call, which copies them; the handler is a COM
    // object that the system holds until it has answered.
    let operation = unsafe {
        ActivateAudioInterfaceAsync(
            VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
            &IAudioClient::IID,
            Some(&raw const blob),
            &handler,
        )
    }
    .map_err(|e| format!("process loopback: {}", text(e)))?;
    done.recv_timeout(ACTIVATION)
        .map_err(|_| "process loopback: no answer from the system".to_owned())?;
    let mut result = HRESULT(0);
    let mut activated: Option<IUnknown> = None;
    // SAFETY: both pointers are to locals that live through the call.
    unsafe { operation.GetActivateResult(&mut result, &mut activated) }
        .map_err(|e| format!("process loopback: {}", text(e)))?;
    result
        .ok()
        .map_err(|e| format!("process loopback: {}", text(e)))?;
    activated
        .ok_or_else(|| "process loopback: no client".to_owned())?
        .cast::<IAudioClient>()
        .map_err(|e| format!("process loopback: {}", text(e)))
}

/// Captures until `stop` is set, putting the sound into `shared`.
fn capture(
    pid: u32,
    shared: &Shared,
    stop: &AtomicBool,
    started: &mpsc::Sender<Result<(), String>>,
) -> Result<(), String> {
    let client = activate(pid)?;
    let format = WAVEFORMATEX {
        wFormatTag: WAVE_FORMAT_IEEE_FLOAT as u16,
        nChannels: CHANNELS,
        nSamplesPerSec: RATE,
        nAvgBytesPerSec: RATE * u32::from(CHANNELS) * 4,
        nBlockAlign: CHANNELS * 4,
        wBitsPerSample: 32,
        cbSize: 0,
    };
    let flags = AUDCLNT_STREAMFLAGS_LOOPBACK
        | AUDCLNT_STREAMFLAGS_EVENTCALLBACK
        | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM;
    // SAFETY: `format` lives through the call.
    unsafe { client.Initialize(AUDCLNT_SHAREMODE_SHARED, flags, BUFFER, 0, &format, None) }
        .map_err(|e| format!("the sound client: {}", text(e)))?;
    // SAFETY: a plain auto-reset event, closed below.
    let event: HANDLE = unsafe { CreateEventW(None, false, false, None) }
        .map_err(|e| format!("the sound event: {}", text(e)))?;
    let result = (|| {
        // SAFETY: the event outlives the client's use of it (the client stops first).
        unsafe { client.SetEventHandle(event) }.map_err(text)?;
        // SAFETY: plain COM calls on a client that was initialised above.
        let reader: IAudioCaptureClient = unsafe { client.GetService() }.map_err(text)?;
        unsafe { client.Start() }.map_err(text)?;
        let _ = started.send(Ok(()));
        while !stop.load(Ordering::SeqCst) {
            // SAFETY: a valid event handle; a timeout only means no sound came.
            unsafe { WaitForSingleObject(event, 100) };
            read_packets(&reader, shared)?;
        }
        // SAFETY: stops the stream started above.
        unsafe { client.Stop() }.map_err(text)
    })()
    .map_err(|why| format!("the game's sound: {why}"));
    // SAFETY: the event created above, no longer used.
    let _ = unsafe { CloseHandle(event) };
    result
}

/// Moves every packet the system holds into `shared`.
fn read_packets(reader: &IAudioCaptureClient, shared: &Shared) -> Result<(), String> {
    let mut got = Vec::new();
    loop {
        // SAFETY: plain COM call.
        let size = unsafe { reader.GetNextPacketSize() }.map_err(text)?;
        if size == 0 {
            break;
        }
        let mut data = std::ptr::null_mut();
        let mut frames = 0u32;
        let mut flags = 0u32;
        // SAFETY: the pointers are to locals; the buffer is read before it is released.
        unsafe { reader.GetBuffer(&mut data, &mut frames, &mut flags, None, None) }
            .map_err(text)?;
        let count = frames as usize * usize::from(CHANNELS);
        if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 || data.is_null() {
            got.resize(got.len() + count, 0.0);
        } else {
            // SAFETY: the system hands out `frames` frames of the format asked for: 2 floats
            // each.
            let samples = unsafe { std::slice::from_raw_parts(data.cast::<f32>(), count) };
            got.extend_from_slice(samples);
        }
        // SAFETY: releases what GetBuffer handed out.
        unsafe { reader.ReleaseBuffer(frames) }.map_err(text)?;
    }
    if got.is_empty() {
        return Ok(());
    }
    if let Ok(mut captured) = shared.captured.lock() {
        captured.samples.extend_from_slice(&got);
        // Nobody reads: keep the newest second only.
        let excess = captured.samples.len().saturating_sub(MAX_KEPT);
        captured.samples.drain(..excess);
    }
    shared.arrived.notify_all();
    Ok(())
}
