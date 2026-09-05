//! sl3-capture — capture N seconds of the SL3's 6 inputs to a 24-bit WAV.
//! Usage: sl3-capture [seconds] [rate] [outfile]
use sl3::{Sl3, CHANNELS};
use std::fs::File;
use std::io::{Seek, SeekFrom, Write};

fn wav_header(rate: u32, ch: u16) -> [u8; 44] {
    let mut h = [0u8; 44];
    h[..4].copy_from_slice(b"RIFF");
    h[8..12].copy_from_slice(b"WAVE");
    h[12..16].copy_from_slice(b"fmt ");
    h[16..20].copy_from_slice(&16u32.to_le_bytes());
    h[20..22].copy_from_slice(&1u16.to_le_bytes()); // PCM
    h[22..24].copy_from_slice(&ch.to_le_bytes());
    h[24..28].copy_from_slice(&rate.to_le_bytes());
    h[28..32].copy_from_slice(&(rate * ch as u32 * 3).to_le_bytes()); // byte rate
    h[32..34].copy_from_slice(&(ch * 3).to_le_bytes()); // block align
    h[34..36].copy_from_slice(&24u16.to_le_bytes());
    h[36..40].copy_from_slice(b"data");
    h
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let seconds: f64 = a.get(1).and_then(|s| s.parse().ok()).unwrap_or(5.0);
    let rate: u32 = a.get(2).and_then(|s| s.parse().ok()).unwrap_or(48000);
    let out = a.get(3).map(|s| s.as_str()).unwrap_or("sl3-capture.wav");

    let mut d = Sl3::open().unwrap_or_else(|e| { eprintln!("open: {e}"); std::process::exit(1); });
    pollster::block_on(async {
        if let Err(e) = d.set_sample_rate(rate).await { eprintln!("warning: set rate: {e}"); }
        println!("device rate: {} Hz", d.sample_rate().await.unwrap_or(rate));
    });

    let mut file = File::create(out).expect("create wav");
    file.write_all(&wav_header(rate, CHANNELS as u16)).unwrap();

    let target = (seconds * rate as f64) as u64;
    let mut written: u64 = 0;
    let mut data_bytes: u32 = 0;
    println!("capturing {seconds:.1}s ({target} frames) of {CHANNELS}ch/24-bit -> {out}");

    pollster::block_on(sl3::stream::capture(&d, |planar, frames| {
        let mut buf = Vec::with_capacity(frames * CHANNELS * 3);
        for i in 0..frames {
            for c in 0..CHANNELS {
                let s = planar[c][i];
                buf.extend_from_slice(&[(s & 0xff) as u8, ((s >> 8) & 0xff) as u8, ((s >> 16) & 0xff) as u8]);
            }
        }
        file.write_all(&buf).unwrap();
        data_bytes += (frames * CHANNELS * 3) as u32;
        written += frames as u64;
        written >= target
    })).unwrap_or_else(|e| { eprintln!("capture: {e}"); });

    // patch sizes
    file.seek(SeekFrom::Start(4)).unwrap();
    file.write_all(&(36 + data_bytes).to_le_bytes()).unwrap();
    file.seek(SeekFrom::Start(40)).unwrap();
    file.write_all(&data_bytes.to_le_bytes()).unwrap();
    println!("wrote {written} frames ({:.2}s)", written as f64 / rate as f64);
}
