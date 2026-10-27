//! The frame ring between the game and the share worker (`spec/share-play.md`, Frame ring).
//!
//! The worker creates a block of shared memory and reads from it; the engine maps the block by
//! its name and writes the presented pictures and the game's sound into it. One producer, one
//! consumer, no locks: a slot is written with its `seq` set to 0, then the pixels, then the
//! real `seq`, and the reader checks the `seq` before and after it copies.
//!
//! This module holds the layout and both ends' logic over a [`Memory`], a few calls that a
//! block of shared memory (in the worker) or a plain vector (in tests) can answer. The
//! operating system's mapping, and with it every line of `unsafe`, stays outside the crate.
//! [`RingWriter`] is the engine's half: the engine in C++ implements the same file, and the
//! fake game of the tests uses this one.

use std::fmt;

/// The first eight bytes of the ring.
pub const MAGIC: [u8; 8] = *b"ANYSHR00";
/// The layout this module writes and reads.
pub const LAYOUT_VERSION: u32 = 0;
/// Slots for pictures.
pub const SLOTS: u32 = 3;
/// Bytes of the ring's header: the fields below, then zeros up to this size.
pub const HEADER_LEN: usize = 64;
/// Bytes of a slot's header.
pub const SLOT_HEADER_LEN: usize = 32;
/// Pixel format 1: four bytes per pixel, blue first, SDR.
pub const FORMAT_BGRA8: u32 = 1;
/// Frames in the audio ring: about 340 ms of sound.
pub const AUDIO_FRAMES: u32 = 16_384;
/// Bytes of one stereo frame of 32-bit floats.
pub const AUDIO_FRAME_BYTES: usize = 8;
/// The largest picture: 4K.
pub const MAX_WIDTH: u32 = 3_840;
pub const MAX_HEIGHT: u32 = 2_160;
/// The highest frame rate a ring is made for.
pub const MAX_FPS: u32 = 240;
/// The longest name of a ring.
pub const MAX_NAME: usize = 64;

// Offsets in the header.
const OFF_MAGIC: usize = 0;
const OFF_VERSION: usize = 8;
const OFF_SLOTS: usize = 12;
const OFF_WIDTH: usize = 16;
const OFF_HEIGHT: usize = 20;
const OFF_STRIDE: usize = 24;
const OFF_FORMAT: usize = 28;
const OFF_FPS: usize = 32;
const OFF_AUDIO_FRAMES: usize = 36;
/// Where the engine writes how many audio frames it has written in all (a `u64`).
pub const OFF_AUDIO_WRITE: usize = 40;

// Offsets in a slot header.
const SLOT_SEQ: usize = 0;
const SLOT_PRESENT: usize = 8;
const SLOT_KIND: usize = 16;
const SLOT_FRAME: usize = 20;

/// Why a ring or its parts are refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RingError {
    /// The picture is empty, odd in width or height, above 4K, or the frame rate is 0 or above
    /// 240.
    Size,
    /// The memory is shorter than the header or the layout it describes.
    TooShort,
    /// The magic is not `ANYSHR00`.
    Magic,
    /// Another layout version, slot count or pixel format than this one.
    Unsupported,
    /// The numbers in the header do not fit together (a stride below the width, an audio ring
    /// that is not a power of two).
    Inconsistent,
}

impl fmt::Display for RingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            RingError::Size => "the ring's picture size is out of range",
            RingError::TooShort => "the memory is shorter than the ring",
            RingError::Magic => "the memory is not a frame ring",
            RingError::Unsupported => "the ring has another layout than this one",
            RingError::Inconsistent => "the ring's header is inconsistent",
        })
    }
}

impl std::error::Error for RingError {}

/// The shape of a ring: everything else (offsets, sizes) follows from it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Layout {
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub max_fps: u32,
    pub audio_frames: u32,
}

impl Layout {
    /// The layout for pictures of `width` by `height` with the usual stride and audio ring.
    pub fn new(width: u32, height: u32, max_fps: u32) -> Result<Layout, RingError> {
        // Even sizes: video is coded in pairs of pixels, and an even size keeps every slot at a
        // multiple of 8 bytes, where its 64-bit `seq` is read atomically.
        if width == 0
            || height == 0
            || !width.is_multiple_of(2)
            || !height.is_multiple_of(2)
            || width > MAX_WIDTH
            || height > MAX_HEIGHT
            || max_fps == 0
            || max_fps > MAX_FPS
        {
            return Err(RingError::Size);
        }
        Ok(Layout {
            width,
            height,
            stride: width * 4,
            max_fps,
            audio_frames: AUDIO_FRAMES,
        })
    }

    /// Bytes of a slot: its header and the picture.
    pub fn slot_bytes(&self) -> usize {
        SLOT_HEADER_LEN + self.stride as usize * self.height as usize
    }

    pub fn slot_offset(&self, slot: u32) -> usize {
        HEADER_LEN + slot as usize * self.slot_bytes()
    }

    pub fn audio_offset(&self) -> usize {
        HEADER_LEN + SLOTS as usize * self.slot_bytes()
    }

    /// The size of the whole mapping.
    pub fn total_bytes(&self) -> usize {
        self.audio_offset() + self.audio_frames as usize * AUDIO_FRAME_BYTES
    }

    /// The header as it is written into a new ring.
    pub fn header(&self) -> [u8; HEADER_LEN] {
        let mut h = [0u8; HEADER_LEN];
        h[OFF_MAGIC..OFF_MAGIC + 8].copy_from_slice(&MAGIC);
        let mut put = |at: usize, v: u32| h[at..at + 4].copy_from_slice(&v.to_le_bytes());
        put(OFF_VERSION, LAYOUT_VERSION);
        put(OFF_SLOTS, SLOTS);
        put(OFF_WIDTH, self.width);
        put(OFF_HEIGHT, self.height);
        put(OFF_STRIDE, self.stride);
        put(OFF_FORMAT, FORMAT_BGRA8);
        put(OFF_FPS, self.max_fps);
        put(OFF_AUDIO_FRAMES, self.audio_frames);
        h
    }

    /// Reads and checks a header; the ring is `len` bytes long.
    pub fn parse(header: &[u8], len: usize) -> Result<Layout, RingError> {
        if header.len() < HEADER_LEN {
            return Err(RingError::TooShort);
        }
        if header[OFF_MAGIC..OFF_MAGIC + 8] != MAGIC {
            return Err(RingError::Magic);
        }
        let get = |at: usize| {
            u32::from_le_bytes([header[at], header[at + 1], header[at + 2], header[at + 3]])
        };
        if get(OFF_VERSION) != LAYOUT_VERSION
            || get(OFF_SLOTS) != SLOTS
            || get(OFF_FORMAT) != FORMAT_BGRA8
        {
            return Err(RingError::Unsupported);
        }
        let layout = Layout {
            width: get(OFF_WIDTH),
            height: get(OFF_HEIGHT),
            stride: get(OFF_STRIDE),
            max_fps: get(OFF_FPS),
            audio_frames: get(OFF_AUDIO_FRAMES),
        };
        Layout::new(layout.width, layout.height, layout.max_fps)?;
        if layout.stride < layout.width * 4
            || layout.stride > MAX_WIDTH * 4
            || layout.audio_frames == 0
            || !layout.audio_frames.is_power_of_two()
            || layout.audio_frames > 1 << 20
        {
            return Err(RingError::Inconsistent);
        }
        if len < layout.total_bytes() {
            return Err(RingError::TooShort);
        }
        Ok(layout)
    }
}

/// True for a name the net service may hand out and the engine may open: `anyplay-` and then
/// letters, digits, `-` and `_`, 64 characters at most. Nothing that could name a path or
/// another namespace.
pub fn name_is_valid(name: &str) -> bool {
    name.len() <= MAX_NAME
        && name.len() > "anyplay-".len()
        && name.starts_with("anyplay-")
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// A ring name from 128 random bits: `anyplay-ring-` and 32 hex digits.
pub fn name_from_random(random: [u8; 16]) -> String {
    let mut name = String::from("anyplay-ring-");
    for byte in random {
        name.push_str(&format!("{byte:02x}"));
    }
    name
}

/// A block of memory that the other process writes at the same time. Reads and writes of the
/// `u64` fields are atomic with acquire and release order; the byte copies are plain.
pub trait Memory {
    fn len(&self) -> usize;
    /// Atomic load with acquire order; `offset` is a multiple of 8.
    fn load_u64(&self, offset: usize) -> u64;
    /// Atomic store with release order; `offset` is a multiple of 8.
    fn store_u64(&self, offset: usize, value: u64);
    fn read(&self, offset: usize, out: &mut [u8]);
    fn write(&self, offset: usize, data: &[u8]);

    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl<M: Memory + ?Sized> Memory for std::sync::Arc<M> {
    fn len(&self) -> usize {
        (**self).len()
    }
    fn load_u64(&self, offset: usize) -> u64 {
        (**self).load_u64(offset)
    }
    fn store_u64(&self, offset: usize, value: u64) {
        (**self).store_u64(offset, value)
    }
    fn read(&self, offset: usize, out: &mut [u8]) {
        (**self).read(offset, out)
    }
    fn write(&self, offset: usize, data: &[u8]) {
        (**self).write(offset, data)
    }
}

impl<M: Memory + ?Sized> Memory for &M {
    fn len(&self) -> usize {
        (**self).len()
    }
    fn load_u64(&self, offset: usize) -> u64 {
        (**self).load_u64(offset)
    }
    fn store_u64(&self, offset: usize, value: u64) {
        (**self).store_u64(offset, value)
    }
    fn read(&self, offset: usize, out: &mut [u8]) {
        (**self).read(offset, out)
    }
    fn write(&self, offset: usize, data: &[u8]) {
        (**self).write(offset, data)
    }
}

/// A picture taken out of the ring.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Picture {
    pub width: u32,
    pub height: u32,
    /// `width * 4`: the copy has no padding.
    pub stride: u32,
    /// The engine's monotonic clock when the game presented it, in microseconds.
    pub present_us: u64,
    /// The game's flip count.
    pub frame: u32,
    /// The slot's `seq`: grows by one for every picture written.
    pub seq: u64,
}

/// The consumer: the share worker.
pub struct RingReader<M: Memory> {
    memory: M,
    layout: Layout,
    last_seq: u64,
    audio_read: Option<u64>,
    dropped: u64,
}

/// What the audio ring gave.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AudioRead {
    /// Stereo frames appended to the buffer.
    pub frames: usize,
    /// Frames the engine overwrote before they were read.
    pub lost: u64,
}

impl<M: Memory> RingReader<M> {
    /// Opens a ring that already has its header (the worker writes it with
    /// [`RingReader::create`]; the engine only maps it).
    pub fn open(memory: M) -> Result<RingReader<M>, RingError> {
        let mut header = [0u8; HEADER_LEN];
        if memory.len() < HEADER_LEN {
            return Err(RingError::TooShort);
        }
        memory.read(0, &mut header);
        let layout = Layout::parse(&header, memory.len())?;
        Ok(RingReader {
            memory,
            layout,
            last_seq: 0,
            audio_read: None,
            dropped: 0,
        })
    }

    /// Writes the header of a new, zeroed ring and opens it.
    pub fn create(memory: M, layout: Layout) -> Result<RingReader<M>, RingError> {
        if memory.len() < layout.total_bytes() {
            return Err(RingError::TooShort);
        }
        memory.write(0, &layout.header());
        RingReader::open(memory)
    }

    pub fn layout(&self) -> Layout {
        self.layout
    }

    /// Pictures that were overwritten before they were read, or torn while they were copied.
    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    /// The newest picture that was not taken yet, copied into `pixels` (resized to the
    /// picture). `None` when there is none, or the copy was overwritten while it ran.
    pub fn next_picture(&mut self, pixels: &mut Vec<u8>) -> Option<Picture> {
        let mut best: Option<(u32, u64)> = None;
        for slot in 0..SLOTS {
            let seq = self
                .memory
                .load_u64(self.layout.slot_offset(slot) + SLOT_SEQ);
            if seq > self.last_seq && best.is_none_or(|(_, b)| seq > b) {
                best = Some((slot, seq));
            }
        }
        let (slot, seq) = best?;
        let base = self.layout.slot_offset(slot);
        let mut head = [0u8; SLOT_HEADER_LEN];
        self.memory.read(base, &mut head);
        let present_us = u64::from_le_bytes(head[SLOT_PRESENT..SLOT_PRESENT + 8].try_into().ok()?);
        let kind = u32::from_le_bytes(head[SLOT_KIND..SLOT_KIND + 4].try_into().ok()?);
        let frame = u32::from_le_bytes(head[SLOT_FRAME..SLOT_FRAME + 4].try_into().ok()?);
        let skipped = seq - self.last_seq - 1;
        if kind != 0 {
            // A kind of slot this reader does not know (1 is reserved for a GPU handle).
            self.last_seq = seq;
            self.dropped += skipped + 1;
            return None;
        }
        let (w, h, stride) = (self.layout.width, self.layout.height, self.layout.stride);
        let row = w as usize * 4;
        pixels.clear();
        pixels.resize(row * h as usize, 0);
        for y in 0..h as usize {
            self.memory.read(
                base + SLOT_HEADER_LEN + y * stride as usize,
                &mut pixels[y * row..(y + 1) * row],
            );
        }
        let again = self.memory.load_u64(base + SLOT_SEQ);
        self.last_seq = seq;
        if again != seq {
            self.dropped += skipped + 1;
            return None;
        }
        self.dropped += skipped;
        Some(Picture {
            width: w,
            height: h,
            stride: w * 4,
            present_us,
            frame,
            seq,
        })
    }

    /// Appends the sound written since the last call to `out` as interleaved stereo floats.
    /// The first call starts at the present: sound from before the worker looked is not played.
    pub fn read_audio(&mut self, out: &mut Vec<f32>) -> AudioRead {
        let ring = u64::from(self.layout.audio_frames);
        let written = self.memory.load_u64(OFF_AUDIO_WRITE);
        let Some(read) = self.audio_read else {
            self.audio_read = Some(written);
            return AudioRead::default();
        };
        if written <= read {
            return AudioRead::default();
        }
        let mut lost = 0;
        let mut read = read;
        if written - read > ring {
            lost = written - read - ring;
            read = written - ring;
        }
        let count = (written - read) as usize;
        let mut bytes = vec![0u8; count * AUDIO_FRAME_BYTES];
        let first = (read % ring) as usize;
        let until_wrap = (ring as usize - first).min(count);
        let base = self.layout.audio_offset();
        self.memory.read(
            base + first * AUDIO_FRAME_BYTES,
            &mut bytes[..until_wrap * AUDIO_FRAME_BYTES],
        );
        if until_wrap < count {
            self.memory
                .read(base, &mut bytes[until_wrap * AUDIO_FRAME_BYTES..]);
        }
        out.extend(
            bytes
                .as_chunks::<4>()
                .0
                .iter()
                .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])),
        );
        self.audio_read = Some(written);
        AudioRead {
            frames: count,
            lost,
        }
    }
}

/// The producer: what the engine does. The C++ in AnyPS5 writes the same bytes.
pub struct RingWriter<M: Memory> {
    memory: M,
    layout: Layout,
    seq: u64,
    audio_written: u64,
}

impl<M: Memory> RingWriter<M> {
    /// Maps a ring the worker made and checks it against what the net service asked for in
    /// `share_open`: the sizes must match and the mapping must be at least `ring_bytes` long.
    pub fn open(
        memory: M,
        width: u32,
        height: u32,
        ring_bytes: usize,
    ) -> Result<RingWriter<M>, RingError> {
        if memory.len() < HEADER_LEN || memory.len() < ring_bytes {
            return Err(RingError::TooShort);
        }
        let mut header = [0u8; HEADER_LEN];
        memory.read(0, &mut header);
        let layout = Layout::parse(&header, memory.len())?;
        if layout.width != width || layout.height != height {
            return Err(RingError::Inconsistent);
        }
        let audio_written = memory.load_u64(OFF_AUDIO_WRITE);
        Ok(RingWriter {
            memory,
            layout,
            seq: 0,
            audio_written,
        })
    }

    pub fn layout(&self) -> Layout {
        self.layout
    }

    /// Writes one picture of exactly the ring's size over the oldest slot. Never waits.
    pub fn push_picture(&mut self, bgra: &[u8], present_us: u64, frame: u32) -> bool {
        let (h, stride) = (self.layout.height as usize, self.layout.stride as usize);
        let row = self.layout.width as usize * 4;
        if bgra.len() != row * h {
            return false;
        }
        self.seq += 1;
        let slot = ((self.seq - 1) % u64::from(SLOTS)) as u32;
        let base = self.layout.slot_offset(slot);
        self.memory.store_u64(base + SLOT_SEQ, 0);
        for y in 0..h {
            self.memory.write(
                base + SLOT_HEADER_LEN + y * stride,
                &bgra[y * row..(y + 1) * row],
            );
        }
        let mut head = [0u8; SLOT_HEADER_LEN - 8];
        head[SLOT_PRESENT - 8..SLOT_PRESENT].copy_from_slice(&present_us.to_le_bytes());
        head[SLOT_KIND - 8..SLOT_KIND - 4].copy_from_slice(&0u32.to_le_bytes());
        head[SLOT_FRAME - 8..SLOT_FRAME - 4].copy_from_slice(&frame.to_le_bytes());
        self.memory.write(base + 8, &head);
        self.memory.store_u64(base + SLOT_SEQ, self.seq);
        true
    }

    /// Appends stereo frames of floats to the audio ring.
    pub fn push_audio(&mut self, stereo: &[f32]) {
        let ring = u64::from(self.layout.audio_frames);
        let frames = stereo.len() / 2;
        let base = self.layout.audio_offset();
        for (i, pair) in stereo.as_chunks::<2>().0.iter().enumerate() {
            let at = ((self.audio_written + i as u64) % ring) as usize;
            let mut frame = [0u8; AUDIO_FRAME_BYTES];
            frame[..4].copy_from_slice(&pair[0].to_le_bytes());
            frame[4..].copy_from_slice(&pair[1].to_le_bytes());
            self.memory.write(base + at * AUDIO_FRAME_BYTES, &frame);
        }
        self.audio_written += frames as u64;
        self.memory.store_u64(OFF_AUDIO_WRITE, self.audio_written);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// Plain memory for tests. `on_read` runs inside every byte copy out of the ring, so a test
    /// can write into the ring while the reader copies.
    struct Vec8 {
        bytes: Mutex<Vec<u8>>,
        reads: AtomicU64,
        #[allow(clippy::type_complexity)]
        hook: Mutex<Option<Box<dyn FnMut(u64, &Vec8) + Send>>>,
    }

    impl Vec8 {
        fn new(len: usize) -> Vec8 {
            Vec8 {
                bytes: Mutex::new(vec![0; len]),
                reads: AtomicU64::new(0),
                hook: Mutex::new(None),
            }
        }
    }

    impl Memory for Vec8 {
        fn len(&self) -> usize {
            self.bytes.lock().unwrap().len()
        }
        fn load_u64(&self, offset: usize) -> u64 {
            let b = self.bytes.lock().unwrap();
            u64::from_le_bytes(b[offset..offset + 8].try_into().unwrap())
        }
        fn store_u64(&self, offset: usize, value: u64) {
            self.bytes.lock().unwrap()[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
        }
        fn read(&self, offset: usize, out: &mut [u8]) {
            let n = self.reads.fetch_add(1, Ordering::SeqCst);
            let hook = self.hook.lock().unwrap().take();
            if let Some(mut hook) = hook {
                hook(n, self);
                *self.hook.lock().unwrap() = Some(hook);
            }
            let b = self.bytes.lock().unwrap();
            out.copy_from_slice(&b[offset..offset + out.len()]);
        }
        fn write(&self, offset: usize, data: &[u8]) {
            self.bytes.lock().unwrap()[offset..offset + data.len()].copy_from_slice(data);
        }
    }

    fn layout() -> Layout {
        Layout::new(4, 2, 60).unwrap()
    }

    fn picture(fill: u8) -> Vec<u8> {
        vec![fill; 4 * 4 * 2]
    }

    #[test]
    fn the_layout_is_the_one_in_the_spec() {
        let l = layout();
        let h = l.header();
        assert_eq!(&h[..8], b"ANYSHR00");
        // layout version, slots, width, height, stride, format, fps, audio ring
        let words: Vec<u32> = (2..10)
            .map(|i| u32::from_le_bytes(h[i * 4..i * 4 + 4].try_into().unwrap()))
            .collect();
        assert_eq!(words, [0, 3, 4, 2, 16, 1, 60, 16_384]);
        assert!(h[40..].iter().all(|b| *b == 0));
        assert_eq!(l.slot_bytes(), 32 + 16 * 2);
        assert_eq!(l.slot_offset(0), 64);
        assert_eq!(l.slot_offset(2), 64 + 2 * 64);
        assert_eq!(l.audio_offset(), 64 + 3 * 64);
        assert_eq!(l.total_bytes(), 64 + 3 * 64 + 16_384 * 8);
    }

    #[test]
    fn a_header_reads_back_and_is_checked() {
        let l = layout();
        assert_eq!(Layout::parse(&l.header(), l.total_bytes()), Ok(l));
        assert_eq!(
            Layout::parse(&l.header(), l.total_bytes() - 1),
            Err(RingError::TooShort)
        );
        assert_eq!(Layout::parse(&[0; 10], 100), Err(RingError::TooShort));
        let mut bad = l.header();
        bad[0] = b'X';
        assert_eq!(Layout::parse(&bad, l.total_bytes()), Err(RingError::Magic));
        for (at, value) in [(8usize, 1u32), (12, 2), (28, 2)] {
            let mut bad = l.header();
            bad[at..at + 4].copy_from_slice(&value.to_le_bytes());
            assert_eq!(
                Layout::parse(&bad, l.total_bytes()),
                Err(RingError::Unsupported)
            );
        }
        let mut stride = l.header();
        stride[24..28].copy_from_slice(&8u32.to_le_bytes());
        assert_eq!(
            Layout::parse(&stride, l.total_bytes()),
            Err(RingError::Inconsistent)
        );
        let mut audio = l.header();
        audio[36..40].copy_from_slice(&1000u32.to_le_bytes());
        assert_eq!(
            Layout::parse(&audio, l.total_bytes()),
            Err(RingError::Inconsistent)
        );
        let mut huge = l.header();
        huge[16..20].copy_from_slice(&100_000u32.to_le_bytes());
        assert_eq!(Layout::parse(&huge, usize::MAX), Err(RingError::Size));
    }

    #[test]
    fn sizes_out_of_range_make_no_layout() {
        assert!(Layout::new(0, 10, 60).is_err());
        assert!(Layout::new(10, 0, 60).is_err());
        assert!(Layout::new(3_841, 10, 60).is_err());
        assert!(Layout::new(10, 2_161, 60).is_err());
        assert!(Layout::new(10, 10, 0).is_err());
        assert!(Layout::new(10, 10, 241).is_err());
        assert!(Layout::new(11, 10, 60).is_err());
        assert!(Layout::new(10, 11, 60).is_err());
        // Every slot starts on a multiple of 8 bytes, for any even size.
        for (w, h) in [
            (2, 2),
            (4, 2),
            (1280, 720),
            (1366, 768),
            (1920, 1080),
            (3_840, 2_160),
        ] {
            let l = Layout::new(w, h, 60).unwrap();
            for slot in 0..SLOTS {
                assert_eq!(l.slot_offset(slot) % 8, 0, "{w}x{h}");
            }
        }
        let big = Layout::new(3_840, 2_160, 60).unwrap();
        assert!(big.total_bytes() < u32::MAX as usize);
    }

    #[test]
    fn names_are_plain() {
        assert!(name_is_valid("anyplay-ring-0123456789abcdef"));
        assert!(name_is_valid(&name_from_random([0xAB; 16])));
        assert_eq!(name_from_random([0; 16]).len(), 13 + 32);
        let long = format!("anyplay-{}", "a".repeat(60));
        for bad in [
            "",
            "anyplay-",
            "ring-1",
            "anyplay-a/b",
            "anyplay-a\\b",
            "Global\\anyplay-x",
            "anyplay-a b",
            "anyplay-\u{e4}",
            long.as_str(),
        ] {
            assert!(!name_is_valid(bad), "{bad:?}");
        }
    }

    #[test]
    fn a_picture_goes_from_the_engine_to_the_worker() {
        let mem = Vec8::new(layout().total_bytes());
        let mut reader = RingReader::create(&mem, layout()).unwrap();
        let mut writer = RingWriter::open(&mem, 4, 2, layout().total_bytes()).unwrap();
        let mut pixels = Vec::new();
        assert_eq!(reader.next_picture(&mut pixels), None);
        assert!(writer.push_picture(&picture(7), 123_456, 99));
        let got = reader.next_picture(&mut pixels).unwrap();
        assert_eq!(
            (
                got.width,
                got.height,
                got.stride,
                got.present_us,
                got.frame,
                got.seq
            ),
            (4, 2, 16, 123_456, 99, 1)
        );
        assert_eq!(pixels, picture(7));
        assert_eq!(reader.next_picture(&mut pixels), None, "taken already");
        assert_eq!(reader.dropped(), 0);
    }

    #[test]
    fn the_reader_takes_the_newest_and_counts_what_it_skipped() {
        let mem = Vec8::new(layout().total_bytes());
        let mut reader = RingReader::create(&mem, layout()).unwrap();
        let mut writer = RingWriter::open(&mem, 4, 2, 0).unwrap();
        for i in 1..=5u8 {
            writer.push_picture(&picture(i), i as u64, i as u32);
        }
        let mut pixels = Vec::new();
        let got = reader.next_picture(&mut pixels).unwrap();
        assert_eq!((got.seq, got.frame), (5, 5));
        assert_eq!(pixels, picture(5));
        assert_eq!(reader.dropped(), 4);
        assert_eq!(reader.next_picture(&mut pixels), None);
    }

    #[test]
    fn the_ring_overwrites_the_oldest_and_never_waits() {
        let mem = Vec8::new(layout().total_bytes());
        // No header yet: refused, and nothing was written.
        assert_eq!(
            RingWriter::open(&mem, 4, 2, 0).err(),
            Some(RingError::Magic)
        );
        let mut reader = RingReader::create(&mem, layout()).unwrap();
        let mut writer = RingWriter::open(&mem, 4, 2, 0).unwrap();
        for i in 1..=3u8 {
            writer.push_picture(&picture(i), 0, 0);
        }
        // Nobody reads; ten more pictures are written without a stop.
        for i in 4..=13u8 {
            assert!(writer.push_picture(&picture(i), 0, 0));
        }
        let mut pixels = Vec::new();
        assert_eq!(reader.next_picture(&mut pixels).unwrap().seq, 13);
        assert_eq!(pixels, picture(13));
    }

    #[test]
    fn a_picture_overwritten_while_it_is_copied_is_dropped_whole() {
        let mem = Vec8::new(layout().total_bytes());
        let mut reader = RingReader::create(&mem, layout()).unwrap();
        let mut writer = RingWriter::open(&mem, 4, 2, 0).unwrap();
        writer.push_picture(&picture(1), 0, 0);
        // While the reader copies slot 0 (reads 1 and 2 are its header and first row), the
        // engine zeroes the slot's seq, as it does when it comes round to write it again.
        let start = mem.reads.load(Ordering::SeqCst);
        *mem.hook.lock().unwrap() = Some(Box::new(move |n, m| {
            if n == start + 2 {
                m.store_u64(64, 0);
            }
        }));
        let mut pixels = Vec::new();
        assert_eq!(reader.next_picture(&mut pixels), None);
        assert_eq!(reader.dropped(), 1);
        // The next picture is fine.
        *mem.hook.lock().unwrap() = None;
        writer.push_picture(&picture(2), 0, 0);
        assert_eq!(reader.next_picture(&mut pixels).unwrap().seq, 2);
        assert_eq!(pixels, picture(2));
    }

    #[test]
    fn a_slot_of_another_kind_is_skipped() {
        let mem = Vec8::new(layout().total_bytes());
        let mut reader = RingReader::create(&mem, layout()).unwrap();
        let mut writer = RingWriter::open(&mem, 4, 2, 0).unwrap();
        writer.push_picture(&picture(1), 0, 0);
        mem.write(64 + SLOT_KIND, &1u32.to_le_bytes());
        let mut pixels = Vec::new();
        assert_eq!(reader.next_picture(&mut pixels), None);
        assert_eq!(reader.dropped(), 1);
    }

    #[test]
    fn a_picture_of_the_wrong_size_is_not_written() {
        let mem = Vec8::new(layout().total_bytes());
        RingReader::create(&mem, layout()).unwrap();
        let mut writer = RingWriter::open(&mem, 4, 2, 0).unwrap();
        assert!(!writer.push_picture(&[0; 10], 0, 0));
        assert!(!writer.push_picture(&[0; 100], 0, 0));
    }

    #[test]
    fn the_engine_checks_the_ring_against_what_it_was_asked() {
        let l = layout();
        let mem = Vec8::new(l.total_bytes());
        RingReader::create(&mem, l).unwrap();
        assert!(RingWriter::open(&mem, 4, 2, l.total_bytes()).is_ok());
        assert_eq!(
            RingWriter::open(&mem, 5, 2, 0).err(),
            Some(RingError::Inconsistent)
        );
        assert_eq!(
            RingWriter::open(&mem, 4, 2, l.total_bytes() + 1).err(),
            Some(RingError::TooShort)
        );
        let short = Vec8::new(10);
        assert_eq!(
            RingWriter::open(&short, 4, 2, 0).err(),
            Some(RingError::TooShort)
        );
    }

    #[test]
    fn sound_goes_through_the_audio_ring() {
        let mem = Vec8::new(layout().total_bytes());
        let mut reader = RingReader::create(&mem, layout()).unwrap();
        let mut writer = RingWriter::open(&mem, 4, 2, 0).unwrap();
        let mut out = Vec::new();
        // The first look starts at the present.
        writer.push_audio(&[0.5, -0.5]);
        assert_eq!(reader.read_audio(&mut out), AudioRead::default());
        assert!(out.is_empty());
        writer.push_audio(&[0.25, -0.25, 0.125, -0.125]);
        assert_eq!(
            reader.read_audio(&mut out),
            AudioRead { frames: 2, lost: 0 }
        );
        assert_eq!(out, [0.25, -0.25, 0.125, -0.125]);
        assert_eq!(reader.read_audio(&mut out).frames, 0);
    }

    #[test]
    fn sound_survives_the_wrap_of_the_ring() {
        let mem = Vec8::new(layout().total_bytes());
        let mut reader = RingReader::create(&mem, layout()).unwrap();
        let mut writer = RingWriter::open(&mem, 4, 2, 0).unwrap();
        let mut out = Vec::new();
        reader.read_audio(&mut out);
        let ring = AUDIO_FRAMES as usize;
        // Fill up to 10 frames short of the end, read, then write across the end.
        let first: Vec<f32> = (0..(ring - 10) * 2).map(|i| i as f32).collect();
        writer.push_audio(&first);
        assert_eq!(reader.read_audio(&mut out).frames, ring - 10);
        out.clear();
        let across: Vec<f32> = (0..40).map(|i| 1000.0 + i as f32).collect();
        writer.push_audio(&across);
        assert_eq!(reader.read_audio(&mut out).frames, 20);
        assert_eq!(out, across);
    }

    #[test]
    fn sound_the_engine_overwrote_is_counted_as_lost() {
        let mem = Vec8::new(layout().total_bytes());
        let mut reader = RingReader::create(&mem, layout()).unwrap();
        let mut writer = RingWriter::open(&mem, 4, 2, 0).unwrap();
        let mut out = Vec::new();
        reader.read_audio(&mut out);
        let ring = AUDIO_FRAMES as usize;
        let total = ring + 100;
        let sound: Vec<f32> = (0..total * 2).map(|i| i as f32).collect();
        writer.push_audio(&sound);
        let got = reader.read_audio(&mut out);
        assert_eq!(
            got,
            AudioRead {
                frames: ring,
                lost: 100
            }
        );
        assert_eq!(out.len(), ring * 2);
        assert_eq!(out[0], (100 * 2) as f32);
        assert_eq!(*out.last().unwrap(), (total * 2 - 1) as f32);
    }
}
