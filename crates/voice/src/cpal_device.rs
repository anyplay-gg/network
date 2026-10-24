//! The real microphone and speakers, through cpal. Compiled only with the feature `devices`.

use std::collections::VecDeque;
use std::sync::mpsc;
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, Sample, SampleFormat, SizedSample, StreamConfig};

use crate::{AudioIo, Error};

/// The microphone ring never holds more than this much, so an engine that is not pumped does not
/// grow memory.
const CAPTURE_LIMIT_MS: u32 = 300;
const PLAYBACK_LIMIT_MS: u32 = 500;

type Ring = Arc<Mutex<VecDeque<f32>>>;

fn lock(ring: &Ring) -> MutexGuard<'_, VecDeque<f32>> {
    ring.lock().unwrap_or_else(|e| e.into_inner())
}

/// The default input and output devices of the system, mixed to mono.
///
/// cpal's streams cannot always move between threads, so a thread of its own holds them and this
/// value only holds the two rings that connect to it. That makes it `Send`.
pub struct CpalDevice {
    capture: Ring,
    playback: Ring,
    capture_rate: u32,
    playback_rate: u32,
    stop: Option<mpsc::Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl CpalDevice {
    /// Open the default microphone and the default speakers and start them.
    pub fn open() -> Result<CpalDevice, Error> {
        let capture: Ring = Ring::default();
        let playback: Ring = Ring::default();
        let (ready_tx, ready_rx) = mpsc::channel::<Result<(u32, u32), Error>>();
        let (stop_tx, stop_rx) = mpsc::channel::<()>();
        let (c, p) = (capture.clone(), playback.clone());
        let thread = std::thread::Builder::new()
            .name("voice-devices".into())
            .spawn(move || match start(&c, &p) {
                Ok((rates, _streams)) => {
                    let _ = ready_tx.send(Ok(rates));
                    // The streams stop when they are dropped, after this returns.
                    let _ = stop_rx.recv();
                }
                Err(e) => {
                    let _ = ready_tx.send(Err(e));
                }
            })
            .map_err(|e| Error::Device(e.to_string()))?;
        let (capture_rate, playback_rate) = ready_rx
            .recv()
            .map_err(|_| Error::Device("the device thread ended".into()))??;
        Ok(CpalDevice {
            capture,
            playback,
            capture_rate,
            playback_rate,
            stop: Some(stop_tx),
            thread: Some(thread),
        })
    }
}

impl Drop for CpalDevice {
    fn drop(&mut self) {
        drop(self.stop.take());
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl AudioIo for CpalDevice {
    fn capture_rate(&self) -> u32 {
        self.capture_rate
    }

    fn playback_rate(&self) -> u32 {
        self.playback_rate
    }

    fn read_capture(&mut self, out: &mut Vec<f32>) {
        out.extend(lock(&self.capture).drain(..));
    }

    fn playback_queued(&self) -> usize {
        lock(&self.playback).len()
    }

    fn write_playback(&mut self, samples: &[f32]) {
        let limit = (self.playback_rate * PLAYBACK_LIMIT_MS / 1000) as usize;
        let mut ring = lock(&self.playback);
        if ring.len() + samples.len() <= limit {
            ring.extend(samples);
        }
    }
}

fn device_error(what: &str, e: impl std::fmt::Display) -> Error {
    Error::Device(format!("{what}: {e}"))
}

type Streams = (cpal::Stream, cpal::Stream);

fn start(capture: &Ring, playback: &Ring) -> Result<((u32, u32), Streams), Error> {
    let host = cpal::default_host();
    let input = host
        .default_input_device()
        .ok_or_else(|| Error::Device("no microphone".into()))?;
    let output = host
        .default_output_device()
        .ok_or_else(|| Error::Device("no speakers".into()))?;
    let in_config = input
        .default_input_config()
        .map_err(|e| device_error("microphone", e))?;
    let out_config = output
        .default_output_config()
        .map_err(|e| device_error("speakers", e))?;
    let in_format = in_config.sample_format();
    let out_format = out_config.sample_format();
    let in_stream_config: StreamConfig = in_config.into();
    let out_stream_config: StreamConfig = out_config.into();
    let rates = (in_stream_config.sample_rate, out_stream_config.sample_rate);

    let in_limit = (rates.0 * CAPTURE_LIMIT_MS / 1000) as usize;
    let input_stream = match in_format {
        SampleFormat::F32 => build_input::<f32>(&input, in_stream_config, capture, in_limit),
        SampleFormat::I16 => build_input::<i16>(&input, in_stream_config, capture, in_limit),
        SampleFormat::U16 => build_input::<u16>(&input, in_stream_config, capture, in_limit),
        SampleFormat::I32 => build_input::<i32>(&input, in_stream_config, capture, in_limit),
        other => Err(Error::Device(format!(
            "microphone sample format {other} is not supported"
        ))),
    }?;
    let output_stream = match out_format {
        SampleFormat::F32 => build_output::<f32>(&output, out_stream_config, playback),
        SampleFormat::I16 => build_output::<i16>(&output, out_stream_config, playback),
        SampleFormat::U16 => build_output::<u16>(&output, out_stream_config, playback),
        SampleFormat::I32 => build_output::<i32>(&output, out_stream_config, playback),
        other => Err(Error::Device(format!(
            "speaker sample format {other} is not supported"
        ))),
    }?;
    input_stream
        .play()
        .map_err(|e| device_error("microphone", e))?;
    output_stream
        .play()
        .map_err(|e| device_error("speakers", e))?;
    Ok((rates, (input_stream, output_stream)))
}

fn build_input<T>(
    device: &cpal::Device,
    config: StreamConfig,
    ring: &Ring,
    limit: usize,
) -> Result<cpal::Stream, Error>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    let channels = usize::from(config.channels);
    let ring = ring.clone();
    device
        .build_input_stream(
            config,
            move |data: &[T], _| {
                let mut ring = lock(&ring);
                for frame in data.chunks(channels) {
                    let sum: f32 = frame.iter().map(|s| f32::from_sample(*s)).sum();
                    if ring.len() < limit {
                        ring.push_back(sum / channels as f32);
                    }
                }
            },
            |e| eprintln!("microphone: {e}"),
            None,
        )
        .map_err(|e| device_error("microphone", e))
}

fn build_output<T>(
    device: &cpal::Device,
    config: StreamConfig,
    ring: &Ring,
) -> Result<cpal::Stream, Error>
where
    T: SizedSample + FromSample<f32>,
{
    let channels = usize::from(config.channels);
    let ring = ring.clone();
    device
        .build_output_stream(
            config,
            move |data: &mut [T], _| {
                let mut ring = lock(&ring);
                for frame in data.chunks_mut(channels) {
                    let s = ring.pop_front().unwrap_or(0.0);
                    frame.fill(T::from_sample(s));
                }
            },
            |e| eprintln!("speakers: {e}"),
            None,
        )
        .map_err(|e| device_error("speakers", e))
}
