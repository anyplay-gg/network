//! The game's pictures and sound from the frame ring: the streamer's [`FrameSource`] and
//! [`SoundSource`] for share play (`spec/share-play.md`, Frame ring). The worker makes the ring;
//! the engine, told its name over the bridge, fills it.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyplay_share::codec::Frame;
use anyplay_share::link::RingReady;
use anyplay_share::ring::{Layout, Memory, RingReader};

use crate::host::{FrameSource, Next};
use crate::shm::Shm;
use crate::sound::SoundSource;

/// How long a call waits for something new in the ring.
const WAIT: Duration = Duration::from_millis(20);
/// How often it looks while it waits.
const LOOK: Duration = Duration::from_millis(1);

/// Pictures from the ring.
pub struct RingSource<M: Memory> {
    reader: RingReader<M>,
    pixels: Vec<u8>,
}

impl<M: Memory> RingSource<M> {
    pub fn new(reader: RingReader<M>) -> RingSource<M> {
        RingSource {
            reader,
            pixels: Vec::new(),
        }
    }

    /// Pictures the engine wrote that nobody took in time, or that were overwritten while they
    /// were copied.
    pub fn dropped(&self) -> u64 {
        self.reader.dropped()
    }
}

impl<M: Memory> FrameSource for RingSource<M> {
    /// The newest picture the engine wrote since the last call. A game that does not present
    /// (a loading screen, a movie that the tap does not see) gives none: the call waits a
    /// little and answers [`Next::NotYet`], so that a stop still gets through.
    fn next_frame(&mut self) -> Next {
        let deadline = Instant::now() + WAIT;
        loop {
            if let Some(picture) = self.reader.next_picture(&mut self.pixels) {
                return Next::Picture(Frame {
                    width: picture.width,
                    height: picture.height,
                    // The system clock when the worker took the picture, in microseconds,
                    // wrapping at 2^32 as the stream does: the engine's own clock is not one
                    // the other PC can be compared with. The engine's copy and the wait in the
                    // ring (a frame at most) are not in the delay figure.
                    timestamp_us: crate::wall_us() as u32,
                    bgra: std::mem::take(&mut self.pixels),
                });
            }
            if Instant::now() >= deadline {
                return Next::NotYet;
            }
            std::thread::sleep(LOOK);
        }
    }
}

/// The game's sound from the ring's audio part: 48 kHz stereo floats.
pub struct RingSound<M: Memory> {
    reader: RingReader<M>,
}

impl<M: Memory> RingSound<M> {
    pub fn new(reader: RingReader<M>) -> RingSound<M> {
        RingSound { reader }
    }
}

impl<M: Memory + Send> SoundSource for RingSound<M> {
    fn read(&mut self, out: &mut Vec<f32>) -> Result<bool, String> {
        std::thread::sleep(Duration::from_millis(10));
        self.reader.read_audio(out);
        Ok(true)
    }
}

/// The readers on a new ring and what the net service is told about it.
pub type NewRing = (RingSource<Arc<Shm>>, RingSound<Arc<Shm>>, RingReady);

/// Makes the ring for a share and the two readers on it. The mapping goes away when the last of
/// them is dropped.
pub fn create(name: &str, width: u32, height: u32, max_fps: u32) -> Result<NewRing, String> {
    let layout = Layout::new(width, height, max_fps).map_err(|e| e.to_string())?;
    let bytes = layout.total_bytes();
    let memory = Arc::new(Shm::create(name, bytes)?);
    let pictures = RingReader::create(memory.clone(), layout).map_err(|e| e.to_string())?;
    let sound = RingReader::open(memory).map_err(|e| e.to_string())?;
    let ready = RingReady {
        name: name.to_owned(),
        bytes: u32::try_from(bytes).map_err(|_| "the ring is larger than 4 GiB".to_owned())?,
        width,
        height,
        max_fps,
    };
    Ok((RingSource::new(pictures), RingSound::new(sound), ready))
}

#[cfg(test)]
mod tests {
    use anyplay_share::ring::{RingWriter, name_from_random};

    use super::*;

    fn name() -> String {
        // Tests run at the same time and a clock may tick coarsely: a counter keeps names apart.
        static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let mut random = [7u8; 16];
        random[12..].copy_from_slice(
            &NEXT
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                .to_le_bytes(),
        );
        random[..4].copy_from_slice(&std::process::id().to_le_bytes());
        random[4..12].copy_from_slice(
            &(std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos() as u64)
                .to_le_bytes(),
        );
        name_from_random(random)
    }

    #[test]
    fn the_engines_pictures_and_sound_come_out_of_the_workers_source() {
        let name = name();
        let (mut source, mut sound, ready) = create(&name, 8, 4, 60).unwrap();
        assert_eq!((ready.width, ready.height, ready.max_fps), (8, 4, 60));
        assert_eq!(ready.name, name);

        // Nothing drawn yet: the source waits a little and says so.
        assert!(matches!(source.next_frame(), Next::NotYet));

        // The engine's side, as the net service told it in `share_open`.
        let engine = Shm::open(&name, ready.bytes as usize).unwrap();
        let mut writer = RingWriter::open(engine, 8, 4, ready.bytes as usize).unwrap();
        let mut out = Vec::new();
        sound.read(&mut out).unwrap();

        let picture = vec![0x40u8; 8 * 4 * 4];
        assert!(writer.push_picture(&picture, 5_000_000, 3));
        writer.push_audio(&[0.1, -0.1, 0.2, -0.2]);
        match source.next_frame() {
            Next::Picture(frame) => {
                assert_eq!((frame.width, frame.height), (8, 4));
                // The system clock of the moment, not the engine's.
                let age = (crate::wall_us() as u32).wrapping_sub(frame.timestamp_us);
                assert!(age < 5_000_000, "stamped {age} us ago");
                assert!(frame.is_well_formed());
                assert_eq!(frame.bgra, picture);
            }
            other => panic!("a picture, got {other:?}"),
        }
        assert!(sound.read(&mut out).unwrap());
        assert_eq!(out, [0.1, -0.1, 0.2, -0.2]);
        assert_eq!(source.dropped(), 0);
    }

    #[test]
    fn a_ring_cannot_be_made_twice_and_odd_sizes_make_none() {
        let name = name();
        let first = create(&name, 8, 4, 60).unwrap();
        assert!(create(&name, 8, 4, 60).is_err());
        drop(first);
        assert!(create(&self::name(), 7, 4, 60).is_err());
        assert!(create("not a ring name", 8, 4, 60).is_err());
    }
}
