//! sl3-tone — play a test sine to deck 1 outputs (channels 1 & 2).
//! Usage: sl3-tone [seconds] [rate] [freqHz]
use sl3::{Sl3, CHANNELS};

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let seconds: f64 = a.get(1).and_then(|s| s.parse().ok()).unwrap_or(3.0);
    let rate: u32 = a.get(2).and_then(|s| s.parse().ok()).unwrap_or(48000);
    let freq: f64 = a.get(3).and_then(|s| s.parse().ok()).unwrap_or(1000.0);

    let mut d = Sl3::open().unwrap_or_else(|e| { eprintln!("open: {e}"); std::process::exit(1); });
    pollster::block_on(async {
        if let Err(e) = d.set_sample_rate(rate).await { eprintln!("warning: set rate: {e}"); }
    });

    let step = 2.0 * std::f64::consts::PI * freq / rate as f64;
    let amp = 0.2 * (1i32 << 23) as f64;
    let mut phase = 0.0f64;
    let mut left = (seconds * rate as f64) as i64;
    println!("playing {freq:.0} Hz sine to deck 1 out for {seconds:.1}s @ {rate} Hz");

    pollster::block_on(sl3::stream::playback(&d, |planar, frames| {
        for i in 0..frames {
            let s = (phase.sin() * amp) as i32;
            phase += step;
            if phase > 2.0 * std::f64::consts::PI { phase -= 2.0 * std::f64::consts::PI; }
            planar[0][i] = s;
            planar[1][i] = s;
            for c in 2..CHANNELS { planar[c][i] = 0; }
        }
        left -= frames as i64;
        left <= 0
    })).unwrap_or_else(|e| { eprintln!("playback: {e}"); });

    println!("done.");
}
