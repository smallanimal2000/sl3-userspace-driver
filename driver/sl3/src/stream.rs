//! Transport-generic isochronous audio: 6ch/24-bit LE (de)interleave plus the
//! multi-transfer pipeline that keeps several iso transfers in flight.
//!
//! This layer is identical on every backend. It drives a fixed number of
//! concurrent [`UsbTransport::iso_in`]/[`UsbTransport::iso_out`] transfers via a
//! [`FuturesUnordered`]: on native each is a libusb async transfer completed by
//! the event-pump thread; in the browser each is an `isochronousTransfer*`
//! Promise. Fractional rate-pacing for the async OUT endpoint is unchanged from
//! the original blocking driver.

use crate::device::Device;
use crate::transport::{IsoPacket, UsbTransport};
use crate::{Error, Result, BYTES_PER_SAMP, CHANNELS, EP_CAPTURE, EP_PLAYBACK, FRAME_BYTES,
            IF_CAPTURE, IF_PLAYBACK};
use futures::future::{FutureExt, LocalBoxFuture};
use futures::stream::{FuturesUnordered, StreamExt};
use std::collections::VecDeque;

// Browser/CLI iso pipeline depth. WebUSB resubmits transfers from the JS microtask
// queue, which jitters by tens of ms; keep a deep queue so the OUT endpoint never
// starves between resubmissions (8 ≈ 16 ms was too shallow → audible clicking).
// (The native daemon does NOT use this path — it uses native_audio.rs.)
const NUM_TRANSFERS: usize = 32;
const ISO_PKTS: usize = 16;
const PKT_LEN: usize = 126; // wMaxPacketSize of the iso endpoints

#[inline]
fn s24le(b: &[u8]) -> i32 {
    let mut v = (b[0] as u32) | ((b[1] as u32) << 8) | ((b[2] as u32) << 16);
    if v & 0x0080_0000 != 0 {
        v |= 0xFF00_0000;
    }
    v as i32
}

#[inline]
fn put_s24le(b: &mut [u8], s: i32) {
    b[0] = (s & 0xff) as u8;
    b[1] = ((s >> 8) & 0xff) as u8;
    b[2] = ((s >> 16) & 0xff) as u8;
}

// ------------------------------- capture (IN) -------------------------------

/// Capture 6ch/24-bit input. `on_frames(planar, frames)` receives planar i32
/// (sign-extended 24-bit); return `true` to stop. Completes when stopped or on
/// a transport error. Awaitable: `block_on` it natively, or drive it on the JS
/// microtask queue in the browser.
pub async fn capture<T, F>(dev: &Device<T>, mut on_frames: F) -> Result<()>
where
    T: UsbTransport,
    F: FnMut(&[Vec<i32>], usize) -> bool,
{
    if !dev.has_audio() {
        return Err(Error::State("handle has no audio interfaces"));
    }
    let t = dev.transport();
    t.set_alt(IF_CAPTURE, 1).await?;

    let pkt_lens = [PKT_LEN as u32; ISO_PKTS];
    let mut planar: Vec<Vec<i32>> = (0..CHANNELS).map(|_| Vec::new()).collect();
    let mut inflight = FuturesUnordered::new();
    for _ in 0..NUM_TRANSFERS {
        inflight.push(t.iso_in(EP_CAPTURE, &pkt_lens));
    }

    let mut stop = false;
    let mut err_streak = 0u32;
    let mut result = Ok(());
    while let Some(res) = inflight.next().await {
        let packets = match res {
            Ok(p) => { err_streak = 0; p }
            Err(e) => {
                err_streak += 1;
                if err_streak > MAX_ERR_STREAK { result = Err(e); break; }
                if !stop { inflight.push(t.iso_in(EP_CAPTURE, &pkt_lens)); }
                continue;
            }
        };

        let total: usize = packets.iter().filter(|p| p.ok).map(|p| p.data.len() / FRAME_BYTES).sum();
        if total > 0 {
            for c in 0..CHANNELS {
                if planar[c].len() < total { planar[c].resize(total, 0); }
            }
            let mut f = 0usize;
            for p in &packets {
                if !p.ok { continue; }
                let nf = p.data.len() / FRAME_BYTES;
                for i in 0..nf {
                    let frame = &p.data[i * FRAME_BYTES..];
                    for c in 0..CHANNELS {
                        planar[c][f] = s24le(&frame[c * BYTES_PER_SAMP..]);
                    }
                    f += 1;
                }
            }
            if on_frames(&planar, total) { stop = true; }
        }

        if !stop {
            inflight.push(t.iso_in(EP_CAPTURE, &pkt_lens));
        }
        // When stopping we stop resubmitting; the loop drains the remaining
        // in-flight transfers until `inflight` is empty.
    }

    let _ = t.set_alt(IF_CAPTURE, 0).await;
    result
}

// ------------------------------ playback (OUT) ------------------------------

/// One OUT transfer, as a named async fn so every call has the *same* opaque
/// future type (required to collect them in a single `FuturesUnordered`).
async fn iso_out_transfer<T: UsbTransport>(t: &T, data: Vec<u8>, lens: Vec<u32>) -> Result<()> {
    t.iso_out(EP_PLAYBACK, &data, &lens).await
}

/// Build one OUT transfer's bytes + per-packet lengths from the app callback,
/// advancing the fractional frames/microframe pacing accumulator. Returns `None`
/// if the callback asked to stop.
fn build_out<F>(
    rate: u32,
    phase: &mut u32,
    planar: &mut Vec<Vec<i32>>,
    fill: &mut F,
) -> Option<(Vec<u8>, Vec<u32>)>
where
    F: FnMut(&mut [Vec<i32>], usize) -> bool,
{
    let mut data: Vec<u8> = Vec::new();
    let mut lens: Vec<u32> = Vec::with_capacity(ISO_PKTS);
    for _ in 0..ISO_PKTS {
        *phase += rate;
        let nf = (*phase / 8000) as usize;
        *phase %= 8000;
        if planar[0].len() < nf {
            for c in 0..CHANNELS { planar[c].resize(nf, 0); }
        }
        if fill(planar, nf) { return None; }
        let start = data.len();
        data.resize(start + nf * FRAME_BYTES, 0);
        for i in 0..nf {
            for c in 0..CHANNELS {
                let off = start + (i * CHANNELS + c) * BYTES_PER_SAMP;
                put_s24le(&mut data[off..off + BYTES_PER_SAMP], planar[c][i]);
            }
        }
        lens.push((nf * FRAME_BYTES) as u32);
    }
    Some((data, lens))
}

/// Play 6ch/24-bit output. `fill(planar, frames)` must write `frames` samples per
/// channel into `planar`; return `true` to stop. Completes when stopped or on a
/// transport error.
pub async fn playback<T, F>(dev: &Device<T>, mut fill: F) -> Result<()>
where
    T: UsbTransport,
    F: FnMut(&mut [Vec<i32>], usize) -> bool,
{
    if !dev.has_audio() {
        return Err(Error::State("handle has no audio interfaces"));
    }
    let t = dev.transport();
    t.set_alt(IF_PLAYBACK, 1).await?;
    let rate = if dev.cached_rate() != 0 { dev.cached_rate() } else { 48000 };

    let mut phase = 0u32;
    let mut planar: Vec<Vec<i32>> = (0..CHANNELS).map(|_| Vec::new()).collect();
    let mut inflight = FuturesUnordered::new();
    let mut stop = false;

    for _ in 0..NUM_TRANSFERS {
        match build_out(rate, &mut phase, &mut planar, &mut fill) {
            Some((data, lens)) => {
                inflight.push(iso_out_transfer(t, data, lens));
            }
            None => { stop = true; break; }
        }
    }

    let mut result = Ok(());
    while let Some(res) = inflight.next().await {
        if let Err(e) = res { result = Err(e); break; }
        if !stop {
            match build_out(rate, &mut phase, &mut planar, &mut fill) {
                Some((data, lens)) => {
                    inflight.push(iso_out_transfer(t, data, lens));
                }
                None => stop = true,
            }
        }
    }

    let _ = t.set_alt(IF_PLAYBACK, 0).await;
    result
}

// -------------------- full-duplex playback (implicit feedback) --------------------

const FB_MAX: usize = 4 * NUM_TRANSFERS;
// Tolerate transient iso errors (a WebUSB hiccup should not permanently kill the
// stream): retry on error, give up only after this many consecutive failures.
const MAX_ERR_STREAK: u32 = 24;

/// Build one OUT transfer, mirroring the IN endpoint's per-packet frame counts if
/// feedback is available, else the nominal fractional pacing. `None` = stop.
fn build_out_fb<F>(
    rate: u32,
    phase: &mut u32,
    fb: &mut VecDeque<Vec<u32>>,
    planar: &mut Vec<Vec<i32>>,
    fill: &mut F,
) -> Option<(Vec<u8>, Vec<u32>)>
where
    F: FnMut(&mut [Vec<i32>], usize) -> bool,
{
    let sizes: Vec<usize> = match fb.pop_front() {
        Some(fbs) => fbs.iter().map(|&s| s as usize).collect(),
        None => (0..ISO_PKTS)
            .map(|_| { *phase += rate; let n = (*phase / 8000) as usize; *phase %= 8000; n })
            .collect(),
    };
    let mut data: Vec<u8> = Vec::new();
    let mut lens: Vec<u32> = Vec::with_capacity(sizes.len());
    for &nf in &sizes {
        if planar[0].len() < nf {
            for c in 0..CHANNELS { planar[c].resize(nf, 0); }
        }
        if fill(planar, nf) { return None; }
        let start = data.len();
        data.resize(start + nf * FRAME_BYTES, 0);
        for i in 0..nf {
            for c in 0..CHANNELS {
                let off = start + (i * CHANNELS + c) * BYTES_PER_SAMP;
                put_s24le(&mut data[off..off + BYTES_PER_SAMP], planar[c][i]);
            }
        }
        lens.push((nf * FRAME_BYTES) as u32);
    }
    Some((data, lens))
}

enum Done {
    In(Result<Vec<IsoPacket>>),
    Out(Result<()>),
}

/// Full-duplex capture **and** playback in a single stream. The SL 3 has one iso IN
/// endpoint, so capture and playback cannot be independent streams — IN is read once
/// and used for BOTH: delivering capture audio to `on_cap`, and driving OUT via
/// implicit feedback (mirror IN's per-packet frame counts onto OUT so playback tracks
/// the device clock — the fix that made native glitch-free). OUT is built from `fill`.
/// Either callback returning `true` stops. Nominal pacing until the first feedback.
pub async fn duplex<T, C, F>(dev: &Device<T>, mut on_cap: C, mut fill: F) -> Result<()>
where
    T: UsbTransport,
    C: FnMut(&[Vec<i32>], usize) -> bool,
    F: FnMut(&mut [Vec<i32>], usize) -> bool,
{
    if !dev.has_audio() {
        return Err(Error::State("handle has no audio interfaces"));
    }
    let t = dev.transport();
    t.set_alt(IF_CAPTURE, 1).await?;
    t.set_alt(IF_PLAYBACK, 1).await?;
    let rate = if dev.cached_rate() != 0 { dev.cached_rate() } else { 48000 };

    let pkt_lens = [PKT_LEN as u32; ISO_PKTS];
    let mut fb: VecDeque<Vec<u32>> = VecDeque::new();
    let mut phase = 0u32;
    let mut cap_planar: Vec<Vec<i32>> = (0..CHANNELS).map(|_| Vec::new()).collect();
    let mut play_planar: Vec<Vec<i32>> = (0..CHANNELS).map(|_| Vec::new()).collect();
    let mut stop = false;
    let mut err_streak = 0u32;
    let mut result = Ok(());

    let mut inflight: FuturesUnordered<LocalBoxFuture<Done>> = FuturesUnordered::new();
    for _ in 0..NUM_TRANSFERS {
        inflight.push(async { Done::In(t.iso_in(EP_CAPTURE, &pkt_lens).await) }.boxed_local());
    }
    for _ in 0..NUM_TRANSFERS {
        match build_out_fb(rate, &mut phase, &mut fb, &mut play_planar, &mut fill) {
            Some((data, lens)) => {
                inflight.push(async move { Done::Out(iso_out_transfer(t, data, lens).await) }.boxed_local());
            }
            None => { stop = true; break; }
        }
    }

    while let Some(done) = inflight.next().await {
        match done {
            Done::In(Ok(packets)) => {
                err_streak = 0;
                let total: usize = packets.iter().filter(|p| p.ok).map(|p| p.data.len() / FRAME_BYTES).sum();
                // Per-packet frame counts — the OUT clock (implicit feedback).
                let sizes: Vec<u32> = packets.iter()
                    .map(|p| if p.ok { (p.data.len() / FRAME_BYTES) as u32 } else { 0 })
                    .collect();
                // Deinterleave and deliver the capture audio.
                if total > 0 {
                    for c in 0..CHANNELS {
                        if cap_planar[c].len() < total { cap_planar[c].resize(total, 0); }
                    }
                    let mut f = 0usize;
                    for p in &packets {
                        if !p.ok { continue; }
                        let nf = p.data.len() / FRAME_BYTES;
                        for i in 0..nf {
                            let frame = &p.data[i * FRAME_BYTES..];
                            for c in 0..CHANNELS {
                                cap_planar[c][f] = s24le(&frame[c * BYTES_PER_SAMP..]);
                            }
                            f += 1;
                        }
                    }
                    if on_cap(&cap_planar, total) { stop = true; }
                }
                if sizes.iter().any(|&s| s > 0) && fb.len() < FB_MAX {
                    fb.push_back(sizes);
                }
                if !stop {
                    inflight.push(async { Done::In(t.iso_in(EP_CAPTURE, &pkt_lens).await) }.boxed_local());
                }
            }
            Done::In(Err(e)) => {
                err_streak += 1;
                if err_streak > MAX_ERR_STREAK { result = Err(e); break; }
                if !stop {
                    inflight.push(async { Done::In(t.iso_in(EP_CAPTURE, &pkt_lens).await) }.boxed_local());
                }
            }
            Done::Out(Ok(())) => {
                err_streak = 0;
                if !stop {
                    match build_out_fb(rate, &mut phase, &mut fb, &mut play_planar, &mut fill) {
                        Some((data, lens)) => {
                            inflight.push(async move { Done::Out(iso_out_transfer(t, data, lens).await) }.boxed_local());
                        }
                        None => stop = true,
                    }
                }
            }
            Done::Out(Err(e)) => {
                err_streak += 1;
                if err_streak > MAX_ERR_STREAK { result = Err(e); break; }
                if !stop {
                    match build_out_fb(rate, &mut phase, &mut fb, &mut play_planar, &mut fill) {
                        Some((data, lens)) => {
                            inflight.push(async move { Done::Out(iso_out_transfer(t, data, lens).await) }.boxed_local());
                        }
                        None => stop = true,
                    }
                }
            }
        }
    }

    let _ = t.set_alt(IF_PLAYBACK, 0).await;
    let _ = t.set_alt(IF_CAPTURE, 0).await;
    result
}

/// Glitch-free playback via full-duplex implicit feedback, capture audio discarded.
pub async fn playback_fd<T, F>(dev: &Device<T>, fill: F) -> Result<()>
where
    T: UsbTransport,
    F: FnMut(&mut [Vec<i32>], usize) -> bool,
{
    duplex(dev, |_, _| false, fill).await
}
