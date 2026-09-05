//! Drop-in reimplementation of Rane's `Sl3Api.framework`, backed by the Rust
//! `sl3` driver (direct USB) instead of the dead kext. Exports the exact C ABI
//! the shipped `SL 3 Audio Control Panel.prefPane` imports, so that binary runs
//! unmodified. Mirrors `framework/Sl3Api.cpp`.

use sl3::Sl3;
use std::os::raw::{c_int, c_void};
use std::sync::atomic::{AtomicI32, AtomicU32, Ordering};
use std::sync::Mutex;
use std::thread::JoinHandle;
use std::time::Duration;

// ---- C status structs (exact sizes) ----
#[repr(C)] pub struct PhonoData { pub bytes: [u8; 3] }
#[repr(C)] pub struct ThruData { pub bytes: [u8; 4] }
#[repr(C)] pub struct OverloadData { pub bytes: [u8; 6] }
#[repr(C)] pub struct UsbPortStatus { pub status: u32, pub reserved: u32 }
#[repr(C)] pub struct Version { pub a: u32, pub b: u32, pub c: u32, pub d: u32 }
#[repr(C)] pub struct FirmwareStatus { pub state: u32, pub percent: u32 }

type PhonoCb = extern "C" fn(*mut PhonoData);
type ThruCb = extern "C" fn(*mut ThruData);
type OverloadCb = extern "C" fn(*mut OverloadData);
type UsbPortCb = extern "C" fn(*mut UsbPortStatus);

#[derive(Default, Clone, Copy)]
struct Cbs {
    phono: Option<PhonoCb>,
    thru: Option<ThruCb>,
    overload: Option<OverloadCb>,
    usbport: Option<UsbPortCb>,
}

struct Shared {
    dev: Mutex<Sl3>,
    cb: Mutex<Cbs>,
    running: std::sync::atomic::AtomicBool,
    buffer_ms: AtomicU32,
}

struct HandleBox {
    shared: std::sync::Arc<Shared>,
    reader: Option<JoinHandle<()>>,
}

static LAST_CONN_ERR: AtomicI32 = AtomicI32::new(0);

fn is_event(code: u8) -> bool { matches!(code, 0x34 | 0x38 | 0x39) }

// Copy out the relevant callback under a short lock, then call it unlocked.
fn dispatch(shared: &Shared, r: &[u8; 64]) {
    let cbs = *shared.cb.lock().unwrap();
    match r[0] {
        0x38 => if let Some(f) = cbs.phono { let mut d = PhonoData { bytes: [r[5], r[6], r[7]] }; f(&mut d); }
        0x39 => if let Some(f) = cbs.thru { let mut d = ThruData { bytes: [r[5], r[6], r[7], r[8]] }; f(&mut d); }
        0x34 => if let Some(f) = cbs.overload { let mut d = OverloadData { bytes: [r[5], r[6], r[7], r[8], r[9], r[10]] }; f(&mut d); }
        _ => {}
    }
}

// One command: send OUT, read replies, dispatching interleaved events until our
// reply arrives. Serialised against the reader thread by the dev mutex.
fn command(shared: &Shared, code: u8, payload: &[u8]) -> Option<[u8; 64]> {
    let mut dev = shared.dev.lock().unwrap();
    pollster::block_on(async {
        dev.hid_send(code, payload).await.ok()?;
        for _ in 0..8 {
            let r = dev.read_event(Duration::from_millis(1000)).await.ok()?;
            if is_event(r[0]) { dispatch(shared, &r); continue; }
            return Some(r);
        }
        None
    })
}

unsafe fn handle<'a>(p: *mut c_void) -> Option<&'a HandleBox> {
    (p as *const HandleBox).as_ref()
}

// ------------------------------- exports -------------------------------

#[no_mangle]
pub extern "C" fn sl3_open() -> *mut c_void {
    let dev = match Sl3::open_control() {
        Ok(d) => d,
        Err(_) => { LAST_CONN_ERR.store(0xE00002D8u32 as i32, Ordering::Relaxed); return std::ptr::null_mut(); }
    };
    let shared = std::sync::Arc::new(Shared {
        dev: Mutex::new(dev),
        cb: Mutex::new(Cbs::default()),
        running: std::sync::atomic::AtomicBool::new(true),
        buffer_ms: AtomicU32::new(10),
    });
    // Reader thread: catch unsolicited phono/thru/overload reports while idle.
    let rshared = shared.clone();
    let reader = std::thread::spawn(move || {
        while rshared.running.load(Ordering::Relaxed) {
            let got = {
                let dev = rshared.dev.lock().unwrap();
                pollster::block_on(dev.read_event(Duration::from_millis(20)))
            };
            match got {
                Ok(r) if is_event(r[0]) => dispatch(&rshared, &r),
                _ => std::thread::sleep(Duration::from_millis(3)),
            }
        }
    });
    LAST_CONN_ERR.store(0, Ordering::Relaxed);
    Box::into_raw(Box::new(HandleBox { shared, reader: Some(reader) })) as *mut c_void
}

#[no_mangle]
pub extern "C" fn sl3_close(p: *mut c_void) -> c_int {
    if p.is_null() { return 8; }
    let mut hb = unsafe { Box::from_raw(p as *mut HandleBox) };
    hb.shared.running.store(false, Ordering::Relaxed);
    if let Some(j) = hb.reader.take() { let _ = j.join(); }
    0 // Sl3 dropped here -> interfaces released
}

#[no_mangle]
pub extern "C" fn sl3_get_audio_controls(p: *mut c_void, offset: c_int, dst: *mut c_void, count: c_int) -> c_int {
    let hb = match unsafe { handle(p) } { Some(h) => h, None => return 8 };
    if offset < 0 || count <= 0 || (offset + count) as usize > sl3::AUDIO_CTRL_BYTES { return 2; }
    match command(&hb.shared, 0x32, &[]) {
        Some(r) => { unsafe {
            std::ptr::copy_nonoverlapping(r.as_ptr().add(5 + offset as usize), dst as *mut u8, count as usize);
        } 0 }
        None => 4,
    }
}

#[no_mangle]
pub extern "C" fn sl3_set_audio_controls(p: *mut c_void, offset: c_int, data: *const c_void, count: c_int) -> c_int {
    let hb = match unsafe { handle(p) } { Some(h) => h, None => return 8 };
    if offset < 0 || count <= 0 || (offset + count) as usize > sl3::AUDIO_CTRL_BYTES { return 2; }
    let mut pl = Vec::with_capacity(2 + count as usize);
    pl.push(offset as u8);
    pl.push(count as u8);
    unsafe { pl.extend_from_slice(std::slice::from_raw_parts(data as *const u8, count as usize)); }
    if command(&hb.shared, 0x33, &pl).is_some() { 0 } else { 4 }
}

#[no_mangle]
pub extern "C" fn sl3_get_parameter(p: *mut c_void, param_id: c_int, out: *mut u32) -> c_int {
    let hb = match unsafe { handle(p) } { Some(h) => h, None => return 8 };
    if param_id != 0 { return 2; }
    match command(&hb.shared, 0x30, &[]) {
        Some(r) => { unsafe { *out = ((r[5] as u32) << 8) | r[6] as u32; } 0 }
        None => 4,
    }
}

#[no_mangle]
pub extern "C" fn sl3_set_parameter(p: *mut c_void, param_id: c_int, value: u32) -> c_int {
    let hb = match unsafe { handle(p) } { Some(h) => h, None => return 8 };
    if param_id != 0 { return 2; }
    if value != 44100 && value != 48000 { return 2; }
    let be = [((value >> 8) & 0xff) as u8, (value & 0xff) as u8];
    if command(&hb.shared, 0x31, &be).is_some() { 0 } else { 4 }
}

#[no_mangle]
pub extern "C" fn sl3_get_buffer_millisecs(p: *mut c_void, out_ms: *mut u32) -> c_int {
    let hb = match unsafe { handle(p) } { Some(h) => h, None => return 8 };
    unsafe { *out_ms = hb.shared.buffer_ms.load(Ordering::Relaxed); }
    0
}

#[no_mangle]
pub extern "C" fn sl3_set_buffer_millisecs(p: *mut c_void, ms: c_int) -> c_int {
    let hb = match unsafe { handle(p) } { Some(h) => h, None => return 8 };
    if ms < 1 || ms > 50 { return 2; }
    hb.shared.buffer_ms.store(ms as u32, Ordering::Relaxed);
    0
}

#[no_mangle]
pub extern "C" fn sl3_is_C0_device(p: *mut c_void, out: *mut c_int) -> c_int {
    if p.is_null() { return 8; }
    if !out.is_null() { unsafe { *out = 0; } } // shipping hardware is never in C0/boot mode
    0
}

#[no_mangle]
pub extern "C" fn sl3_get_vendor_id(p: *mut c_void, out: *mut u32) -> c_int {
    if p.is_null() { return 8; } if !out.is_null() { unsafe { *out = sl3::VID as u32; } } 0
}
#[no_mangle]
pub extern "C" fn sl3_get_product_id(p: *mut c_void, out: *mut u32) -> c_int {
    if p.is_null() { return 8; } if !out.is_null() { unsafe { *out = sl3::PID as u32; } } 0
}

#[no_mangle]
pub extern "C" fn sl3_get_status(p: *mut c_void, out: *mut u8) -> c_int {
    let hb = match unsafe { handle(p) } { Some(h) => h, None => return 8 };
    match command(&hb.shared, 0x0a, &[]) { Some(r) => { if !out.is_null() { unsafe { *out = r[5]; } } 0 } None => 4 }
}

#[no_mangle]
pub extern "C" fn sl3_get_overload_status(p: *mut c_void, out: *mut OverloadData) -> c_int {
    let hb = match unsafe { handle(p) } { Some(h) => h, None => return 8 };
    match command(&hb.shared, 0x35, &[]) {
        Some(r) => { if !out.is_null() { unsafe { (*out).bytes.copy_from_slice(&r[5..11]); } } 0 }
        None => 4,
    }
}

#[no_mangle]
pub extern "C" fn sl3_get_usb_port_status(p: *mut c_void, out: *mut UsbPortStatus) -> c_int {
    if p.is_null() { return 8; }
    if !out.is_null() { unsafe { (*out).status = 0; (*out).reserved = 0; } }
    0
}

#[no_mangle]
pub extern "C" fn sl3_get_driver_version(p: *mut c_void, out: *mut Version) -> c_int {
    if p.is_null() { return 8; }
    if !out.is_null() { unsafe { *out = Version { a: 2, b: 0, c: 1, d: 2 }; } }
    0
}
#[no_mangle]
pub extern "C" fn sl3_get_api_version(out: *mut Version) -> c_int {
    if !out.is_null() { unsafe { *out = Version { a: 2, b: 0, c: 1, d: 2 }; } } 0
}

#[no_mangle]
pub extern "C" fn sl3_set_phonoswitch_callback(p: *mut c_void, cb: Option<PhonoCb>) -> c_int {
    let hb = match unsafe { handle(p) } { Some(h) => h, None => return 8 };
    hb.shared.cb.lock().unwrap().phono = cb; 0
}
#[no_mangle]
pub extern "C" fn sl3_set_thrustate_callback(p: *mut c_void, cb: Option<ThruCb>) -> c_int {
    let hb = match unsafe { handle(p) } { Some(h) => h, None => return 8 };
    hb.shared.cb.lock().unwrap().thru = cb; 0
}
#[no_mangle]
pub extern "C" fn sl3_set_overload_callback(p: *mut c_void, cb: Option<OverloadCb>) -> c_int {
    let hb = match unsafe { handle(p) } { Some(h) => h, None => return 8 };
    hb.shared.cb.lock().unwrap().overload = cb; 0
}
#[no_mangle]
pub extern "C" fn sl3_set_usb_port_status_callback(p: *mut c_void, cb: Option<UsbPortCb>) -> c_int {
    let hb = match unsafe { handle(p) } { Some(h) => h, None => return 8 };
    hb.shared.cb.lock().unwrap().usbport = cb; 0
}

// ---- firmware: stubbed (report versions, never flash) ----
#[no_mangle]
pub extern "C" fn sl3_get_firmware_version(p: *mut c_void, out: *mut Version) -> c_int {
    if p.is_null() { return 8; }
    if !out.is_null() { unsafe { *out = Version { a: 2, b: 50, c: 0, d: 0 }; } } 0
}
#[no_mangle]
pub extern "C" fn sl3_get_firmware_embedded_version(out: *mut Version) -> c_int {
    if !out.is_null() { unsafe { *out = Version { a: 2, b: 50, c: 0, d: 0 }; } } 0
}
#[no_mangle]
pub extern "C" fn sl3_set_update_firmware_from_embedded(p: *mut c_void, _cb: *const c_void, _flags: u32) -> c_int {
    if p.is_null() { 8 } else { 0 } // no-op
}

// C++-linkage symbol (mangled name) the prefPane imports.
#[export_name = "_Z33sl3_private_last_connection_errorv"]
pub extern "C" fn sl3_private_last_connection_error() -> c_int {
    LAST_CONN_ERR.load(Ordering::Relaxed)
}
