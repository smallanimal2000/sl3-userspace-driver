//! sl3-probe — dump the SL 3 USB descriptor set (Rust port of tools/sl3-probe.c).
use rusb::{Context, UsbContext};
use std::time::Duration;

fn xfer_type(a: u8) -> &'static str {
    match a & 0x03 { 0 => "control", 1 => "isochronous", 2 => "bulk", 3 => "interrupt", _ => "?" }
}

fn main() {
    let ctx = Context::new().expect("libusb init");
    let dev = match ctx.devices().unwrap().iter().find(|d| {
        d.device_descriptor().map(|dd| dd.vendor_id() == sl3::VID && dd.product_id() == sl3::PID).unwrap_or(false)
    }) {
        Some(d) => d,
        None => { eprintln!("SL3 {:04x}:{:04x} not found", sl3::VID, sl3::PID); std::process::exit(2); }
    };

    let dd = dev.device_descriptor().unwrap();
    let h = dev.open().expect("open");
    let lang = h.read_languages(Duration::from_millis(200)).ok().and_then(|l| l.into_iter().next());
    let s = |idx| lang.and_then(|l| h.read_string_descriptor(l, idx, Duration::from_millis(200)).ok());

    println!("=== Device {:04x}:{:04x} (bcdUSB {}) ===", dd.vendor_id(), dd.product_id(), dd.usb_version());
    println!("  class/sub/proto = {}/{}/{}  numConfigs = {}",
        dd.class_code(), dd.sub_class_code(), dd.protocol_code(), dd.num_configurations());
    if let Some(m) = s(dd.manufacturer_string_index().unwrap_or(0)) { println!("  manufacturer = {m:?}"); }
    if let Some(p) = s(dd.product_string_index().unwrap_or(0)) { println!("  product      = {p:?}"); }
    if let Some(sn) = s(dd.serial_number_string_index().unwrap_or(0)) { println!("  serial       = {sn:?}"); }
    println!("  speed = {:?}", dev.speed());

    for ci in 0..dd.num_configurations() {
        let cfg = match dev.config_descriptor(ci) { Ok(c) => c, Err(_) => continue };
        println!("\n--- Configuration {}: {} interface(s), {} mA ---",
            cfg.number(), cfg.num_interfaces(), cfg.max_power());
        for itf in cfg.interfaces() {
            for id in itf.descriptors() {
                println!("\n  Interface {} alt {} class/sub/proto={}/{}/{} endpoints={}",
                    id.interface_number(), id.setting_number(),
                    id.class_code(), id.sub_class_code(), id.protocol_code(), id.num_endpoints());
                let extra = id.extra();
                if !extra.is_empty() {
                    print!("    class-specific ({} bytes):", extra.len());
                    for b in extra { print!(" {b:02x}"); }
                    println!();
                }
                for ep in id.endpoint_descriptors() {
                    println!("    EP 0x{:02x} {:<4} {:<11} maxPkt={} bInterval={}",
                        ep.address(),
                        if ep.address() & 0x80 != 0 { "IN" } else { "OUT" },
                        xfer_type(match ep.transfer_type() {
                            rusb::TransferType::Control => 0, rusb::TransferType::Isochronous => 1,
                            rusb::TransferType::Bulk => 2, rusb::TransferType::Interrupt => 3 }),
                        ep.max_packet_size(), ep.interval());
                }
            }
        }
    }
}
