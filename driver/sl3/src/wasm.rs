//! wasm-bindgen surface: a JS-callable `Sl3Device` class backed by the WebUSB
//! transport. Every device operation returns a Promise.
//!
//! Control ops run against a persistent control handle; capture/playback each
//! run on their own cloned device handle (same underlying USB device, disjoint
//! interfaces) so streaming does not block control calls — mirroring the native
//! daemon's split.

use crate::device::Device;
use crate::transport::webusb::WebUsbTransport;
use crate::{CHANNELS, IF_CAPTURE, IF_HID, IF_PLAYBACK, PID, VID};
use std::cell::Cell;
use std::rc::Rc;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::{spawn_local, JsFuture};

fn e2js(e: crate::Error) -> JsValue {
    JsValue::from_str(&e.to_string())
}

// 24-bit (sign-extended i32) <-> float32 [-1,1). The driver works in i24 internally;
// the *F32 endpoints convert at the JS boundary so callers can use Web-Audio-native
// Float32Array without scaling.
const F_SCALE: f32 = 8_388_608.0; // 2^23
#[inline]
fn i24_to_f32(s: i32) -> f32 { s as f32 / F_SCALE }
#[inline]
fn f32_to_i24(f: f32) -> i32 { (f * F_SCALE).clamp(-F_SCALE, F_SCALE - 1.0) as i32 }

/// An open SL 3 in the browser. Obtain one with `await Sl3Device.open()` (must be
/// called from a user gesture — WebUSB requires it).
#[wasm_bindgen]
pub struct Sl3Device {
    device: web_sys::UsbDevice,
    ctrl: Device<WebUsbTransport>,
    stop: Rc<Cell<bool>>,
    rate: Rc<Cell<u32>>,
}

/// `navigator.usb` from either a Window or a WorkerGlobalScope. WebUSB device
/// *selection* (`requestDevice`) is main-thread-only, but a Worker can reach an
/// already-granted device via `getDevices()` — which is how we run streaming off
/// the main thread (its event-loop jank starves the iso-OUT endpoint otherwise).
fn get_usb() -> Result<web_sys::Usb, JsValue> {
    let g = js_sys::global();
    if let Some(w) = g.dyn_ref::<web_sys::Window>() {
        return Ok(w.navigator().usb());
    }
    if let Some(ws) = g.dyn_ref::<web_sys::WorkerGlobalScope>() {
        return Ok(ws.navigator().usb());
    }
    Err(JsValue::from_str("navigator.usb unavailable (not a Window or Worker)"))
}

#[wasm_bindgen]
impl Sl3Device {
    async fn finish_open(device: web_sys::UsbDevice) -> Result<Sl3Device, JsValue> {
        JsFuture::from(device.open()).await?;
        JsFuture::from(device.select_configuration(1)).await?;
        for iface in [IF_PLAYBACK, IF_CAPTURE, IF_HID] {
            JsFuture::from(device.claim_interface(iface)).await?;
        }
        let ctrl = Device::new(WebUsbTransport::new(device.clone()), true, true);
        Ok(Sl3Device {
            device,
            ctrl,
            stop: Rc::new(Cell::new(false)),
            rate: Rc::new(Cell::new(48000)),
        })
    }

    /// Prompt the user to pick the SL 3 (main thread / user gesture only), open it,
    /// select config 1, and claim the playback/capture/HID interfaces.
    pub async fn open() -> Result<Sl3Device, JsValue> {
        console_error_panic_hook::set_once();
        let usb = get_usb()?;
        let filter = web_sys::UsbDeviceFilter::new();
        filter.set_vendor_id(VID);
        filter.set_product_id(PID);
        let opts = web_sys::UsbDeviceRequestOptions::new(&[filter]);
        let device: web_sys::UsbDevice = JsFuture::from(usb.request_device(&opts)).await?;
        Self::finish_open(device).await
    }

    /// Open an already-granted SL 3 without a picker — works inside a Worker (where
    /// `requestDevice` is unavailable). The main thread must have granted access
    /// (via `open()`/`requestDevice`) first.
    #[wasm_bindgen(js_name = openGranted)]
    pub async fn open_granted() -> Result<Sl3Device, JsValue> {
        console_error_panic_hook::set_once();
        let usb = get_usb()?;
        let list = JsFuture::from(usb.get_devices()).await?;
        let arr = js_sys::Array::from(&list);
        for i in 0..arr.length() {
            let d: web_sys::UsbDevice = arr.get(i).dyn_into()?;
            if d.vendor_id() == VID && d.product_id() == PID {
                return Self::finish_open(d).await;
            }
        }
        Err(JsValue::from_str("SL 3 not in getDevices() — grant it on the main thread first"))
    }

    /// Set the device sample rate (44100 or 48000).
    #[wasm_bindgen(js_name = setSampleRate)]
    pub async fn set_sample_rate(&mut self, hz: u32) -> Result<(), JsValue> {
        self.ctrl.set_sample_rate(hz).await.map_err(e2js)?;
        self.rate.set(hz);
        Ok(())
    }

    /// Read the current sample rate back from the device.
    #[wasm_bindgen(js_name = sampleRate)]
    pub async fn sample_rate(&mut self) -> Result<u32, JsValue> {
        self.ctrl.sample_rate().await.map_err(e2js)
    }

    /// Read the single status byte.
    #[wasm_bindgen(js_name = getStatus)]
    pub async fn get_status(&mut self) -> Result<u8, JsValue> {
        self.ctrl.status_byte().await.map_err(e2js)
    }

    /// Read the 6-byte overload block.
    #[wasm_bindgen(js_name = getOverload)]
    pub async fn get_overload(&mut self) -> Result<Vec<u8>, JsValue> {
        Ok(self.ctrl.overload().await.map_err(e2js)?.to_vec())
    }

    /// Read `len` bytes of the audio-controls register file starting at `offset`.
    #[wasm_bindgen(js_name = getControls)]
    pub async fn get_controls(&mut self, offset: usize, len: usize) -> Result<Vec<u8>, JsValue> {
        let mut buf = vec![0u8; len];
        self.ctrl.get_audio_controls(offset, &mut buf).await.map_err(e2js)?;
        Ok(buf)
    }

    /// Write bytes into the audio-controls register file at `offset`.
    #[wasm_bindgen(js_name = setControls)]
    pub async fn set_controls(&mut self, offset: usize, data: Vec<u8>) -> Result<(), JsValue> {
        self.ctrl.set_audio_controls(offset, &data).await.map_err(e2js)
    }

    /// Start 6ch/24-bit capture. `cb(samples: Int32Array, frames: number)` is
    /// called per transfer with channel-interleaved sign-extended samples; return
    /// a truthy value to stop. Runs until stopped or a transport error.
    #[wasm_bindgen(js_name = startCapture)]
    pub fn start_capture(&self, cb: js_sys::Function) {
        let device = self.device.clone();
        let stop = self.stop.clone();
        stop.set(false);
        spawn_local(async move {
            let dev = Device::new(WebUsbTransport::new(device), true, false);
            let _ = crate::stream::capture(&dev, |planar, frames| {
                let arr = js_sys::Int32Array::new_with_length((frames * CHANNELS) as u32);
                for i in 0..frames {
                    for c in 0..CHANNELS {
                        arr.set_index((i * CHANNELS + c) as u32, planar[c][i]);
                    }
                }
                let js_stop = cb
                    .call2(&JsValue::NULL, &arr, &JsValue::from(frames as u32))
                    .map(|v| v.is_truthy())
                    .unwrap_or(true);
                stop.get() || js_stop
            })
            .await;
        });
    }

    /// Start 6ch/24-bit playback. `cb(frames: number)` must return an
    /// Int32Array of `frames * 6` channel-interleaved samples to play, or a
    /// non-array (e.g. null) to stop. Runs until stopped or a transport error.
    #[wasm_bindgen(js_name = startPlayback)]
    pub fn start_playback(&self, cb: js_sys::Function) {
        let device = self.device.clone();
        let stop = self.stop.clone();
        let rate = self.rate.get();
        stop.set(false);
        spawn_local(async move {
            let mut dev = Device::new(WebUsbTransport::new(device), true, false);
            dev.set_pacing_rate(rate);
            let _ = crate::stream::playback(&dev, |planar, frames| {
                let ret = cb.call1(&JsValue::NULL, &JsValue::from(frames as u32));
                let arr: js_sys::Int32Array = match ret {
                    Ok(v) => match v.dyn_into() {
                        Ok(a) => a,
                        Err(_) => return true, // non-array -> stop
                    },
                    Err(_) => return true,
                };
                let n = arr.length();
                for i in 0..frames {
                    for c in 0..CHANNELS {
                        let idx = (i * CHANNELS + c) as u32;
                        planar[c][i] = if idx < n { arr.get_index(idx) } else { 0 };
                    }
                }
                stop.get()
            })
            .await;
        });
    }

    /// Like `startPlayback`, but full-duplex with **implicit feedback**: mirrors the
    /// capture endpoint's per-packet frame counts onto OUT so playback tracks the
    /// device clock (no nominal-rate drift). Best over WebUSB from a Worker.
    #[wasm_bindgen(js_name = startPlaybackFd)]
    pub fn start_playback_fd(&self, cb: js_sys::Function) {
        let device = self.device.clone();
        let stop = self.stop.clone();
        let rate = self.rate.get();
        stop.set(false);
        spawn_local(async move {
            let mut dev = Device::new(WebUsbTransport::new(device), true, false);
            dev.set_pacing_rate(rate);
            let _ = crate::stream::playback_fd(&dev, |planar, frames| {
                let ret = cb.call1(&JsValue::NULL, &JsValue::from(frames as u32));
                let arr: js_sys::Int32Array = match ret {
                    Ok(v) => match v.dyn_into() {
                        Ok(a) => a,
                        Err(_) => return true,
                    },
                    Err(_) => return true,
                };
                let n = arr.length();
                for i in 0..frames {
                    for c in 0..CHANNELS {
                        let idx = (i * CHANNELS + c) as u32;
                        planar[c][i] = if idx < n { arr.get_index(idx) } else { 0 };
                    }
                }
                stop.get()
            })
            .await;
        });
    }

    /// Simultaneous capture + playback in one full-duplex stream (the SL 3 has a
    /// single iso IN endpoint, so this is the only way to do both at once).
    /// `capCb(samples: Int32Array, frames)` receives capture; `playCb(frames)` returns
    /// an Int32Array to play. Either returning truthy / a non-array stops.
    #[wasm_bindgen(js_name = startDuplex)]
    pub fn start_duplex(&self, cap_cb: js_sys::Function, play_cb: js_sys::Function) {
        let device = self.device.clone();
        let stop = self.stop.clone();
        let stop_cap = stop.clone();
        let rate = self.rate.get();
        stop.set(false);
        spawn_local(async move {
            let mut dev = Device::new(WebUsbTransport::new(device), true, false);
            dev.set_pacing_rate(rate);
            let _ = crate::stream::duplex(
                &dev,
                |planar, frames| {
                    let arr = js_sys::Int32Array::new_with_length((frames * CHANNELS) as u32);
                    for i in 0..frames {
                        for c in 0..CHANNELS {
                            arr.set_index((i * CHANNELS + c) as u32, planar[c][i]);
                        }
                    }
                    let js_stop = cap_cb
                        .call2(&JsValue::NULL, &arr, &JsValue::from(frames as u32))
                        .map(|v| v.is_truthy())
                        .unwrap_or(true);
                    stop_cap.get() || js_stop
                },
                |planar, frames| {
                    let ret = play_cb.call1(&JsValue::NULL, &JsValue::from(frames as u32));
                    let arr: js_sys::Int32Array = match ret {
                        Ok(v) => match v.dyn_into() {
                            Ok(a) => a,
                            Err(_) => return true,
                        },
                        Err(_) => return true,
                    };
                    let n = arr.length();
                    for i in 0..frames {
                        for c in 0..CHANNELS {
                            let idx = (i * CHANNELS + c) as u32;
                            planar[c][i] = if idx < n { arr.get_index(idx) } else { 0 };
                        }
                    }
                    stop.get()
                },
            )
            .await;
        });
    }

    // ---- Float32 variants (Web-Audio-native; convert i24<->f32 internally) ----

    /// Like `startCapture`, but the callback gets a `Float32Array` in [-1,1).
    #[wasm_bindgen(js_name = startCaptureF32)]
    pub fn start_capture_f32(&self, cb: js_sys::Function) {
        let device = self.device.clone();
        let stop = self.stop.clone();
        stop.set(false);
        spawn_local(async move {
            let dev = Device::new(WebUsbTransport::new(device), true, false);
            let _ = crate::stream::capture(&dev, |planar, frames| {
                let arr = js_sys::Float32Array::new_with_length((frames * CHANNELS) as u32);
                for i in 0..frames {
                    for c in 0..CHANNELS {
                        arr.set_index((i * CHANNELS + c) as u32, i24_to_f32(planar[c][i]));
                    }
                }
                let js_stop = cb
                    .call2(&JsValue::NULL, &arr, &JsValue::from(frames as u32))
                    .map(|v| v.is_truthy())
                    .unwrap_or(true);
                stop.get() || js_stop
            })
            .await;
        });
    }

    /// Like `startPlaybackFd`, but the callback returns a `Float32Array` in [-1,1).
    #[wasm_bindgen(js_name = startPlaybackF32)]
    pub fn start_playback_f32(&self, cb: js_sys::Function) {
        let device = self.device.clone();
        let stop = self.stop.clone();
        let rate = self.rate.get();
        stop.set(false);
        spawn_local(async move {
            let mut dev = Device::new(WebUsbTransport::new(device), true, false);
            dev.set_pacing_rate(rate);
            let _ = crate::stream::playback_fd(&dev, |planar, frames| {
                let ret = cb.call1(&JsValue::NULL, &JsValue::from(frames as u32));
                let arr: js_sys::Float32Array = match ret {
                    Ok(v) => match v.dyn_into() {
                        Ok(a) => a,
                        Err(_) => return true,
                    },
                    Err(_) => return true,
                };
                let n = arr.length();
                for i in 0..frames {
                    for c in 0..CHANNELS {
                        let idx = (i * CHANNELS + c) as u32;
                        planar[c][i] = if idx < n { f32_to_i24(arr.get_index(idx)) } else { 0 };
                    }
                }
                stop.get()
            })
            .await;
        });
    }

    /// Like `startDuplex`, but both callbacks use `Float32Array` in [-1,1).
    #[wasm_bindgen(js_name = startDuplexF32)]
    pub fn start_duplex_f32(&self, cap_cb: js_sys::Function, play_cb: js_sys::Function) {
        let device = self.device.clone();
        let stop = self.stop.clone();
        let stop_cap = stop.clone();
        let rate = self.rate.get();
        stop.set(false);
        spawn_local(async move {
            let mut dev = Device::new(WebUsbTransport::new(device), true, false);
            dev.set_pacing_rate(rate);
            let _ = crate::stream::duplex(
                &dev,
                |planar, frames| {
                    let arr = js_sys::Float32Array::new_with_length((frames * CHANNELS) as u32);
                    for i in 0..frames {
                        for c in 0..CHANNELS {
                            arr.set_index((i * CHANNELS + c) as u32, i24_to_f32(planar[c][i]));
                        }
                    }
                    let js_stop = cap_cb
                        .call2(&JsValue::NULL, &arr, &JsValue::from(frames as u32))
                        .map(|v| v.is_truthy())
                        .unwrap_or(true);
                    stop_cap.get() || js_stop
                },
                |planar, frames| {
                    let ret = play_cb.call1(&JsValue::NULL, &JsValue::from(frames as u32));
                    let arr: js_sys::Float32Array = match ret {
                        Ok(v) => match v.dyn_into() {
                            Ok(a) => a,
                            Err(_) => return true,
                        },
                        Err(_) => return true,
                    };
                    let n = arr.length();
                    for i in 0..frames {
                        for c in 0..CHANNELS {
                            let idx = (i * CHANNELS + c) as u32;
                            planar[c][i] = if idx < n { f32_to_i24(arr.get_index(idx)) } else { 0 };
                        }
                    }
                    stop.get()
                },
            )
            .await;
        });
    }

    /// Signal any running capture/playback task to stop after its next transfer.
    pub fn stop(&self) {
        self.stop.set(true);
    }
}
