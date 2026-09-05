//! Transport-agnostic device facade.
//!
//! [`Device<T>`] holds a [`UsbTransport`] plus the small amount of driver state
//! that is independent of the backend (the HID sequence counter and the cached
//! sample rate) and implements the control operations by combining the pure
//! [`crate::proto`] codec with async transport calls. Backend-specific
//! constructors live with each transport (see [`crate::transport::native`] and
//! [`crate::wasm`]).

use crate::proto;
use crate::transport::UsbTransport;
use crate::{Error, Result, AUDIO_CTRL_BYTES, EP_HID_IN, EP_HID_OUT, HID_REPORT_LEN};
use std::time::Duration;

/// Default per-command timeout, matching the old blocking driver (1 s).
const CMD_TIMEOUT: Duration = Duration::from_millis(1000);

/// An open SL 3 over transport `T`. Construct via a backend constructor
/// (`Sl3::open*` on native, `Sl3Device.open()` in the browser).
pub struct Device<T> {
    transport: T,
    has_audio: bool,
    has_control: bool,
    hid_seq: u32,
    cached_rate: u32,
}

impl<T: UsbTransport> Device<T> {
    /// Wrap an already-opened, interface-claimed transport. `has_audio`/
    /// `has_control` record which interface roles this handle owns.
    pub(crate) fn new(transport: T, has_audio: bool, has_control: bool) -> Self {
        Device { transport, has_audio, has_control, hid_seq: 0, cached_rate: 0 }
    }

    pub fn vendor_id(&self) -> u16 { crate::VID }
    pub fn product_id(&self) -> u16 { crate::PID }
    pub fn has_audio(&self) -> bool { self.has_audio }

    /// Access the underlying transport (used by [`crate::stream`]).
    pub(crate) fn transport(&self) -> &T { &self.transport }

    // ---- HID transport ----

    /// Send one OUT report `[code][seq:LE32][payload…]` (padded to 64).
    pub async fn hid_send(&mut self, code: u8, payload: &[u8]) -> Result<()> {
        if !self.has_control { return Err(Error::State("handle has no control interface")); }
        self.hid_seq = self.hid_seq.wrapping_add(1);
        let req = proto::build_report(code, self.hid_seq, payload)?;
        self.transport.interrupt_out(EP_HID_OUT, &req).await
    }

    /// Read one 64-byte IN report (a command reply or an unsolicited event).
    pub async fn read_event(&self, timeout: Duration) -> Result<[u8; HID_REPORT_LEN]> {
        if !self.has_control { return Err(Error::State("handle has no control interface")); }
        let v = self.transport.interrupt_in(EP_HID_IN, HID_REPORT_LEN, timeout).await?;
        if v.is_empty() { return Err(Error::State("empty hid read")); }
        let mut buf = [0u8; HID_REPORT_LEN];
        let n = v.len().min(HID_REPORT_LEN);
        buf[..n].copy_from_slice(&v[..n]);
        Ok(buf)
    }

    /// Full command: send OUT, drain the one reply (required or the next OUT stalls).
    async fn hid_cmd(&mut self, code: u8, payload: &[u8]) -> Result<[u8; HID_REPORT_LEN]> {
        self.hid_send(code, payload).await?;
        self.read_event(CMD_TIMEOUT).await
    }

    // ---- control operations ----

    pub async fn set_sample_rate(&mut self, hz: u32) -> Result<()> {
        if !self.has_control { return Err(Error::State("rate needs the control interface")); }
        let be = proto::encode_rate(hz)?; // big-endian on the wire
        self.hid_cmd(proto::CMD_SET_RATE, &be).await?;
        self.cached_rate = hz;
        Ok(())
    }

    pub async fn sample_rate(&mut self) -> Result<u32> {
        let r = self.hid_cmd(proto::CMD_GET_PARAM, &[]).await?;
        Ok(proto::decode_rate(&r))
    }

    /// Read a range of the 22-byte audio-controls register file (HID cmd 0x32).
    pub async fn get_audio_controls(&mut self, offset: usize, dst: &mut [u8]) -> Result<()> {
        if offset + dst.len() > AUDIO_CTRL_BYTES {
            return Err(Error::Param("audio-controls range".into()));
        }
        let r = self.hid_cmd(proto::CMD_GET_CONTROLS, &[]).await?;
        proto::decode_controls(&r, offset, dst)
    }

    /// Write a range of the audio-controls register file (HID cmd 0x33).
    pub async fn set_audio_controls(&mut self, offset: usize, data: &[u8]) -> Result<()> {
        let pl = proto::encode_set_controls(offset, data)?;
        self.hid_cmd(proto::CMD_SET_CONTROLS, &pl).await?;
        Ok(())
    }

    pub async fn overload(&mut self) -> Result<[u8; 6]> {
        let r = self.hid_cmd(proto::CMD_GET_OVERLOAD, &[]).await?;
        Ok(proto::decode_overload(&r))
    }

    pub async fn status_byte(&mut self) -> Result<u8> {
        Ok(proto::decode_status(&self.hid_cmd(proto::CMD_GET_STATUS, &[]).await?))
    }

    /// Set the playback pacing rate on an audio handle without a device command.
    pub fn set_pacing_rate(&mut self, hz: u32) { self.cached_rate = hz; }

    /// The rate the audio path should pace to (0 = unset -> stream defaults to 48k).
    pub(crate) fn cached_rate(&self) -> u32 { self.cached_rate }
}
