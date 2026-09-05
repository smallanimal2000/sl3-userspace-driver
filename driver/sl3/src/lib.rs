//! Userland driver for the Rane SL 3 USB DJ audio interface.
//!
//! The driver is transport-agnostic: all USB I/O goes through the async
//! [`UsbTransport`] trait, with two backends selected at compile time —
//!
//! * **native** ([`transport::native`]): `rusb` + raw `libusb1-sys` iso transfers,
//!   driven by a libusb event-pump thread. This is the default on every non-wasm
//!   target and is what the CLIs, the `sl3d` daemon, and the `Sl3Api` framework use.
//! * **WebUSB** ([`transport::webusb`], `wasm32` only): `web-sys` bindings that
//!   await browser Promises, exposed to JavaScript via [`wasm`].
//!
//! The device is vendor-class but UAC2-shaped. Its real control channel is a
//! 64-byte HID report pair on interface 3 (`[code][seq:LE32][payload…]`, see
//! [`proto`]), and audio is 6ch/24-bit LE over vendor iso endpoints (see
//! [`stream`]). Protocol details in `docs/protocol.md`.

pub mod device;
pub mod proto;
pub mod stream;
pub mod transport;

#[cfg(target_arch = "wasm32")]
pub mod wasm;

// Dedicated stable full-duplex iso streaming for the native daemon (not wasm).
#[cfg(not(target_arch = "wasm32"))]
pub mod native_audio;

pub use device::Device;
pub use transport::{IsoPacket, UsbTransport};

/// The native (libusb-backed) SL 3 handle. `Sl3::open*` constructors live in
/// [`transport::native`]'s inherent impl on this alias.
#[cfg(not(target_arch = "wasm32"))]
pub type Sl3 = Device<transport::native::NativeTransport>;

// ---- device identity / topology ----
pub const VID: u16 = 0x1CC5;
pub const PID: u16 = 0x0001;
pub const CHANNELS: usize = 6; // 3 stereo decks
pub const BYTES_PER_SAMP: usize = 3; // 24-bit
pub const FRAME_BYTES: usize = CHANNELS * BYTES_PER_SAMP; // 18

pub const IF_PLAYBACK: u8 = 1;
pub const IF_CAPTURE: u8 = 2;
pub const IF_HID: u8 = 3;
pub const EP_PLAYBACK: u8 = 0x06;
pub const EP_CAPTURE: u8 = 0x82;
pub const EP_HID_IN: u8 = 0x81;
pub const EP_HID_OUT: u8 = 0x01;
pub const HID_REPORT_LEN: usize = 64;
pub const AUDIO_CTRL_BYTES: usize = 22;

/// Driver-wide error type. The `rusb`-bearing variants only exist on native
/// targets; the WebUSB backend reports through [`Error::Transport`].
#[derive(Debug)]
pub enum Error {
    NoDevice,
    #[cfg(not(target_arch = "wasm32"))]
    Access(rusb::Error),
    #[cfg(not(target_arch = "wasm32"))]
    Io(rusb::Error),
    /// Backend-agnostic transport failure (used by the WebUSB backend and for
    /// libusb error codes surfaced from raw iso transfers).
    Transport(String),
    Param(String),
    State(&'static str),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::NoDevice => write!(f, "SL3 ({:04x}:{:04x}) not found", VID, PID),
            #[cfg(not(target_arch = "wasm32"))]
            Error::Access(e) => write!(f, "claim/access: {e}"),
            #[cfg(not(target_arch = "wasm32"))]
            Error::Io(e) => write!(f, "usb io: {e}"),
            Error::Transport(s) => write!(f, "usb transport: {s}"),
            Error::Param(s) => write!(f, "bad parameter: {s}"),
            Error::State(s) => write!(f, "invalid state: {s}"),
        }
    }
}
impl std::error::Error for Error {}
pub type Result<T> = std::result::Result<T, Error>;
