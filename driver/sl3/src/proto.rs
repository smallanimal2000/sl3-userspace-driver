//! Pure, transport-free codec for the SL 3 HID control protocol.
//!
//! Every device command is a 64-byte HID interrupt-OUT report shaped
//! `[code:1][seq:LE32][payload…]` zero-padded to 64, with a 64-byte reply on the
//! IN endpoint echoing the seq (see `docs/protocol.md`). This module has no I/O:
//! it only builds request reports and reads fields out of reply reports, so it is
//! shared verbatim by the native (libusb) and WebUSB backends and unit-testable.

use crate::{Error, Result, AUDIO_CTRL_BYTES, HID_REPORT_LEN};

// HID command codes (host->device OUT report byte 0).
pub const CMD_GET_STATUS: u8 = 0x0a;
pub const CMD_GET_PARAM: u8 = 0x30;
pub const CMD_SET_RATE: u8 = 0x31;
pub const CMD_GET_CONTROLS: u8 = 0x32;
pub const CMD_SET_CONTROLS: u8 = 0x33;
pub const CMD_GET_OVERLOAD: u8 = 0x35;

/// Offset of the payload within a report (`[code:1][seq:LE32]` = 5 bytes).
pub const PAYLOAD_OFF: usize = 5;

/// Unsolicited event report codes (device->host, not command replies).
pub const EVT_OVERLOAD: u8 = 0x34;
pub const EVT_PHONO: u8 = 0x38;
pub const EVT_THRU: u8 = 0x39;

/// True if `code` is an unsolicited event report rather than a command reply.
pub fn is_event(code: u8) -> bool {
    matches!(code, EVT_OVERLOAD | EVT_PHONO | EVT_THRU)
}

/// Build one OUT report `[code][seq:LE32][payload…]` zero-padded to 64 bytes.
pub fn build_report(code: u8, seq: u32, payload: &[u8]) -> Result<[u8; HID_REPORT_LEN]> {
    if payload.len() > HID_REPORT_LEN - PAYLOAD_OFF {
        return Err(Error::Param(format!("hid payload too long ({})", payload.len())));
    }
    let mut req = [0u8; HID_REPORT_LEN];
    req[0] = code;
    req[1..5].copy_from_slice(&seq.to_le_bytes());
    req[PAYLOAD_OFF..PAYLOAD_OFF + payload.len()].copy_from_slice(payload);
    Ok(req)
}

/// Encode a sample rate as the 2-byte **big-endian** payload for `CMD_SET_RATE`.
/// Only 44100/48000 are accepted (the shipping firmware emits only these).
pub fn encode_rate(hz: u32) -> Result<[u8; 2]> {
    if hz != 44100 && hz != 48000 {
        return Err(Error::Param(format!("unsupported sample rate {hz}")));
    }
    Ok([(hz >> 8) as u8, (hz & 0xff) as u8])
}

/// Read the sample rate back out of a `CMD_GET_PARAM` reply (big-endian at payload).
pub fn decode_rate(reply: &[u8; HID_REPORT_LEN]) -> u32 {
    ((reply[PAYLOAD_OFF] as u32) << 8) | reply[PAYLOAD_OFF + 1] as u32
}

/// Build the `CMD_SET_CONTROLS` payload `[offset][len][data…]`.
pub fn encode_set_controls(offset: usize, data: &[u8]) -> Result<Vec<u8>> {
    if offset + data.len() > AUDIO_CTRL_BYTES {
        return Err(Error::Param("audio-controls range".into()));
    }
    let mut pl = Vec::with_capacity(2 + data.len());
    pl.push(offset as u8);
    pl.push(data.len() as u8);
    pl.extend_from_slice(data);
    Ok(pl)
}

/// Copy a range of the 22-byte audio-controls register file out of a
/// `CMD_GET_CONTROLS` reply into `dst`.
pub fn decode_controls(reply: &[u8; HID_REPORT_LEN], offset: usize, dst: &mut [u8]) -> Result<()> {
    if offset + dst.len() > AUDIO_CTRL_BYTES {
        return Err(Error::Param("audio-controls range".into()));
    }
    dst.copy_from_slice(&reply[PAYLOAD_OFF + offset..PAYLOAD_OFF + offset + dst.len()]);
    Ok(())
}

/// Read the 6-byte overload block out of a `CMD_GET_OVERLOAD` reply.
pub fn decode_overload(reply: &[u8; HID_REPORT_LEN]) -> [u8; 6] {
    let mut o = [0u8; 6];
    o.copy_from_slice(&reply[PAYLOAD_OFF..PAYLOAD_OFF + 6]);
    o
}

/// Read the single status byte out of a `CMD_GET_STATUS` reply.
pub fn decode_status(reply: &[u8; HID_REPORT_LEN]) -> u8 {
    reply[PAYLOAD_OFF]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_framing_roundtrip() {
        let r = build_report(CMD_SET_CONTROLS, 0x0403_0201, &[0xaa, 0xbb]).unwrap();
        assert_eq!(r.len(), HID_REPORT_LEN);
        assert_eq!(r[0], CMD_SET_CONTROLS);
        assert_eq!(&r[1..5], &[0x01, 0x02, 0x03, 0x04]); // seq little-endian
        assert_eq!(&r[5..7], &[0xaa, 0xbb]);
        assert!(r[7..].iter().all(|&b| b == 0)); // zero-padded tail
    }

    #[test]
    fn payload_too_long_rejected() {
        assert!(build_report(CMD_GET_PARAM, 1, &[0u8; HID_REPORT_LEN]).is_err());
    }

    #[test]
    fn rate_encodes_big_endian() {
        assert_eq!(encode_rate(44100).unwrap(), [0xac, 0x44]);
        assert_eq!(encode_rate(48000).unwrap(), [0xbb, 0x80]);
        assert!(encode_rate(96000).is_err());
    }

    #[test]
    fn rate_decode_matches_encode() {
        let mut reply = [0u8; HID_REPORT_LEN];
        reply[PAYLOAD_OFF..PAYLOAD_OFF + 2].copy_from_slice(&encode_rate(48000).unwrap());
        assert_eq!(decode_rate(&reply), 48000);
    }

    #[test]
    fn set_controls_payload_shape() {
        let pl = encode_set_controls(8, &[1]).unwrap();
        assert_eq!(pl, vec![8, 1, 1]);
        assert!(encode_set_controls(AUDIO_CTRL_BYTES, &[0]).is_err()); // out of range
    }

    #[test]
    fn controls_and_overload_slices() {
        let mut reply = [0u8; HID_REPORT_LEN];
        for i in 0..AUDIO_CTRL_BYTES {
            reply[PAYLOAD_OFF + i] = i as u8;
        }
        let mut dst = [0u8; 3];
        decode_controls(&reply, 8, &mut dst).unwrap();
        assert_eq!(dst, [8, 9, 10]);
        assert_eq!(decode_overload(&reply), [0, 1, 2, 3, 4, 5]);
        assert_eq!(decode_status(&reply), 0);
    }
}
