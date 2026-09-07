//! sl3d — the SL 3 audio daemon (Rust). Owns the USB device and bridges it to
//! the CoreAudio plugin (which cannot open USB) through a shared-memory ring.
//!
//! Uses the dedicated [`sl3::native_audio`] streaming path: one libusb context,
//! full-duplex, transfers resubmitted inside their completion callback on this
//! thread — the proven zero-gap model. `run()` blocks here and invokes the
//! capture/playback closures on this same thread (no cross-thread shm races).
mod shm;
use shm::Shm;
use sl3::native_audio::AudioDevice;
use sl3::CHANNELS;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

static STOP: AtomicBool = AtomicBool::new(false);

extern "C" { fn mach_absolute_time() -> u64; }

extern "C" fn on_signal(_: libc::c_int) { STOP.store(true, Ordering::Relaxed); }

fn main() {
    let rate: u32 = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(48000);
    unsafe {
        libc::signal(libc::SIGINT, on_signal as usize);
        libc::signal(libc::SIGTERM, on_signal as usize);
    }

    shm::assert_layout(); // fail fast if the Rust/C shm ABI ever drifts
    let shm = unsafe { shm::map(true) };
    if shm.is_null() { eprintln!("sl3d: shm map failed"); std::process::exit(1); }
    unsafe { shm::init_header(shm); }
    unsafe { (*shm).sample_rate.store(rate, Ordering::Relaxed); }
    println!("sl3d: shm ready ({} bytes), target rate {rate} Hz", std::mem::size_of::<Shm>());

    while !STOP.load(Ordering::Relaxed) {
        let dev = match AudioDevice::open() {
            Ok(d) => d,
            Err(_) => {
                unsafe { (*shm).device_present.store(0, Ordering::Relaxed); }
                std::thread::sleep(Duration::from_secs(1)); // wait for the device
                continue;
            }
        };
        println!("sl3d: device opened (audio)");

        // The device stays open across suspend/resume, but we only run the USB
        // isochronous stream while a CoreAudio client is doing IO (shm.io_running,
        // published by the plugin's StartIO/StopIO). When idle we drop the stream
        // and light-poll, so an idle device costs ~0% CPU instead of the ~20% the
        // always-on full-duplex stream burns. `stream_err` forces a device reopen.
        let mut stream_err = false;
        // Idle-loop iterations between USB liveness probes (~500 ms at 10 ms/iter).
        // We can't probe every tick (a control transfer per 10 ms is wasteful), but
        // must probe *something* or a detach-while-idle goes unnoticed forever.
        const PROBE_EVERY: u32 = 50;
        let mut idle_ticks: u32 = 0;
        while !STOP.load(Ordering::Relaxed) {
            // ---- Suspended: no client IO. Hold the device open but idle. ----
            if unsafe { (*shm).io_running.load(Ordering::Relaxed) } == 0 {
                // Periodically confirm the device is still attached. On detach, drop
                // the stale handle and re-enumerate — otherwise we'd keep publishing
                // device_present=1 for a device that's gone and never bind the one
                // the user plugs back in.
                idle_ticks += 1;
                if idle_ticks >= PROBE_EVERY {
                    idle_ticks = 0;
                    if !dev.is_present() {
                        unsafe { (*shm).device_present.store(0, Ordering::Relaxed); }
                        break; // -> reopen loop
                    }
                }
                unsafe {
                    (*shm).device_present.store(1, Ordering::Relaxed);
                    // Clear the device-clock anchor so the plugin's GetZeroTimeStamp
                    // falls back to host-locked timing instead of reusing a stale
                    // (cap_write, clock_host) pair while we produce no capture frames.
                    (*shm).clock_host.store(0, Ordering::Relaxed);
                }
                std::thread::sleep(Duration::from_millis(10)); // resume latency vs idle cost
                continue;
            }
            idle_ticks = 0;

            // ---- Active: a client started IO. Stream until it goes idle. ----
            // Rate is owned by the control side; we only pace playback to the shm value.
            let r = unsafe { (*shm).sample_rate.load(Ordering::Relaxed) };
            let r = if r == 44100 || r == 48000 { r } else { rate };
            unsafe {
                (*shm).sample_rate.store(r, Ordering::Relaxed);
                // Start each stream from an empty ring, but WITHOUT rewinding the
                // producer cursors. On a mid-stream device reopen the plugin has not
                // cycled StopIO/StartIO, so its cap_read has already drained forward
                // to the (frozen) cap_write. Zeroing cap_write here — as we used to —
                // would leave cap_read > cap_write, an unsigned underflow in the
                // plugin's ReadInput (avail ~= 2^64) that self-heals for playback (the
                // MAXLAT drop) but leaves capture permanently dead. Snapping each
                // consumer up to its producer instead drops any pending frames while
                // keeping both cursors monotonic, so avail can never underflow.
                let cw = (*shm).cap_write.load(Ordering::Relaxed);
                (*shm).cap_read.store(cw, Ordering::Relaxed);
                let pw = (*shm).play_write.load(Ordering::Relaxed);
                (*shm).play_read.store(pw, Ordering::Relaxed);
                (*shm).device_present.store(1, Ordering::Relaxed);
            }
            println!("sl3d: streaming ({r} Hz)");

            // Callbacks run on this thread (inside dev.run's event loop) — the raw shm
            // pointer needs no synchronization beyond the ring's atomics.
            let cap_shm = shm;
            let play_shm = shm;
            // Exit the stream to re-lock the device on shutdown, a CoreAudio rate
            // change (the plugin publishes its chosen rate into shm.sample_rate), or
            // when IO has been idle long enough to suspend.
            let stream_stop = AtomicBool::new(false);
            let ss = &stream_stop;
            // Suspend after ~1 s with no client IO, debouncing apps that briefly
            // stop/restart IO (a short StopIO/StartIO gap keeps the stream alive).
            let idle_stop = r as u64;
            let mut idle_frames: u64 = 0;
            // Playback ring buffering: coreaudiod writes output in bursts while the
            // device drains it smoothly, so we hold a latency cushion to absorb the
            // jitter (else the ring bottoms out and underruns crackle).
            let mut primed = false;
            const CUSHION: u64 = 1024; // ~21 ms target buffer @ 48 kHz
            const MAXLAT: u64 = 6144; // ~128 ms; drop old audio beyond this
            let res = dev.run(
                &stream_stop,
                r,
                move |planar: &[Vec<i32>], frames: usize| unsafe {
                    let w = (*cap_shm).cap_write.load(Ordering::Relaxed);
                    for i in 0..frames {
                        let base = ((w + i as u64) & shm::RING_MASK) as usize * CHANNELS;
                        for c in 0..CHANNELS { (*cap_shm).cap[base + c] = shm::i24_to_f32(planar[c][i]); }
                    }
                    let now = mach_absolute_time();
                    (*cap_shm).cap_write.store(w + frames as u64, Ordering::Release);
                    (*cap_shm).clock_host.store(now, Ordering::Release); // device-clock anchor
                    (*cap_shm).daemon_heartbeat.fetch_add(1, Ordering::Relaxed);
                    let want = (*cap_shm).sample_rate.load(Ordering::Relaxed);
                    if (*cap_shm).io_running.load(Ordering::Relaxed) == 0 {
                        idle_frames += frames as u64;
                    } else {
                        idle_frames = 0;
                    }
                    if STOP.load(Ordering::Relaxed)
                        || idle_frames >= idle_stop
                        || ((want == 44100 || want == 48000) && want != r) {
                        ss.store(true, Ordering::Relaxed);
                    }
                },
                move |planar: &mut [Vec<i32>], frames: usize| unsafe {
                    let mut rd = (*play_shm).play_read.load(Ordering::Relaxed);
                    let wr = (*play_shm).play_write.load(Ordering::Acquire);
                    let mut avail = wr.wrapping_sub(rd);
                    // Overflow (producer ran ahead / stalled): drop old audio to the cushion.
                    if avail > MAXLAT {
                        rd = wr - CUSHION;
                        avail = CUSHION;
                        (*play_shm).play_read.store(rd, Ordering::Relaxed);
                    }
                    // Prime: output silence until a cushion has accumulated.
                    if !primed {
                        if avail < CUSHION {
                            for c in 0..CHANNELS { for i in 0..frames { planar[c][i] = 0; } }
                            return;
                        }
                        primed = true;
                    }
                    for i in 0..frames {
                        if (i as u64) < avail {
                            let base = ((rd + i as u64) & shm::RING_MASK) as usize * CHANNELS;
                            for c in 0..CHANNELS { planar[c][i] = shm::f32_to_i24((*play_shm).play[base + c]); }
                        } else {
                            for c in 0..CHANNELS { planar[c][i] = 0; }
                        }
                    }
                    let consumed = avail.min(frames as u64);
                    (*play_shm).play_read.store(rd + consumed, Ordering::Release);
                    if avail < frames as u64 { primed = false; } // underran -> re-prime
                },
            );
            if let Err(e) = res {
                eprintln!("sl3d: audio ended: {e}");
                stream_err = true; // device likely gone -> reopen
                break;
            }
            // Ok: stopped for idle, rate change, or shutdown. A mid-stream detach
            // also lands here (the iso transfers drain to inflight==0 and run()
            // returns Ok, not Err), so verify the device is still attached before
            // re-streaming — otherwise we'd re-arm the stream on a dead handle.
            if !dev.is_present() {
                unsafe { (*shm).device_present.store(0, Ordering::Relaxed); }
                break; // -> reopen loop
            }
            // The inner loop re-checks io_running and either suspends or re-streams
            // at the new rate.
        }

        unsafe { (*shm).device_present.store(0, Ordering::Relaxed); }
        println!("sl3d: device closed");

        // Backoff before reopening so a transient failure can't runaway-churn.
        if stream_err && !STOP.load(Ordering::Relaxed) { std::thread::sleep(Duration::from_millis(500)); }
    }
    println!("sl3d: exiting");
}
