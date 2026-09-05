//! Native USB backend: `rusb` for lifecycle/claim + raw `libusb1-sys` async
//! transfers for everything on the wire, driven by a dedicated libusb event-pump
//! thread.
//!
//! Every USB operation is expressed as a libusb *async* transfer so the whole
//! backend shares one event-handling discipline: one background thread calls
//! `libusb_handle_events_timeout_completed`, and each transfer's completion
//! callback records its result and wakes the `Future` awaiting it. (Interface
//! claiming is a kernel ioctl, not a transfer, so it stays synchronous;
//! `set_alt` is issued as an async control transfer to avoid mixing libusb's
//! synchronous control path with the event-pump thread.)

use crate::device::Device;
use crate::transport::{IsoPacket, UsbTransport};
use crate::{Error, Result, EP_HID_IN, EP_HID_OUT, HID_REPORT_LEN, IF_CAPTURE, IF_HID, IF_PLAYBACK,
            PID, VID};
use async_trait::async_trait;
use libusb1_sys as ffi;
use libusb1_sys::constants::*;
use rusb::{Context, DeviceHandle, UsbContext};
use std::future::Future;
use std::os::raw::c_void;
use std::pin::Pin;
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context as TaskCx, Poll, Waker};
use std::thread::JoinHandle;
use std::time::Duration;

/// Result of one completed libusb transfer, handed from the callback (pump
/// thread) to the awaiting future.
enum XferResult {
    /// Control / interrupt transfer: overall status + received bytes.
    Plain { status: i32, data: Vec<u8> },
    /// Isochronous transfer: per-packet (status, bytes). Overall status is not
    /// meaningful for iso; only per-packet status matters.
    Iso { packets: Vec<(i32, Vec<u8>)> },
}

#[derive(Default)]
struct XferState {
    completed: bool,
    result: Option<XferResult>,
    waker: Option<Waker>,
}

/// Owns one in-flight (or completed) libusb transfer plus the heap buffer libusb
/// reads/writes and the shared state the callback fills in.
struct Transfer {
    ptr: *mut ffi::libusb_transfer,
    buf: Vec<u8>,
    state: Arc<Mutex<XferState>>,
    user_data: *mut c_void,
    submitted: bool,
}

impl Drop for Transfer {
    fn drop(&mut self) {
        unsafe {
            let completed = self.state.lock().unwrap().completed;
            if !self.submitted || completed {
                // Safe to reclaim: libusb is done with this transfer.
                ffi::libusb_free_transfer(self.ptr);
                drop(Arc::from_raw(self.user_data as *const Mutex<XferState>));
            } else {
                // Dropped mid-flight (abnormal teardown): cancel and intentionally
                // leak the transfer, its buffer, and the callback's Arc ref, since
                // the completion callback may still fire and touch them. A leak is
                // memory-safe; use-after-free would not be.
                ffi::libusb_cancel_transfer(self.ptr);
                let _ = std::mem::take(&mut self.buf).leak();
            }
        }
    }
}

/// Future that resolves when its transfer's callback fires.
struct XferFuture {
    xfer: Transfer,
}

impl Future for XferFuture {
    type Output = XferResult;
    fn poll(self: Pin<&mut Self>, cx: &mut TaskCx<'_>) -> Poll<XferResult> {
        let mut st = self.xfer.state.lock().unwrap();
        if st.completed {
            Poll::Ready(st.result.take().expect("transfer completed without a result"))
        } else {
            st.waker = Some(cx.waker().clone());
            Poll::Pending
        }
    }
}

/// libusb completion callback (runs on the event-pump thread). Copies the wire
/// bytes out, records the result, and wakes the awaiting future.
extern "system" fn xfer_cb(xfer: *mut ffi::libusb_transfer) {
    unsafe {
        let state = &*((*xfer).user_data as *const Mutex<XferState>);
        let status = (*xfer).status;
        let result = if (*xfer).transfer_type as i32 == LIBUSB_TRANSFER_TYPE_ISOCHRONOUS as i32 {
            let np = (*xfer).num_iso_packets as isize;
            let descs = (*xfer).iso_packet_desc.as_ptr();
            let base = (*xfer).buffer;
            let mut packets = Vec::with_capacity(np as usize);
            let mut off = 0usize;
            for p in 0..np {
                let d = &*descs.offset(p);
                let alen = d.actual_length as usize;
                let mut data = vec![0u8; alen];
                if alen > 0 {
                    ptr::copy_nonoverlapping(base.add(off), data.as_mut_ptr(), alen);
                }
                packets.push((d.status, data));
                off += d.length as usize; // packets are laid out at their requested length
            }
            let _ = status;
            XferResult::Iso { packets }
        } else {
            let alen = (*xfer).actual_length as usize;
            let mut data = vec![0u8; alen];
            if alen > 0 {
                ptr::copy_nonoverlapping((*xfer).buffer, data.as_mut_ptr(), alen);
            }
            XferResult::Plain { status, data }
        };
        let mut st = state.lock().unwrap();
        st.result = Some(result);
        st.completed = true;
        if let Some(w) = st.waker.take() {
            w.wake();
        }
    }
}

fn status_result(status: i32) -> Result<()> {
    if status == LIBUSB_TRANSFER_COMPLETED {
        Ok(())
    } else if status == LIBUSB_TRANSFER_TIMED_OUT {
        Err(Error::Transport("transfer timed out".into()))
    } else {
        Err(Error::Transport(format!("transfer status {status}")))
    }
}

/// The native transport: an open device handle plus its event-pump thread.
pub struct NativeTransport {
    handle: DeviceHandle<Context>,
    running: Arc<AtomicBool>,
    pump: Option<JoinHandle<()>>,
}

impl Drop for NativeTransport {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Relaxed);
        if let Some(j) = self.pump.take() {
            let _ = j.join();
        }
    }
}

impl NativeTransport {
    fn raw_handle(&self) -> *mut ffi::libusb_device_handle {
        self.handle.as_raw()
    }

    /// Alloc + submit a control/interrupt transfer. `buf` is the (owned) data
    /// buffer: for control it must already hold the 8-byte setup packet; for
    /// interrupt IN it is the read buffer, for interrupt OUT the payload.
    unsafe fn submit_plain(
        &self,
        transfer_type: u8,
        endpoint: u8,
        mut buf: Vec<u8>,
        length: i32,
        timeout_ms: u32,
    ) -> Result<XferFuture> {
        let state = Arc::new(Mutex::new(XferState::default()));
        let user_data = Arc::into_raw(state.clone()) as *mut c_void;
        let x = ffi::libusb_alloc_transfer(0);
        if x.is_null() {
            drop(Arc::from_raw(user_data as *const Mutex<XferState>));
            return Err(Error::Transport("libusb_alloc_transfer failed".into()));
        }
        (*x).dev_handle = self.raw_handle();
        (*x).endpoint = endpoint;
        (*x).transfer_type = transfer_type as u8;
        (*x).timeout = timeout_ms;
        (*x).buffer = buf.as_mut_ptr();
        (*x).length = length;
        (*x).num_iso_packets = 0;
        (*x).callback = xfer_cb;
        (*x).user_data = user_data;
        (*x).flags = 0;
        let mut t = Transfer { ptr: x, buf, state, user_data, submitted: false };
        if ffi::libusb_submit_transfer(x) != 0 {
            return Err(Error::Transport("libusb_submit_transfer failed".into()));
        }
        t.submitted = true;
        Ok(XferFuture { xfer: t })
    }

    /// Alloc + submit one isochronous transfer with per-packet lengths.
    unsafe fn submit_iso(
        &self,
        endpoint: u8,
        mut buf: Vec<u8>,
        packet_lengths: &[u32],
        timeout_ms: u32,
    ) -> Result<XferFuture> {
        let np = packet_lengths.len();
        let state = Arc::new(Mutex::new(XferState::default()));
        let user_data = Arc::into_raw(state.clone()) as *mut c_void;
        let x = ffi::libusb_alloc_transfer(np as i32);
        if x.is_null() {
            drop(Arc::from_raw(user_data as *const Mutex<XferState>));
            return Err(Error::Transport("libusb_alloc_transfer failed".into()));
        }
        ffi::libusb_fill_iso_transfer(
            x,
            self.raw_handle(),
            endpoint,
            buf.as_mut_ptr(),
            buf.len() as i32,
            np as i32,
            xfer_cb,
            user_data,
            timeout_ms,
        );
        let descs = (*x).iso_packet_desc.as_mut_ptr();
        for (p, &len) in packet_lengths.iter().enumerate() {
            (*descs.add(p)).length = len;
        }
        let mut t = Transfer { ptr: x, buf, state, user_data, submitted: false };
        let rc = ffi::libusb_submit_transfer(x);
        if rc != 0 {
            return Err(Error::Transport(format!("iso submit failed (libusb {rc}) ep 0x{endpoint:02x} np={np} buf={}", t.buf.len())));
        }
        t.submitted = true;
        Ok(XferFuture { xfer: t })
    }
}

#[async_trait(?Send)]
impl UsbTransport for NativeTransport {
    async fn claim_interface(&self, iface: u8) -> Result<()> {
        self.handle.claim_interface(iface).map_err(Error::Access)
    }

    async fn set_alt(&self, iface: u8, alt: u8) -> Result<()> {
        // SET_INTERFACE must go through IOKit's SetAlternateInterface on macOS — a
        // raw ep0 control transfer is rejected there. This is a synchronous ioctl
        // (not an event-loop transfer), so it does not race the pump thread.
        self.handle.set_alternate_setting(iface, alt).map_err(Error::Io)
    }

    async fn interrupt_out(&self, ep: u8, data: &[u8]) -> Result<()> {
        let len = data.len() as i32;
        let fut = unsafe {
            self.submit_plain(LIBUSB_TRANSFER_TYPE_INTERRUPT as u8, ep, data.to_vec(), len, 1000)?
        };
        match fut.await {
            XferResult::Plain { status, .. } => status_result(status),
            _ => unreachable!(),
        }
    }

    async fn interrupt_in(&self, ep: u8, len: usize, timeout: Duration) -> Result<Vec<u8>> {
        let ms = timeout.as_millis().min(u32::MAX as u128) as u32;
        let fut = unsafe {
            self.submit_plain(LIBUSB_TRANSFER_TYPE_INTERRUPT as u8, ep, vec![0u8; len], len as i32, ms)?
        };
        match fut.await {
            XferResult::Plain { status, data } => {
                if status == LIBUSB_TRANSFER_COMPLETED {
                    Ok(data)
                } else if status == LIBUSB_TRANSFER_TIMED_OUT {
                    Err(Error::Transport("interrupt in timed out".into()))
                } else {
                    Err(Error::Transport(format!("interrupt in status {status}")))
                }
            }
            _ => unreachable!(),
        }
    }

    async fn iso_in(&self, ep: u8, packet_lengths: &[u32]) -> Result<Vec<IsoPacket>> {
        let total: usize = packet_lengths.iter().map(|&l| l as usize).sum();
        let fut = unsafe { self.submit_iso(ep, vec![0u8; total], packet_lengths, 1000)? };
        match fut.await {
            XferResult::Iso { packets } => Ok(packets
                .into_iter()
                .map(|(st, data)| IsoPacket { data, ok: st == LIBUSB_TRANSFER_COMPLETED })
                .collect()),
            _ => unreachable!(),
        }
    }

    async fn iso_out(&self, ep: u8, data: &[u8], packet_lengths: &[u32]) -> Result<()> {
        let fut = unsafe { self.submit_iso(ep, data.to_vec(), packet_lengths, 1000)? };
        let _ = fut.await; // OUT: per-packet status not surfaced; drop pacing is inaudible
        Ok(())
    }
}

// ---- native constructors (mirror the old blocking `Sl3::open*`) ----

impl Device<NativeTransport> {
    /// Full (streaming + control) — standalone tools.
    pub fn open() -> Result<Self> { Self::open_role(true, true, true) }
    /// Both streaming interfaces — combined audio owner.
    pub fn open_audio() -> Result<Self> { Self::open_role(true, true, false) }
    /// HID control interface only — control panel / CLI.
    pub fn open_control() -> Result<Self> { Self::open_role(false, false, true) }
    /// Capture interface only (its own libusb context/thread).
    pub fn open_capture() -> Result<Self> { Self::open_role(false, true, false) }
    /// Playback interface only.
    pub fn open_playback() -> Result<Self> { Self::open_role(true, false, false) }

    fn open_role(playback: bool, capture: bool, control: bool) -> Result<Self> {
        let ctx = Context::new().map_err(Error::Io)?;
        let handle = ctx.open_device_with_vid_pid(VID, PID).ok_or(Error::NoDevice)?;

        // Only set config if not already 1, so a second (disjoint) owner doesn't
        // disturb the first.
        if handle.active_configuration().map_err(Error::Io)? != 1 {
            let _ = handle.set_active_configuration(1);
        }
        let _ = handle.set_auto_detach_kernel_driver(true); // no-op / Err on macOS

        let mut ifaces = Vec::new();
        if playback { ifaces.push(IF_PLAYBACK); }
        if capture { ifaces.push(IF_CAPTURE); }
        if control { ifaces.push(IF_HID); }
        for &i in &ifaces {
            handle.claim_interface(i).map_err(Error::Access)?;
        }

        if control {
            // Recover pipes left halted by a crashed session; drain a stale reply.
            // Done synchronously *before* the pump thread starts (no async transfers
            // are in flight yet, so this can't race the event loop).
            let _ = handle.clear_halt(EP_HID_OUT);
            let _ = handle.clear_halt(EP_HID_IN);
            let mut junk = [0u8; HID_REPORT_LEN];
            let _ = handle.read_interrupt(EP_HID_IN, &mut junk, Duration::from_millis(50));
        }

        // Event-pump thread: drives all async transfers on this context.
        let running = Arc::new(AtomicBool::new(true));
        let ctx_thread = ctx.clone();
        let r2 = running.clone();
        let pump = std::thread::spawn(move || {
            let tv = libc::timeval { tv_sec: 0, tv_usec: 100_000 };
            while r2.load(Ordering::Relaxed) {
                unsafe {
                    ffi::libusb_handle_events_timeout_completed(
                        ctx_thread.as_raw(),
                        &tv,
                        ptr::null_mut(),
                    );
                }
            }
        });

        let transport = NativeTransport { handle, running, pump: Some(pump) };
        Ok(Device::new(transport, playback || capture, control))
    }
}
