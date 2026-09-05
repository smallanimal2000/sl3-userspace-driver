//! The async USB transport abstraction shared by the native (libusb) and WebUSB
//! backends.
//!
//! Every operation is async because the WebUSB backend is Promise-based and
//! browser code cannot block. The native backend implements the same shape on
//! top of libusb async transfers driven by an event-pump thread (see
//! [`native`](crate::transport::native)). The trait is `?Send` because WebUSB
//! futures wrap `JsValue`, which is not `Send`.

use crate::Result;
use async_trait::async_trait;
use std::time::Duration;

#[cfg(not(target_arch = "wasm32"))]
pub mod native;

#[cfg(target_arch = "wasm32")]
pub mod webusb;

/// One isochronous packet's worth of bytes plus whether the controller reported
/// it completed successfully. Data is the raw wire bytes (tightly-packed 18-byte
/// 6ch/24-bit frames); (de)interleaving happens in [`crate::stream`].
pub struct IsoPacket {
    pub data: Vec<u8>,
    pub ok: bool,
}

/// Low-level, transport-agnostic USB operations the driver needs. One `iso_in` /
/// `iso_out` call corresponds to exactly one USB isochronous transfer (a batch of
/// packets), mapping 1:1 to WebUSB's `isochronousTransfer{In,Out}` and to a single
/// `libusb` iso transfer. Higher-level pipelining (keeping several transfers in
/// flight) lives in [`crate::stream`], so it is identical across backends.
#[async_trait(?Send)]
pub trait UsbTransport {
    /// Claim a USB interface for exclusive access.
    async fn claim_interface(&self, iface: u8) -> Result<()>;

    /// Select an alternate setting on an interface (stream start/stop).
    async fn set_alt(&self, iface: u8, alt: u8) -> Result<()>;

    /// Write one interrupt OUT report.
    async fn interrupt_out(&self, ep: u8, data: &[u8]) -> Result<()>;

    /// Read one interrupt IN report of up to `len` bytes.
    async fn interrupt_in(&self, ep: u8, len: usize, timeout: Duration) -> Result<Vec<u8>>;

    /// Submit one isochronous IN transfer of `packet_lengths.len()` packets, each
    /// requesting the given number of bytes. Returns per-packet results.
    async fn iso_in(&self, ep: u8, packet_lengths: &[u32]) -> Result<Vec<IsoPacket>>;

    /// Submit one isochronous OUT transfer. `data` is the concatenation of all
    /// packets; `packet_lengths` gives each packet's byte length (variable, for
    /// fractional rate pacing).
    async fn iso_out(&self, ep: u8, data: &[u8], packet_lengths: &[u32]) -> Result<()>;
}
