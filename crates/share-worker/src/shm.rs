//! Shared memory by name: the block the frame ring lives in (`spec/share-play.md`, Frame ring).
//!
//! The worker creates it ([`Shm::create`]) and the game's engine maps it by the same name; a
//! test opens it the way the engine does ([`Shm::open`]). On Windows it is a named file mapping
//! in the session's local namespace, on Linux and the other Unixes a `shm_open` object. Only
//! the user who runs the game can read it: Windows gives a new mapping the creator's access
//! rules, and the Unix object is made with mode 0600.
//!
//! This file holds all of the `unsafe` that the ring needs. The ring's logic is in
//! `anyplay_share::ring`, over the [`Memory`] that [`Shm`] implements.

use std::sync::atomic::{AtomicU64, Ordering};

use anyplay_share::ring::{Memory, name_is_valid};

/// A mapped block of shared memory.
pub struct Shm {
    ptr: *mut u8,
    len: usize,
    os: os::Handle,
}

// The mapping is plain memory that another process writes at the same time; the `u64` fields
// are read and written atomically and the rest is copied, so a `Shm` may move between threads
// and be shared by them.
unsafe impl Send for Shm {}
unsafe impl Sync for Shm {}

impl Shm {
    /// Makes a new block of `len` bytes, zeroed, with a name that only `name_is_valid` allows.
    /// Fails when the name is in use: a ring is never shared between two shares.
    pub fn create(name: &str, len: usize) -> Result<Shm, String> {
        check(name, len)?;
        let (ptr, os) = os::create(name, len)?;
        Ok(Shm { ptr, len, os })
    }

    /// Maps a block that exists, as the engine does. `len` is the size the mapping is expected
    /// to have.
    pub fn open(name: &str, len: usize) -> Result<Shm, String> {
        check(name, len)?;
        let (ptr, os) = os::open(name, len)?;
        Ok(Shm { ptr, len, os })
    }

    fn atomic(&self, offset: usize) -> &AtomicU64 {
        assert!(
            offset.is_multiple_of(8) && offset + 8 <= self.len,
            "an atomic field at {offset} of {}",
            self.len
        );
        // SAFETY: the pointer is the start of a mapping of `len` bytes that lives as long as
        // `self`, the offset is in it, and it is a multiple of 8 on a page-aligned mapping.
        unsafe { &*self.ptr.add(offset).cast::<AtomicU64>() }
    }
}

fn check(name: &str, len: usize) -> Result<(), String> {
    if !name_is_valid(name) {
        return Err(format!("{name:?} is not a name for a frame ring"));
    }
    if len == 0 || len > u32::MAX as usize {
        return Err(format!("a frame ring of {len} bytes"));
    }
    Ok(())
}

impl Drop for Shm {
    fn drop(&mut self) {
        os::release(self.ptr, self.len, &mut self.os);
    }
}

impl Memory for Shm {
    fn len(&self) -> usize {
        self.len
    }

    fn load_u64(&self, offset: usize) -> u64 {
        self.atomic(offset).load(Ordering::Acquire)
    }

    fn store_u64(&self, offset: usize, value: u64) {
        self.atomic(offset).store(value, Ordering::Release);
    }

    fn read(&self, offset: usize, out: &mut [u8]) {
        assert!(offset + out.len() <= self.len, "a read past the ring");
        // SAFETY: in bounds (checked) and the two ranges cannot overlap: `out` is memory of
        // this process that is not the mapping.
        unsafe { std::ptr::copy_nonoverlapping(self.ptr.add(offset), out.as_mut_ptr(), out.len()) }
    }

    fn write(&self, offset: usize, data: &[u8]) {
        assert!(offset + data.len() <= self.len, "a write past the ring");
        // SAFETY: as in `read`.
        unsafe { std::ptr::copy_nonoverlapping(data.as_ptr(), self.ptr.add(offset), data.len()) }
    }
}

#[cfg(windows)]
mod os {
    use windows::Win32::Foundation::{
        CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE, INVALID_HANDLE_VALUE,
        SetLastError, WIN32_ERROR,
    };
    use windows::Win32::System::Memory::{
        CreateFileMappingW, FILE_MAP_ALL_ACCESS, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile,
        OpenFileMappingW, PAGE_READWRITE, UnmapViewOfFile,
    };
    use windows::core::HSTRING;

    pub struct Handle(HANDLE);

    fn map(handle: HANDLE, len: usize) -> Result<*mut u8, String> {
        // SAFETY: a valid mapping handle; a view of `len` bytes from the start.
        let view = unsafe { MapViewOfFile(handle, FILE_MAP_ALL_ACCESS, 0, 0, len) };
        if view.Value.is_null() {
            Err(format!(
                "the ring cannot be mapped: {}",
                std::io::Error::last_os_error()
            ))
        } else {
            Ok(view.Value.cast())
        }
    }

    pub fn create(name: &str, len: usize) -> Result<(*mut u8, Handle), String> {
        let wide = HSTRING::from(name);
        // SAFETY: plain calls with a valid, owned name; the handle is closed in `release` or
        // below on a failure.
        unsafe { SetLastError(WIN32_ERROR(0)) };
        let handle = unsafe {
            CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                None,
                PAGE_READWRITE,
                (len as u64 >> 32) as u32,
                len as u32,
                &wide,
            )
        }
        .map_err(|e| format!("the ring cannot be created: {e}"))?;
        // A mapping of that name that existed already is opened by CreateFileMappingW and the
        // error code says so; a ring is never shared, so that is a refusal.
        if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
            unsafe {
                let _ = CloseHandle(handle);
            }
            return Err("a ring of this name exists already".into());
        }
        match map(handle, len) {
            Ok(ptr) => Ok((ptr, Handle(handle))),
            Err(why) => {
                unsafe {
                    let _ = CloseHandle(handle);
                }
                Err(why)
            }
        }
    }

    pub fn open(name: &str, len: usize) -> Result<(*mut u8, Handle), String> {
        let wide = HSTRING::from(name);
        // SAFETY: as in `create`.
        let handle = unsafe { OpenFileMappingW(FILE_MAP_ALL_ACCESS.0, false, &wide) }
            .map_err(|e| format!("the ring cannot be opened: {e}"))?;
        match map(handle, len) {
            Ok(ptr) => Ok((ptr, Handle(handle))),
            Err(why) => {
                unsafe {
                    let _ = CloseHandle(handle);
                }
                Err(why)
            }
        }
    }

    pub fn release(ptr: *mut u8, _len: usize, handle: &mut Handle) {
        // SAFETY: the view and the handle are the ones made above and are released once.
        unsafe {
            let _ = UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS { Value: ptr.cast() });
            let _ = CloseHandle(handle.0);
        }
    }
}

#[cfg(unix)]
mod os {
    use std::ffi::CString;

    /// The name to remove again, for the side that made the block.
    pub struct Handle(Option<CString>);

    fn cname(name: &str) -> Result<CString, String> {
        CString::new(format!("/{name}")).map_err(|_| "a name with a zero byte".to_owned())
    }

    fn map(fd: libc::c_int, len: usize) -> Result<*mut u8, String> {
        // SAFETY: `fd` is a descriptor of an object at least `len` bytes long.
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            Err(format!(
                "the ring cannot be mapped: {}",
                std::io::Error::last_os_error()
            ))
        } else {
            Ok(ptr.cast())
        }
    }

    pub fn create(name: &str, len: usize) -> Result<(*mut u8, Handle), String> {
        let cname = cname(name)?;
        // SAFETY: plain libc calls on an owned, NUL-terminated name and a descriptor that is
        // closed on every path.
        unsafe {
            let fd = libc::shm_open(
                cname.as_ptr(),
                libc::O_CREAT | libc::O_EXCL | libc::O_RDWR,
                0o600,
            );
            if fd < 0 {
                return Err(format!(
                    "the ring cannot be created: {}",
                    std::io::Error::last_os_error()
                ));
            }
            if libc::ftruncate(fd, len as libc::off_t) != 0 {
                let why = std::io::Error::last_os_error();
                libc::close(fd);
                libc::shm_unlink(cname.as_ptr());
                return Err(format!("the ring cannot be sized: {why}"));
            }
            let mapped = map(fd, len);
            libc::close(fd);
            match mapped {
                Ok(ptr) => Ok((ptr, Handle(Some(cname)))),
                Err(why) => {
                    libc::shm_unlink(cname.as_ptr());
                    Err(why)
                }
            }
        }
    }

    pub fn open(name: &str, len: usize) -> Result<(*mut u8, Handle), String> {
        let cname = cname(name)?;
        // SAFETY: as in `create`.
        unsafe {
            let fd = libc::shm_open(cname.as_ptr(), libc::O_RDWR, 0);
            if fd < 0 {
                return Err(format!(
                    "the ring cannot be opened: {}",
                    std::io::Error::last_os_error()
                ));
            }
            let mut stat: libc::stat = std::mem::zeroed();
            if libc::fstat(fd, &mut stat) != 0 || (stat.st_size as u64) < len as u64 {
                libc::close(fd);
                return Err("the ring is shorter than expected".into());
            }
            let mapped = map(fd, len);
            libc::close(fd);
            mapped.map(|ptr| (ptr, Handle(None)))
        }
    }

    pub fn release(ptr: *mut u8, len: usize, handle: &mut Handle) {
        // SAFETY: the mapping is the one made above and is released once.
        unsafe {
            libc::munmap(ptr.cast(), len);
            if let Some(name) = handle.0.take() {
                libc::shm_unlink(name.as_ptr());
            }
        }
    }
}

#[cfg(not(any(windows, unix)))]
mod os {
    pub struct Handle;

    pub fn create(_name: &str, _len: usize) -> Result<(*mut u8, Handle), String> {
        Err("this system has no shared memory for the frame ring".into())
    }

    pub fn open(_name: &str, _len: usize) -> Result<(*mut u8, Handle), String> {
        Err("this system has no shared memory for the frame ring".into())
    }

    pub fn release(_ptr: *mut u8, _len: usize, _handle: &mut Handle) {}
}

#[cfg(test)]
mod tests {
    use anyplay_share::ring::{Layout, RingReader, RingWriter, name_from_random};

    use super::*;

    fn unique() -> String {
    // Tests run at the same time and a clock may tick coarsely: a counter keeps names apart.
    static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let mut random = [0u8; 16];
        random[12..].copy_from_slice(
            &NEXT
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                .to_le_bytes(),
        );
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        random[..8].copy_from_slice(&(now as u64).to_le_bytes());
        random[8..12].copy_from_slice(&std::process::id().to_le_bytes());
        name_from_random(random)
    }

    #[test]
    fn a_name_that_is_not_a_ring_name_makes_nothing() {
        assert!(Shm::create("../etc/passwd", 4096).is_err());
        assert!(Shm::create("Local\\anyplay-x", 4096).is_err());
        assert!(Shm::create("anyplay-ok-name", 0).is_err());
        assert!(Shm::open("nothing", 4096).is_err());
    }

    #[test]
    fn a_ring_made_by_the_worker_is_filled_by_a_second_mapping() {
        let layout = Layout::new(8, 4, 60).unwrap();
        let name = unique();
        let worker = Shm::create(&name, layout.total_bytes()).unwrap();
        let mut reader = RingReader::create(worker, layout).unwrap();
        // The engine's side: another mapping of the same block, with its own pointer.
        let engine = Shm::open(&name, layout.total_bytes()).unwrap();
        let mut writer = RingWriter::open(engine, 8, 4, layout.total_bytes()).unwrap();

        let picture: Vec<u8> = (0..8 * 4 * 4).map(|i| i as u8).collect();
        assert!(writer.push_picture(&picture, 777, 5));
        let mut pixels = Vec::new();
        let got = reader.next_picture(&mut pixels).expect("a picture");
        assert_eq!((got.present_us, got.frame, got.seq), (777, 5, 1));
        assert_eq!(pixels, picture);

        writer.push_audio(&[0.5, -0.5]);
        let mut sound = Vec::new();
        reader.read_audio(&mut sound);
        writer.push_audio(&[0.25, -0.25]);
        assert_eq!(reader.read_audio(&mut sound).frames, 1);
        assert_eq!(sound, [0.25, -0.25]);
    }

    #[test]
    fn a_name_in_use_is_not_made_again_and_is_free_after_the_block_goes() {
        let name = unique();
        let first = Shm::create(&name, 4096).unwrap();
        assert!(Shm::create(&name, 4096).is_err());
        drop(first);
        #[cfg(unix)]
        assert!(Shm::open(&name, 4096).is_err(), "removed with its owner");
    }

    #[test]
    fn the_ring_holds_the_atomic_fields_across_threads() {
        let name = unique();
        let shm = std::sync::Arc::new(Shm::create(&name, 4096).unwrap());
        let other = shm.clone();
        let writer = std::thread::spawn(move || {
            for i in 1..=1000u64 {
                other.store_u64(64, i);
            }
        });
        let mut last = 0;
        while last < 1000 {
            let seen = shm.load_u64(64);
            assert!(seen >= last);
            last = seen;
        }
        writer.join().unwrap();
    }
}
