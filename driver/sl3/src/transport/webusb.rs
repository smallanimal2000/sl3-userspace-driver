//! WebUSB backend: implements [`UsbTransport`] on top of `web-sys` bindings by
//! awaiting the browser's `USBDevice` Promises. Chromium-family browsers only.
//!
//! WebUSB endpoint methods take the endpoint *number* (low nibble), not the full
//! address with the direction bit, and it has no separate interrupt method —
//! `transferIn`/`transferOut` cover interrupt and bulk. Isochronous maps 1:1 to
//! `isochronousTransfer{In,Out}`.

use crate::transport::{IsoPacket, UsbTransport};
use crate::{Error, Result};
use async_trait::async_trait;
use std::time::Duration;
use wasm_bindgen::JsValue;
use wasm_bindgen_futures::JsFuture;

/// A transport bound to one browser `USBDevice` handle (already opened, its
/// interfaces claimed). Cloneable — cloning the handle yields another reference
/// to the same underlying device, so a control handle and an audio handle can
/// coexist (mirroring the native disjoint-interface design).
pub struct WebUsbTransport {
    device: web_sys::UsbDevice,
}

impl WebUsbTransport {
    pub fn new(device: web_sys::UsbDevice) -> Self {
        Self { device }
    }
}

fn jserr(ctx: &str, e: JsValue) -> Error {
    Error::Transport(format!("{ctx}: {e:?}"))
}

/// WebUSB endpoint methods want the endpoint number, not the addressed endpoint.
fn ep_num(ep: u8) -> u8 {
    ep & 0x0f
}

/// Copy a `DataView`'s bytes into an owned `Vec<u8>`.
fn dataview_to_vec(dv: &js_sys::DataView) -> Vec<u8> {
    let buffer = dv.buffer();
    let arr = js_sys::Uint8Array::new_with_byte_offset_and_length(
        buffer.as_ref(),
        dv.byte_offset() as u32,
        dv.byte_length() as u32,
    );
    arr.to_vec()
}

/// WebUSB's iso methods take `&[js_sys::Number]` for the per-packet lengths.
fn lengths(packet_lengths: &[u32]) -> Vec<js_sys::Number> {
    packet_lengths.iter().map(|&l| js_sys::Number::from(l as f64)).collect()
}

#[async_trait(?Send)]
impl UsbTransport for WebUsbTransport {
    async fn claim_interface(&self, iface: u8) -> Result<()> {
        JsFuture::from(self.device.claim_interface(iface))
            .await
            .map_err(|e| jserr("claim_interface", e))?;
        Ok(())
    }

    async fn set_alt(&self, iface: u8, alt: u8) -> Result<()> {
        JsFuture::from(self.device.select_alternate_interface(iface, alt))
            .await
            .map_err(|e| jserr("select_alternate_interface", e))?;
        Ok(())
    }

    async fn interrupt_out(&self, ep: u8, data: &[u8]) -> Result<()> {
        let mut buf = data.to_vec();
        // The `_with_u8_slice` variant can throw synchronously (shared-memory), so
        // it returns a Result before the Promise.
        let promise = self
            .device
            .transfer_out_with_u8_slice(ep_num(ep), &mut buf)
            .map_err(|e| jserr("transfer_out", e))?;
        JsFuture::from(promise).await.map_err(|e| jserr("transfer_out", e))?;
        Ok(())
    }

    async fn interrupt_in(&self, ep: u8, len: usize, _timeout: Duration) -> Result<Vec<u8>> {
        // WebUSB has no per-transfer timeout; the browser resolves when data arrives.
        let res = JsFuture::from(self.device.transfer_in(ep_num(ep), len as u32))
            .await
            .map_err(|e| jserr("transfer_in", e))?;
        Ok(res.data().map(|dv| dataview_to_vec(&dv)).unwrap_or_default())
    }

    async fn iso_in(&self, ep: u8, packet_lengths: &[u32]) -> Result<Vec<IsoPacket>> {
        let lens = lengths(packet_lengths);
        let res = JsFuture::from(self.device.isochronous_transfer_in(ep_num(ep), &lens))
            .await
            .map_err(|e| jserr("isochronous_transfer_in", e))?;
        let packets = res.packets();
        let mut out = Vec::with_capacity(packets.length() as usize);
        for i in 0..packets.length() {
            let pkt = packets.get(i);
            let ok = pkt.status() == web_sys::UsbTransferStatus::Ok;
            let data = pkt.data().map(|dv| dataview_to_vec(&dv)).unwrap_or_default();
            out.push(IsoPacket { data, ok });
        }
        Ok(out)
    }

    async fn iso_out(&self, ep: u8, data: &[u8], packet_lengths: &[u32]) -> Result<()> {
        let lens = lengths(packet_lengths);
        let mut buf = data.to_vec();
        let promise = self
            .device
            .isochronous_transfer_out_with_u8_slice(ep_num(ep), &mut buf, &lens)
            .map_err(|e| jserr("isochronous_transfer_out", e))?;
        JsFuture::from(promise).await.map_err(|e| jserr("isochronous_transfer_out", e))?;
        Ok(())
    }
}
