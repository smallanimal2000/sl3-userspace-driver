//! Dedicated, stable full-duplex isochronous streaming for the native daemon.
//!
//! Reuses a *fixed pool* of libusb transfers and resubmits each inside its own
//! completion callback on one event-loop thread (the proven zero-gap model),
//! with no per-cycle transfer alloc/free.
//!
//! The SL 3's OUT endpoint is asynchronous with **implicit feedback**: the IN
//! (capture) endpoint's delivered sample counts ARE the OUT clock. So we mirror
//! IN's per-microframe frame counts onto OUT exactly — the device's own clock
//! drives playback pacing, which keeps it glitch-free (an averaged rate drifts
//! and the device's buffer periodically over/underflows).

use crate::{Error, Result, BYTES_PER_SAMP, CHANNELS, EP_CAPTURE, EP_HID_IN, EP_HID_OUT,
            EP_PLAYBACK, FRAME_BYTES, HID_REPORT_LEN, IF_CAPTURE, IF_HID, IF_PLAYBACK, PID, VID};
use libusb1_sys as ffi;
use libusb1_sys::constants::*;
use rusb::{Context, DeviceHandle, UsbContext};
use std::collections::VecDeque;
use std::os::raw::c_void;
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};

const NUM: usize = 8; // transfers per direction, in flight
const PKTS: usize = 16; // iso packets per transfer
const PKT: usize = 126; // wMaxPacketSize (7 frames)
const MAXNF: usize = PKT / FRAME_BYTES; // 7 frames/packet
const BUFLEN: usize = PKTS * PKT;
const FB_MAX: usize = 4 * NUM; // cap feedback backlog (bounds latency if OUT lags)

/// Promote the calling thread to a real-time (time-constraint) scheduling class.
/// REQUIRED: without it the libusb event-loop thread is preempted under load, OUT
/// transfers are serviced late, the device drains the play ring slower than
/// coreaudiod fills it, and the backlog grows until MAXLAT drops it — producing
/// glitches that worsen over time (verified: removing this reintroduces them).
fn set_realtime() {
    #[repr(C)]
    struct TimeConstraint { period: u32, computation: u32, constraint: u32, preemptible: u32 }
    #[repr(C)]
    struct MachTimebase { numer: u32, denom: u32 }
    extern "C" {
        fn mach_thread_self() -> u32;
        fn thread_policy_set(thread: u32, flavor: i32, policy_info: *const u32, count: u32) -> i32;
        fn mach_timebase_info(info: *mut MachTimebase) -> i32;
        fn mach_port_deallocate(task: u32, name: u32) -> i32;
        fn mach_task_self() -> u32;
    }
    const THREAD_TIME_CONSTRAINT_POLICY: i32 = 2;
    unsafe {
        let mut tb = MachTimebase { numer: 0, denom: 0 };
        mach_timebase_info(&mut tb);
        let abs = |ns: f64| -> u32 { (ns * tb.denom as f64 / tb.numer as f64) as u32 };
        let tc = TimeConstraint {
            period: abs(2_666_666.0),   // ~128 frames @ 48k
            computation: abs(2_000_000.0),
            constraint: abs(2_666_666.0),
            preemptible: 1,
        };
        let t = mach_thread_self();
        thread_policy_set(t, THREAD_TIME_CONSTRAINT_POLICY, &tc as *const _ as *const u32, 4);
        mach_port_deallocate(mach_task_self(), t);
    }
}

#[inline]
fn s24le(p: *const u8) -> i32 {
    unsafe {
        let mut v = (*p as u32) | ((*p.add(1) as u32) << 8) | ((*p.add(2) as u32) << 16);
        if v & 0x0080_0000 != 0 { v |= 0xFF00_0000; }
        v as i32
    }
}
#[inline]
fn put24(p: *mut u8, s: i32) {
    unsafe { *p = (s & 0xff) as u8; *p.add(1) = ((s >> 8) & 0xff) as u8; *p.add(2) = ((s >> 16) & 0xff) as u8; }
}

struct St<C, P> {
    running: bool,
    inflight: i32,
    on_cap: C,
    fill: P,
    rate: u32, // nominal (fallback only, when the feedback backlog is empty)
    phase: u32,
    fb: VecDeque<[u8; PKTS]>, // IN per-packet frame counts = the OUT clock (implicit feedback)
    cap_planar: Vec<Vec<i32>>,
    play_planar: Vec<Vec<i32>>,
}

/// Resubmit a transfer, retrying a transient failure a few times.
unsafe fn resubmit(xfer: *mut ffi::libusb_transfer) -> bool {
    for _ in 0..8 {
        if ffi::libusb_submit_transfer(xfer) == 0 { return true; }
    }
    false
}

extern "system" fn cap_cb<C, P>(xfer: *mut ffi::libusb_transfer)
where
    C: FnMut(&[Vec<i32>], usize),
    P: FnMut(&mut [Vec<i32>], usize),
{
    unsafe {
        let st = &mut *((*xfer).user_data as *mut St<C, P>);
        if !st.running || (*xfer).status == LIBUSB_TRANSFER_CANCELLED { st.inflight -= 1; return; }
        let np = (*xfer).num_iso_packets as isize;
        let descs = (*xfer).iso_packet_desc.as_ptr();
        let base = (*xfer).buffer;

        // Per-packet frame counts (the feedback we mirror onto OUT).
        let mut sizes = [0u8; PKTS];
        let mut total = 0usize;
        for p in 0..np {
            let d = &*descs.offset(p);
            if d.status == LIBUSB_TRANSFER_COMPLETED {
                let nf = (d.actual_length as usize / FRAME_BYTES).min(MAXNF);
                if (p as usize) < PKTS { sizes[p as usize] = nf as u8; }
                total += nf;
            }
        }
        if total > 0 {
            for c in 0..CHANNELS { if st.cap_planar[c].len() < total { st.cap_planar[c].resize(total, 0); } }
            let mut f = 0usize;
            for p in 0..np {
                let d = &*descs.offset(p);
                if d.status != LIBUSB_TRANSFER_COMPLETED { continue; }
                let pkt = base.add(p as usize * PKT);
                let nf = d.actual_length as usize / FRAME_BYTES;
                for i in 0..nf {
                    let fr = pkt.add(i * FRAME_BYTES);
                    for c in 0..CHANNELS { st.cap_planar[c][f] = s24le(fr.add(c * BYTES_PER_SAMP)); }
                    f += 1;
                }
            }
            let planar: *const Vec<Vec<i32>> = &st.cap_planar;
            (st.on_cap)(&*planar, total);
            if st.fb.len() < FB_MAX { st.fb.push_back(sizes); } // enqueue feedback for OUT
        }
        if st.running && resubmit(xfer) { return; }
        st.inflight -= 1;
    }
}

/// Fill one OUT transfer, mirroring the oldest IN feedback if available, else the
/// nominal rate. Always non-empty (macOS rejects a zero-length iso transfer).
unsafe fn build_out<C, P>(st: &mut St<C, P>, xfer: *mut ffi::libusb_transfer)
where
    P: FnMut(&mut [Vec<i32>], usize),
{
    let base = (*xfer).buffer;
    let descs = (*xfer).iso_packet_desc.as_mut_ptr();

    let mut sizes = [0u8; PKTS];
    let mut total = 0usize;
    if let Some(fb) = st.fb.pop_front() {
        for p in 0..PKTS { sizes[p] = fb[p]; total += fb[p] as usize; }
    }
    if total == 0 {
        // Startup prime / feedback underrun: nominal (6 @ 48k, 5/6 @ 44.1k).
        for p in 0..PKTS {
            st.phase += st.rate;
            let nf = ((st.phase / 8000) as usize).clamp(1, MAXNF);
            st.phase %= 8000;
            sizes[p] = nf as u8;
        }
    }

    let mut off = 0usize;
    for p in 0..PKTS {
        let nf = sizes[p] as usize;
        if nf > 0 {
            for c in 0..CHANNELS { if st.play_planar[c].len() < nf { st.play_planar[c].resize(nf, 0); } }
            let planar: *mut Vec<Vec<i32>> = &mut st.play_planar;
            (st.fill)(&mut *planar, nf);
            for i in 0..nf {
                for c in 0..CHANNELS { put24(base.add(off), st.play_planar[c][i]); off += BYTES_PER_SAMP; }
            }
        }
        (*descs.add(p)).length = (nf * FRAME_BYTES) as u32;
    }
    (*xfer).length = off as i32;
}

extern "system" fn play_cb<C, P>(xfer: *mut ffi::libusb_transfer)
where
    C: FnMut(&[Vec<i32>], usize),
    P: FnMut(&mut [Vec<i32>], usize),
{
    unsafe {
        let st = &mut *((*xfer).user_data as *mut St<C, P>);
        if !st.running || (*xfer).status == LIBUSB_TRANSFER_CANCELLED { st.inflight -= 1; return; }
        build_out(st, xfer);
        if st.running && resubmit(xfer) { return; }
        st.inflight -= 1;
    }
}

/// One SL 3 opened for audio only (interfaces 1 & 2 on a single libusb context).
pub struct AudioDevice {
    ctx: Context,
    handle: DeviceHandle<Context>,
}

impl AudioDevice {
    pub fn open() -> Result<Self> {
        let ctx = Context::new().map_err(Error::Io)?;
        let handle = ctx.open_device_with_vid_pid(VID, PID).ok_or(Error::NoDevice)?;
        if handle.active_configuration().map_err(Error::Io)? != 1 {
            let _ = handle.set_active_configuration(1);
        }
        let _ = handle.set_auto_detach_kernel_driver(true);
        handle.claim_interface(IF_PLAYBACK).map_err(Error::Access)?;
        handle.claim_interface(IF_CAPTURE).map_err(Error::Access)?;
        Ok(AudioDevice { ctx, handle })
    }

    /// Configure the device over the HID control interface (IF3) before streaming:
    /// (1) set the hardware sample clock — the SL 3 powers up at 44.1 kHz and would
    /// otherwise free-run there while CoreAudio feeds 48 kHz (ring overflow/glitching);
    /// (2) enable output routing (audio-controls offset 8) — it defaults to OFF on a
    /// freshly powered device, so playback reaches the device but never leaves its
    /// outputs (silence). Claims IF3 for both, then releases so control tools work.
    fn set_device_config(&self, hz: u32) -> Result<()> {
        self.handle.claim_interface(IF_HID).map_err(Error::Access)?;
        let to = std::time::Duration::from_millis(250);
        let cmd = |code: u8, seq: u32, payload: &[u8]| -> Result<()> {
            let report = crate::proto::build_report(code, seq, payload)?;
            self.handle.write_interrupt(EP_HID_OUT, &report, to).map_err(Error::Io)?;
            // Drain the one reply (required, or the next control OUT stalls).
            let mut buf = [0u8; HID_REPORT_LEN];
            let _ = self.handle.read_interrupt(EP_HID_IN, &mut buf, to);
            Ok(())
        };
        let res = (|| -> Result<()> {
            cmd(crate::proto::CMD_SET_RATE, 1, &crate::proto::encode_rate(hz)?)?;
            cmd(crate::proto::CMD_SET_CONTROLS, 2, &crate::proto::encode_set_controls(8, &[1])?)?;
            Ok(())
        })();
        let _ = self.handle.release_interface(IF_HID);
        res
    }

    /// Liveness probe: is the device this handle was opened on still enumerated?
    ///
    /// Purely a device-list walk — NO control transfer or other bus I/O, which on
    /// this device can spuriously fail on a perfectly healthy handle (a false
    /// "gone" would churn the daemon's reopen loop and block activation). A USB
    /// address is stable for the lifetime of an attachment and freed on detach, so
    /// matching our (bus, address) against a fresh enumeration cleanly detects an
    /// unplug; a reattach lands on a new address, so it reads as gone too and the
    /// daemon rebinds to the new one. Descriptors are read from libusb's cache.
    pub fn is_present(&self) -> bool {
        let dev = self.handle.device();
        let (bus, addr) = (dev.bus_number(), dev.address());
        match self.ctx.devices() {
            Ok(list) => list.iter().any(|d| {
                d.bus_number() == bus
                    && d.address() == addr
                    && d.device_descriptor()
                        .map(|dd| dd.vendor_id() == VID && dd.product_id() == PID)
                        .unwrap_or(false)
            }),
            Err(_) => true, // can't enumerate -> assume still present rather than churn
        }
    }

    /// Run full-duplex audio until `stop` is set (or the pool empties on error).
    /// Blocks the calling thread; capture/playback callbacks run on it.
    pub fn run<C, P>(&self, stop: &AtomicBool, rate: u32, on_cap: C, fill: P) -> Result<()>
    where
        C: FnMut(&[Vec<i32>], usize),
        P: FnMut(&mut [Vec<i32>], usize),
    {
        // Lock the device clock to the requested rate BEFORE the iso endpoints go
        // live, so producer (CoreAudio) and consumer (device) run at one rate.
        if let Err(e) = self.set_device_config(rate) {
            eprintln!("native_audio: device config (rate/output) failed: {e}");
        }
        self.handle.set_alternate_setting(IF_CAPTURE, 1).map_err(Error::Io)?;
        self.handle.set_alternate_setting(IF_PLAYBACK, 1).map_err(Error::Io)?;
        set_realtime();

        let stp = Box::into_raw(Box::new(St {
            running: true, inflight: 0, on_cap, fill,
            rate, phase: 0, fb: VecDeque::with_capacity(FB_MAX + 1),
            cap_planar: (0..CHANNELS).map(|_| Vec::new()).collect(),
            play_planar: (0..CHANNELS).map(|_| Vec::new()).collect(),
        }));
        let handle = self.handle.as_raw();
        let ctx = self.ctx.as_raw();
        let mut xfers: Vec<*mut ffi::libusb_transfer> = Vec::new();
        let mut bufs: Vec<Vec<u8>> = Vec::new();
        let mut err: Option<Error> = None;

        unsafe {
            for dir in 0..2 {
                let (ep, cb): (u8, ffi::libusb_transfer_cb_fn) = if dir == 0 {
                    (EP_CAPTURE, cap_cb::<C, P>)
                } else {
                    (EP_PLAYBACK, play_cb::<C, P>)
                };
                for _ in 0..NUM {
                    let mut buf = vec![0u8; BUFLEN];
                    let x = ffi::libusb_alloc_transfer(PKTS as i32);
                    ffi::libusb_fill_iso_transfer(x, handle, ep, buf.as_mut_ptr(), BUFLEN as i32,
                        PKTS as i32, cb, stp as *mut c_void, 1000);
                    if dir == 0 {
                        ffi::libusb_set_iso_packet_lengths(x, PKT as u32);
                    } else {
                        build_out(&mut *stp, x); // nominal prime (feedback queue empty at startup)
                    }
                    if ffi::libusb_submit_transfer(x) != 0 {
                        err = Some(Error::Transport("iso submit failed at startup".into()));
                        ffi::libusb_free_transfer(x);
                        break;
                    }
                    (*stp).inflight += 1;
                    xfers.push(x);
                    bufs.push(buf);
                }
                if err.is_some() { break; }
            }

            // Event loop: callbacks resubmit. Nothing here may block (RT audio thread).
            if err.is_none() {
                while !stop.load(Ordering::Relaxed) && (*stp).inflight > 0 {
                    let tv = libc::timeval { tv_sec: 0, tv_usec: 100_000 };
                    ffi::libusb_handle_events_timeout_completed(ctx, &tv, ptr::null_mut());
                }
            }

            (*stp).running = false;
            for &x in &xfers { ffi::libusb_cancel_transfer(x); }
            while (*stp).inflight > 0 {
                let tv = libc::timeval { tv_sec: 0, tv_usec: 100_000 };
                ffi::libusb_handle_events_timeout_completed(ctx, &tv, ptr::null_mut());
            }
            for &x in &xfers { ffi::libusb_free_transfer(x); }
            drop(Box::from_raw(stp));
        }

        drop(bufs);
        let _ = self.handle.set_alternate_setting(IF_CAPTURE, 0);
        let _ = self.handle.set_alternate_setting(IF_PLAYBACK, 0);
        match err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }
}
