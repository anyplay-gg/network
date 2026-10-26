//! The viewer's speakers on Windows, through cpal (WASAPI): the game's 48 kHz stereo, brought
//! to the device's rate and channels.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, mpsc};
use std::thread::JoinHandle;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SampleFormat, SizedSample, StreamConfig};

use crate::sound::{Resampler, SoundOut};

const RATE: u32 = 48_000;
/// The most kept for the device: 200 ms of stereo.
const MAX_QUEUED: usize = RATE as usize * 2 / 5;

type Ring = Arc<Mutex<VecDeque<f32>>>;

/// The system's default speakers.
pub struct Speakers {
    ring: Ring,
    stop: Option<mpsc::Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl Speakers {
    pub fn open() -> Result<Speakers, String> {
        let ring: Ring = Arc::default();
        let (ready_tx, ready) = mpsc::channel::<Result<(), String>>();
        let (stop_tx, stop) = mpsc::channel::<()>();
        let r = ring.clone();
        // A cpal stream stays on the thread that made it.
        let thread = std::thread::Builder::new()
            .name("speakers".into())
            .spawn(move || match start(&r) {
                Ok(stream) => {
                    let _ = ready_tx.send(Ok(()));
                    let _ = stop.recv();
                    drop(stream);
                }
                Err(why) => {
                    let _ = ready_tx.send(Err(why));
                }
            })
            .map_err(|e| format!("the speakers' thread: {e}"))?;
        ready
            .recv()
            .map_err(|_| "the speakers' thread ended".to_owned())??;
        Ok(Speakers {
            ring,
            stop: Some(stop_tx),
            thread: Some(thread),
        })
    }
}

impl SoundOut for Speakers {
    fn queued(&self) -> usize {
        self.ring.lock().map(|r| r.len() / 2).unwrap_or(0)
    }

    fn play(&mut self, samples: &[f32]) {
        if let Ok(mut ring) = self.ring.lock()
            && ring.len() + samples.len() <= MAX_QUEUED
        {
            ring.extend(samples);
        }
    }
}

impl Drop for Speakers {
    fn drop(&mut self) {
        drop(self.stop.take());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn start(ring: &Ring) -> Result<cpal::Stream, String> {
    let device = cpal::default_host()
        .default_output_device()
        .ok_or("no speakers")?;
    let config = device
        .default_output_config()
        .map_err(|e| format!("the speakers: {e}"))?;
    let format = config.sample_format();
    let config: StreamConfig = config.into();
    let stream = match format {
        SampleFormat::F32 => build::<f32>(&device, config, ring),
        SampleFormat::I16 => build::<i16>(&device, config, ring),
        SampleFormat::U16 => build::<u16>(&device, config, ring),
        SampleFormat::I32 => build::<i32>(&device, config, ring),
        other => Err(format!("speaker sample format {other} is not supported")),
    }?;
    stream.play().map_err(|e| format!("the speakers: {e}"))?;
    Ok(stream)
}

fn build<T>(
    device: &cpal::Device,
    config: StreamConfig,
    ring: &Ring,
) -> Result<cpal::Stream, String>
where
    T: SizedSample + FromSample<f32>,
{
    let channels = usize::from(config.channels);
    let mut resampler = Resampler::new(config.sample_rate);
    let ring = ring.clone();
    device
        .build_output_stream(
            config,
            move |data: &mut [T], _| {
                let Ok(mut ring) = ring.lock() else {
                    return;
                };
                for out in data.chunks_mut(channels) {
                    let [l, r] = resampler.frame(&mut ring);
                    match out {
                        [mono] => *mono = T::from_sample((l + r) / 2.0),
                        [left, right, rest @ ..] => {
                            *left = T::from_sample(l);
                            *right = T::from_sample(r);
                            rest.fill(T::from_sample(0.0));
                        }
                        [] => {}
                    }
                }
            },
            |e| eprintln!("speakers: {e}"),
            None,
        )
        .map_err(|e| format!("the speakers: {e}"))
}
