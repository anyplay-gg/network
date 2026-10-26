//! AAC-LC through the system: the AAC encoder of Windows Media Foundation.
//!
//! Windows ships it, so the worker carries no AAC code and owes no AAC licence of its own (the
//! same rule as the system's H.264 encoders, ADR 0027). It is not on every edition (the "N"
//! editions of Windows lack Media Foundation's codecs until the Media Feature Pack is added):
//! there [`MfAac::open`] fails and the broadcast carries silence, which is said in the log.
//!
//! The format is fixed to what RTMP services take and what `flv::AAC_CONFIG` announces: AAC-LC,
//! 48 kHz, stereo, raw frames (no ADTS), 128 kbit/s.
//!
//! Everything here runs on the thread that made the object; none of it is `Send`.

use std::mem::ManuallyDrop;
use windows::Win32::Media::MediaFoundation::{
    IMFTransform, MF_E_NOTACCEPTING, MF_E_TRANSFORM_NEED_MORE_INPUT, MF_E_TRANSFORM_STREAM_CHANGE,
    MF_MT_AAC_AUDIO_PROFILE_LEVEL_INDICATION, MF_MT_AAC_PAYLOAD_TYPE,
    MF_MT_AUDIO_AVG_BYTES_PER_SECOND, MF_MT_AUDIO_BITS_PER_SAMPLE, MF_MT_AUDIO_BLOCK_ALIGNMENT,
    MF_MT_AUDIO_NUM_CHANNELS, MF_MT_AUDIO_SAMPLES_PER_SECOND, MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE,
    MF_MT_USER_DATA, MFAudioFormat_AAC, MFAudioFormat_PCM, MFCreateMediaType, MFMediaType_Audio,
    MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, MFT_MESSAGE_NOTIFY_START_OF_STREAM, MFT_OUTPUT_DATA_BUFFER,
};
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance};
use windows::core::GUID;

use super::audio::{AudioEncoder, CHANNELS};
use super::flv;
use crate::mf::{read_sample, sample_with_buffer, start, text};

/// The system's AAC encoder, `CLSID_AACMFTEncoder`.
const AAC_ENCODER: GUID = GUID::from_u128(0x93af0c51_2275_45d2_a35b_f2ba21caed00);
/// AAC-LC at level 2 (`MF_MT_AAC_AUDIO_PROFILE_LEVEL_INDICATION`).
const PROFILE_LC_L2: u32 = 0x29;

/// The system's AAC encoder, fed 48 kHz stereo.
pub struct MfAac {
    transform: IMFTransform,
    output_size: u32,
    /// Frames (one sample per channel) given so far, for the samples' times.
    fed: u64,
}

impl MfAac {
    pub fn open() -> Result<MfAac, String> {
        start()?;
        // SAFETY: plain COM calls on objects this function creates.
        unsafe {
            let transform: IMFTransform =
                CoCreateInstance(&AAC_ENCODER, None, CLSCTX_INPROC_SERVER)
                    .map_err(|e| format!("this PC has no AAC encoder: {}", text(e)))?;
            let rate = flv::AAC_RATE as u32;
            let channels = CHANNELS as u32;

            let output = MFCreateMediaType().map_err(text)?;
            output
                .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)
                .map_err(text)?;
            output
                .SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_AAC)
                .map_err(text)?;
            output
                .SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)
                .map_err(text)?;
            output
                .SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, rate)
                .map_err(text)?;
            output
                .SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, channels)
                .map_err(text)?;
            output
                .SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, flv::AAC_KBPS * 1000 / 8)
                .map_err(text)?;
            output
                .SetUINT32(&MF_MT_AUDIO_BLOCK_ALIGNMENT, 1)
                .map_err(text)?;
            // Raw AAC frames, not ADTS: what an FLV audio message carries.
            output.SetUINT32(&MF_MT_AAC_PAYLOAD_TYPE, 0).map_err(text)?;
            output
                .SetUINT32(&MF_MT_AAC_AUDIO_PROFILE_LEVEL_INDICATION, PROFILE_LC_L2)
                .map_err(text)?;
            transform
                .SetOutputType(0, &output, 0)
                .map_err(|e| format!("the AAC encoder refused its output: {}", text(e)))?;

            let input = MFCreateMediaType().map_err(text)?;
            input
                .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)
                .map_err(text)?;
            input
                .SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_PCM)
                .map_err(text)?;
            input
                .SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)
                .map_err(text)?;
            input
                .SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, rate)
                .map_err(text)?;
            input
                .SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, channels)
                .map_err(text)?;
            input
                .SetUINT32(&MF_MT_AUDIO_BLOCK_ALIGNMENT, channels * 2)
                .map_err(text)?;
            input
                .SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, rate * channels * 2)
                .map_err(text)?;
            transform
                .SetInputType(0, &input, 0)
                .map_err(|e| format!("the AAC encoder refused its input: {}", text(e)))?;

            // The two bytes that tell a player how to read the frames must be the ones the
            // stream announces (`flv::AAC_CONFIG`); the encoder's type ends with them.
            if let Some(config) = audio_specific_config(&transform)
                && config != flv::AAC_CONFIG
            {
                return Err(format!(
                    "the AAC encoder's configuration {config:02x?} is not the announced one"
                ));
            }

            let info = transform.GetOutputStreamInfo(0).map_err(text)?;
            let _ = transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0);
            let _ = transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0);
            Ok(MfAac {
                transform,
                output_size: info.cbSize.max(8192),
                fed: 0,
            })
        }
    }

    /// Every frame the encoder has ready.
    fn drain(&mut self, out: &mut Vec<Vec<u8>>) -> Result<(), String> {
        loop {
            let sample = sample_with_buffer(self.output_size)?;
            let mut buffer = [MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: 0,
                pSample: ManuallyDrop::new(Some(sample)),
                dwStatus: 0,
                pEvents: ManuallyDrop::new(None),
            }];
            let mut status = 0u32;
            // SAFETY: the buffer holds a sample this call owns; after the call it is moved out
            // exactly once and the unused event collection is dropped.
            let (result, sample) = unsafe {
                let result = self.transform.ProcessOutput(0, &mut buffer, &mut status);
                let sample = ManuallyDrop::take(&mut buffer[0].pSample);
                drop(ManuallyDrop::take(&mut buffer[0].pEvents));
                (result, sample)
            };
            match result {
                Ok(()) => {
                    if let Some(sample) = sample {
                        let frame = read_sample(&sample)?;
                        if !frame.is_empty() {
                            out.push(frame);
                        }
                    }
                }
                Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(()),
                Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => return Ok(()),
                Err(e) => return Err(text(e)),
            }
        }
    }
}

impl AudioEncoder for MfAac {
    fn encode(&mut self, samples: &[f32]) -> Result<Vec<Vec<u8>>, String> {
        let mut out = Vec::new();
        if samples.is_empty() {
            return Ok(out);
        }
        let mut pcm = Vec::with_capacity(samples.len() * 2);
        for sample in samples {
            pcm.extend_from_slice(&((sample.clamp(-1.0, 1.0) * 32767.0) as i16).to_le_bytes());
        }
        let input = sample_with_buffer(pcm.len() as u32)?;
        // SAFETY: the buffer is locked for the copy, which stays inside its capacity, and
        // unlocked after it.
        unsafe {
            let buffer = input.GetBufferByIndex(0).map_err(text)?;
            let mut at = std::ptr::null_mut();
            buffer.Lock(&mut at, None, None).map_err(text)?;
            std::ptr::copy_nonoverlapping(pcm.as_ptr(), at, pcm.len());
            let _ = buffer.Unlock();
            buffer.SetCurrentLength(pcm.len() as u32).map_err(text)?;
            // The encoder wants times (100 ns units) on its input.
            let frames = (samples.len() / CHANNELS) as u64;
            let rate = flv::AAC_RATE;
            input
                .SetSampleTime((self.fed * 10_000_000 / rate) as i64)
                .map_err(text)?;
            input
                .SetSampleDuration((frames * 10_000_000 / rate) as i64)
                .map_err(text)?;
            self.fed += frames;
        }
        // An encoder that holds finished frames does not take more until they are read.
        for _ in 0..4 {
            // SAFETY: plain COM call; the sample outlives it.
            match unsafe { self.transform.ProcessInput(0, &input, 0) } {
                Ok(()) => {
                    self.drain(&mut out)?;
                    return Ok(out);
                }
                Err(e) if e.code() == MF_E_NOTACCEPTING => self.drain(&mut out)?,
                Err(e) => return Err(text(e)),
            }
        }
        Err("the AAC encoder takes no more sound".into())
    }
}

/// The two bytes of `AudioSpecificConfig` at the end of the encoder's output type, when it has
/// them (after the 12 bytes of `HEAACWAVEINFO`).
fn audio_specific_config(transform: &IMFTransform) -> Option<[u8; 2]> {
    // SAFETY: the blob is read into a buffer of the size the type reports.
    unsafe {
        let current = transform.GetOutputCurrentType(0).ok()?;
        let size = current.GetBlobSize(&MF_MT_USER_DATA).ok()? as usize;
        let mut blob = vec![0u8; size];
        current.GetBlob(&MF_MT_USER_DATA, &mut blob, None).ok()?;
        (size >= 14).then(|| [blob[12], blob[13]])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The system's encoder gives raw AAC frames at the pace of the input. Skips where the
    /// system has none (an "N" edition or a server image).
    #[test]
    fn a_tone_becomes_raw_aac_frames() {
        let Ok(mut encoder) = MfAac::open() else {
            eprintln!("skipped: no AAC encoder here");
            return;
        };
        let mut frames = Vec::new();
        let mut produced = 0usize;
        // Two seconds of a 440 Hz tone, in 10 ms pieces as the capture delivers it.
        for _ in 0..200 {
            let piece: Vec<f32> = (0..480)
                .flat_map(|_| {
                    let t = produced as f32 / 48_000.0;
                    produced += 1;
                    let s = (t * 440.0 * std::f32::consts::TAU).sin() * 0.5;
                    [s, s]
                })
                .collect();
            frames.extend(encoder.encode(&piece).unwrap());
        }
        // 96 000 samples are 93 frames of 1024; the encoder holds back its first one or two.
        assert!((88..=94).contains(&frames.len()), "{} frames", frames.len());
        for frame in &frames[2..] {
            assert!(
                frame.len() > 40 && frame.len() < 2000,
                "{} bytes",
                frame.len()
            );
            // Not ADTS (which starts with 0xFFF).
            assert!(frame[0] != 0xFF || frame[1] & 0xF0 != 0xF0);
        }
    }

    #[test]
    fn silence_still_encodes_to_frames() {
        let Ok(mut encoder) = MfAac::open() else {
            return;
        };
        let mut frames = Vec::new();
        for _ in 0..50 {
            frames.extend(encoder.encode(&vec![0.0; 480 * 2]).unwrap());
        }
        assert!(frames.len() >= 20);
    }
}
