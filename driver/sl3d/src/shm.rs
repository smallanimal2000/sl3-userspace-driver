//! Shared-memory bridge between the sl3d daemon and the CoreAudio plugin.
//!
//! The in-memory layout is byte-identical to the C `struct sl3_shm` in
//! `driver/sl3d/include/sl3_shm.h`, so the (still-C) AudioServerPlugin interoperates with this
//! Rust daemon. Two single-producer/single-consumer float32 rings.

use std::ffi::CString;
use std::mem::size_of;
use std::sync::atomic::{AtomicU32, AtomicU64};

pub const NAME: &str = "/sl3_audio";
pub const MAGIC: u32 = 0x334C_5341; // 'ASL3'
pub const VERSION: u32 = 2;
pub const CHANNELS: usize = 6;
pub const RING_FRAMES: u64 = 32768;
pub const RING_MASK: u64 = RING_FRAMES - 1;
pub const RING_SAMPLES: usize = (RING_FRAMES as usize) * CHANNELS;

/// Must match `struct sl3_shm` (driver/sl3d/include/sl3_shm.h) field-for-field, including the
/// padding the C++ atomics imply. Verified by `assert_layout()`.
#[repr(C)]
pub struct Shm {
    pub magic: u32,
    pub version: u32,
    pub channels: u32,
    pub sample_rate: AtomicU32,
    pub device_present: AtomicU32,
    pub daemon_heartbeat: AtomicU64,
    pub cap_write: AtomicU64,
    pub cap_read: AtomicU64,
    pub play_write: AtomicU64,
    pub play_read: AtomicU64,
    pub cap: [f32; RING_SAMPLES],
    pub play: [f32; RING_SAMPLES],
    // Device-clock anchor: mach_absolute_time() sampled when cap_write was last
    // advanced (cap_write ticks on the hardware clock). The plugin uses the pair
    // (cap_write, clock_host) so GetZeroTimeStamp reports the REAL device rate,
    // not a fake host-locked 48000 — otherwise coreaudiod drifts vs the device.
    pub clock_host: AtomicU64,
    // Client IO activity, published by the plugin (1 = coreaudiod is running IO,
    // 0 = idle). The daemon suspends the USB iso stream while this is 0. See the
    // matching field in driver/sl3d/include/sl3_shm.h.
    pub io_running: AtomicU32,
}

/// Debug check that the Rust layout matches the C struct.
pub fn assert_layout() {
    // clock_host (u64) + io_running (u32) + 4 bytes trailing pad (8-byte align) = 16.
    assert_eq!(size_of::<Shm>(), 64 + 2 * RING_SAMPLES * 4 + 16);
}

#[inline]
pub fn i24_to_f32(s: i32) -> f32 { s as f32 / 8_388_608.0 }
#[inline]
pub fn f32_to_i24(f: f32) -> i32 {
    let v = f * 8_388_608.0;
    v.clamp(-8_388_608.0, 8_388_607.0) as i32
}

/// Create (or open) and mmap the shared memory. `create` sizes it on first use.
/// Returns a pointer valid for the process lifetime, or null on failure.
pub unsafe fn map(create: bool) -> *mut Shm {
    let name = CString::new(NAME).unwrap();
    // A POSIX shm object can only be sized once (ftruncate). If a stale segment
    // from a different struct layout persists, mapping the current struct over it
    // would fault — so recreate it. But ONLY when the size actually differs:
    // unconditionally unlinking would orphan a plugin already mapped to a valid
    // segment across a daemon restart (KeepAlive), silently disconnecting audio.
    if create {
        let fd0 = libc::shm_open(name.as_ptr(), libc::O_RDWR, 0o666);
        if fd0 >= 0 {
            let mut st: libc::stat = std::mem::zeroed();
            let wrong = libc::fstat(fd0, &mut st) != 0 || st.st_size as usize != size_of::<Shm>();
            libc::close(fd0);
            if wrong { libc::shm_unlink(name.as_ptr()); }
        }
    }
    let flags = if create { libc::O_CREAT | libc::O_RDWR } else { libc::O_RDWR };
    let fd = libc::shm_open(name.as_ptr(), flags, 0o666);
    if fd < 0 { return std::ptr::null_mut(); }

    // A POSIX shm object can only be ftruncate()d once (at creation) on macOS.
    if create {
        // shm_open's mode is masked by umask (typically -> 0644), which would deny
        // the CoreAudio plugin (a different user, _coreaudiod) its O_RDWR open.
        // Force 0666 so the plugin can map the ring.
        libc::fchmod(fd, 0o666);
        let mut st: libc::stat = std::mem::zeroed();
        if libc::fstat(fd, &mut st) == 0 && (st.st_size as usize) < size_of::<Shm>() {
            if libc::ftruncate(fd, size_of::<Shm>() as libc::off_t) != 0 {
                libc::close(fd);
                return std::ptr::null_mut();
            }
        }
    }
    let p = libc::mmap(std::ptr::null_mut(), size_of::<Shm>(),
        libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED, fd, 0);
    libc::close(fd);
    if p == libc::MAP_FAILED { return std::ptr::null_mut(); }
    p as *mut Shm
}

/// Initialise the header if this is a fresh segment.
pub unsafe fn init_header(s: *mut Shm) {
    if (*s).magic != MAGIC || (*s).version != VERSION {
        std::ptr::write_bytes(s as *mut u8, 0, size_of::<Shm>());
        (*s).magic = MAGIC;
        (*s).version = VERSION;
        (*s).channels = CHANNELS as u32;
    }
}
