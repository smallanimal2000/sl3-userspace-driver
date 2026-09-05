//! natone — play a sine to deck-1 out via the native_audio full-duplex path
//! (the daemon's streaming path). Verifies playback independently of CoreAudio.
//! Usage: natone [seconds] [rate] [freqHz]
use sl3::native_audio::AudioDevice;
use sl3::CHANNELS;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let secs: f64 = a.get(1).and_then(|s| s.parse().ok()).unwrap_or(3.0);
    let rate: u32 = a.get(2).and_then(|s| s.parse().ok()).unwrap_or(48000);
    let freq: f64 = a.get(3).and_then(|s| s.parse().ok()).unwrap_or(440.0);

    let dev = AudioDevice::open().unwrap_or_else(|e| { eprintln!("open: {e}"); std::process::exit(1); });
    let stop = AtomicBool::new(false);
    let step = 2.0 * std::f64::consts::PI * freq / rate as f64;
    let amp = 0.2 * (1i32 << 23) as f64;
    let mut phase = 0.0f64;
    let start = Instant::now();
    println!("playing {freq:.0} Hz to deck 1 out for {secs:.1}s @ {rate} Hz (native_audio path)");

    let r = dev.run(
        &stop,
        rate,
        |_planar: &[Vec<i32>], _frames: usize| {}, // capture ignored
        |planar: &mut [Vec<i32>], frames: usize| {
            for i in 0..frames {
                let s = (phase.sin() * amp) as i32;
                phase += step;
                if phase > 2.0 * std::f64::consts::PI { phase -= 2.0 * std::f64::consts::PI; }
                planar[0][i] = s;
                planar[1][i] = s;
                for c in 2..CHANNELS { planar[c][i] = 0; }
            }
            if start.elapsed().as_secs_f64() >= secs { stop.store(true, Ordering::Relaxed); }
        },
    );
    match r { Ok(_) => println!("done."), Err(e) => eprintln!("error: {e}") }
}
