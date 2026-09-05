//! sl3-ctl — command-line control (Rust port of cli/sl3-ctl.cpp).
//!
//!   sl3-ctl status | rate <hz> | get <off> | set <off> <val> | output on|off | watch
//!
//! The driver API is async (shared with the WebUSB backend); this CLI drives it
//! with a minimal `block_on`.
use sl3::{Sl3, AUDIO_CTRL_BYTES};
use std::time::Duration;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("usage: {} status|rate|get|set|output|watch ...", args[0]);
        std::process::exit(2);
    }

    let mut d = match Sl3::open_control() {
        Ok(d) => d,
        Err(e) => { eprintln!("open: {e}"); std::process::exit(1); }
    };

    if let Err(e) = pollster::block_on(run(&mut d, &args)) {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

async fn run(d: &mut Sl3, args: &[String]) -> sl3::Result<()> {
    match args[1].as_str() {
        "status" => status(d).await,
        "rate" if args.len() == 3 => {
            let hz: u32 = args[2].parse().unwrap_or(0);
            d.set_sample_rate(hz).await.map(|_| println!("sample rate set to {hz} Hz"))
        }
        "get" if args.len() == 3 => {
            let off: usize = args[2].parse().unwrap_or(0);
            let mut b = [0u8; 1];
            d.get_audio_controls(off, &mut b).await.map(|_| println!("control[{off}] = 0x{:02x}", b[0]))
        }
        "set" if args.len() == 4 => {
            let off: usize = args[2].parse().unwrap_or(0);
            let val = parse_u8(&args[3]);
            d.set_audio_controls(off, &[val]).await.map(|_| println!("set control[{off}] = 0x{val:02x}"))
        }
        "output" if args.len() == 3 => {
            let on = if args[2] == "on" { 1u8 } else { 0u8 };
            d.set_audio_controls(8, &[on]).await.map(|_| println!("aux output {}", if on == 1 { "ON" } else { "off" }))
        }
        "watch" => watch(d).await,
        _ => { eprintln!("unknown/incomplete command"); std::process::exit(2); }
    }
}

fn parse_u8(s: &str) -> u8 {
    if let Some(hex) = s.strip_prefix("0x") { u8::from_str_radix(hex, 16).unwrap_or(0) }
    else { s.parse().unwrap_or(0) }
}

async fn status(d: &mut Sl3) -> sl3::Result<()> {
    let mut ctrls = [0u8; AUDIO_CTRL_BYTES];
    d.get_audio_controls(0, &mut ctrls).await?;
    print!("audio-controls[22]:");
    for b in &ctrls { print!(" {b:02x}"); }
    println!("\n  (offset 8 = aux output = {})", if ctrls[8] != 0 { "ON" } else { "off" });
    let ov = d.overload().await?;
    println!("overload:  {:02x} {:02x} {:02x} {:02x} {:02x} {:02x}", ov[0], ov[1], ov[2], ov[3], ov[4], ov[5]);
    println!("status:    0x{:02x}", d.status_byte().await?);
    println!("rate:      {} Hz", d.sample_rate().await?);
    Ok(())
}

async fn watch(d: &Sl3) -> sl3::Result<()> {
    println!("watching phono/thru/overload events (Ctrl-C to stop)...");
    loop {
        match d.read_event(Duration::from_millis(1000)).await {
            Ok(r) => match r[0] {
                0x38 => println!("phono:    {:02x} {:02x} {:02x}", r[5], r[6], r[7]),
                0x39 => println!("thru:     {:02x} {:02x} {:02x} {:02x}", r[5], r[6], r[7], r[8]),
                0x34 => println!("overload: {:02x} {:02x} {:02x} {:02x} {:02x} {:02x}", r[5], r[6], r[7], r[8], r[9], r[10]),
                c => println!("event 0x{c:02x}: {:02x} {:02x} {:02x} ...", r[5], r[6], r[7]),
            },
            Err(_) => continue, // timeout
        }
    }
}
