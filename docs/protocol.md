# Rane SL 3 — USB Protocol Notes

Ground-truth reverse-engineering notes. Sources: live descriptor dump (`sl3-probe`),
the shipped kext (`/Library/Extensions/Sl3Driver.10.9.kext`), and community reports.

## Identity
- VID/PID: **`0x1CC5` / `0x0001`** (7365 / 1). Manufacturer "Rane Corporation", product "SL 3",
  serial "SL3.01.00".
- USB 2.0 **high speed** (480 Mbps). Device class **`0xFF/0xFF/0xFF`** (vendor-specific).
- 1 configuration, **4 interfaces**, 400 mA bus-powered.

## Key insight
The device is **not** advertised as USB Audio Class, but its class-specific descriptors are
**verbatim UAC 2.0 structures** (`CS_INTERFACE=0x24`, `CS_ENDPOINT=0x25`, HEADER/CLOCK_SOURCE/
INPUT_TERMINAL/OUTPUT_TERMINAL/AS_GENERAL/FORMAT_TYPE_I). Rane hid a UAC2 device under a vendor
class so the OS won't auto-bind its generic UAC driver — but the audio semantics are UAC2. This
is why the kext reused Apple's `convertFromAppleUSBAudioInputStream_NoWrap`, and why SL2/SL3 are
reported to "almost" work on Linux.

**Implication:** sample-rate selection and streaming likely follow UAC2 conventions
(clock SET_CUR + SetInterface alt). To be confirmed against the kext disassembly (M1).

## Interface / endpoint map (config 1)

| IF | Alt | Class/Sub/Proto | Endpoints | Role |
|----|-----|-----------------|-----------|------|
| 0  | 0   | 255/1/32        | none      | AudioControl (topology + clock) |
| 1  | 0   | 255/2/32        | none      | Playback AS — zero-bandwidth idle |
| 1  | 1   | 255/2/32        | EP `0x06` OUT iso async | **Playback** 6ch/24-bit |
| 2  | 0   | 255/2/32        | none      | Capture AS — zero-bandwidth idle |
| 2  | 1   | 255/2/32        | EP `0x82` IN iso implicit-fb | **Capture** 6ch/24-bit |
| 3  | 0   | 255/0/0 (+HID desc) | EP `0x81` IN int, EP `0x01` OUT int (64B) | **Control/status (HID)** |

Iso endpoints: `wMaxPacketSize` = **126 bytes**, `bInterval` = 1 (every microframe, 8/ms).
Frame = 6 ch × 3 bytes = **18 bytes/sample-frame**. 126/18 = 7 frames → headroom for 48 kHz
(6 frames/microframe avg) with async jitter. Exact per-packet framing (header? padding?) TBD in M3.

## Audio topology (AudioControl CS descriptor, IF0)
- **Clock** `id5`: internal **programmable**, frequency control read/write.
- **Playback path**: INPUT_TERMINAL `id1` (USB streaming, 6ch, iChannelNames=5 "CH1 Out"…)
  → OUTPUT_TERMINAL `id2` (Line connector).
- **Capture path**: INPUT_TERMINAL `id3` (Line connector, 6ch, iChannelNames=11 "CH1 In"…)
  → OUTPUT_TERMINAL `id4` (USB streaming, device→host).
- **6 channels each direction = 3 stereo pairs** (the "3" in SL3 — 3 decks/phono pairs).

## Audio format (AS_GENERAL + FORMAT_TYPE_I, IF1/2 alt1)
- Format: **PCM** (`bmFormats` bit0), **FORMAT_TYPE_I**.
- **bSubslotSize = 3 bytes, bBitResolution = 24** → 24-bit LPCM, 3 bytes/sample.
- **bNrChannels = 6**.
- Sample rates (community): **44.1 kHz and 48 kHz**, software-selectable via the programmable clock.

## Control / status channel (IF3, HID)
28-byte HID report descriptor decodes to a **vendor page (0xFF00)** with:
- **64-byte Input report** (device→host) on interrupt EP `0x81`.
- **64-byte Output report** (host→device) on interrupt EP `0x01`.

This is the raw control block. In the kext this maps to `DoAsyncRead`/`ReadHidData`/
`QueueHidDataBlock` (reads) and `SetParameter`/`ProtectedSetParameter` (writes). The 64-byte
report contents (switch/button state, phono-vs-line, sample-rate, needle/status flags) are the
next thing to decode — from the kext (M1) and by observing live reports while flipping switches.

## Kext control surface to reproduce (from symbol table)
`ControlRequest`, `SetParameter`, `GetStatus`, `IsC0Device` ("C0" = a hardware-revision probe),
`Get/SetBufferMilliseconds`, `GetVendorId/ProductId/DriverVersion`, `DoAsyncRead`/`ReadHidData`.

## Empirical results (from the libusb reimplementation)
- **Iso capture WORKS.** IF2/alt1, EP `0x82`, 16 iso pkts/transfer × 8 transfers. Captured 3 s =
  144018 frames ≈ 48006 fps → **48 kHz, and each iso packet is a whole number of 18-byte frames
  with NO per-packet header/status bytes.** 24-bit LE, channel-interleaved, sign-extend confirmed.
- Per-channel levels with a source on deck 1: CH1 −0.5 dBFS, CH2 −21 dBFS, CH3–6 ≈ −43 dBFS
  (open-input noise floor). Confirms **CH1/2 = deck1 L/R, CH3/4 = deck2, CH5/6 = deck3**.
- **Sample-rate control is NOT standard UAC2.** Clock `SET_CUR`/`GET_CUR` (bmReqType 0x21/0xA1,
  clockID5, IF0) both **STALL (LIBUSB_ERROR_PIPE)**. Rate select must be vendor-specific or via the
  HID OUT report. (Default hardware rate is 48 kHz; capture works without setting it.) → needs M1.
- **HID control channel is REQUEST/RESPONSE.** Interrupt IN alone times out; standard HID
  GET_REPORT stalls. But writing a **64-byte OUT report to EP `0x01` then reading EP `0x81`
  returns a 64-byte reply** reliably. `sl3_read_status` now does OUT-then-IN.
  - With an **all-zero** request the reply is a **constant** block (header incl.
    `...02 32 52 af c4 cb 60 00 01 05 60 00 60 00 00 05 60 00 60 00 00 01...` + a fixed
    high-entropy tail) — i.e. a fixed device-info/status block, not live timecode.
  - → The request's **opcode/params bytes** (which select "give me live deck state" vs info) must
    come from M1 (`SetParameter`/`QueueHidDataBlock`/`ProtectedReadHidData`).

## Resolved protocol (M1 kext disassembly — CONFIRMED)
The driver uses **no UAC2 class requests at all** (they stall). Every device command is a 64-byte
HID interrupt-OUT report on EP `0x01`, shaped `[code:1][seq:LE32][payload…]` zero-padded to 64,
with an optional 64-byte reply on EP `0x81` echoing the seq. (kext `DoHidRequest` @0x1d14.)

1. **Sample-rate select** = HID command **`0x31`**, payload = 2-byte rate **BIG-endian**
   (44100→`AC 44`, 48000→`BB 80`). Must be sent **before** streaming. Userclient paramID 0.
   `96000` exists in code but only 44100/48000 are emitted. → **implemented & verified** (capture
   frame-rate tracks the commanded rate). Device has **no rate read-back** (we cache it).
2. **Stream start/stop** = plain **`SET_INTERFACE(IF1→alt1)` / `SET_INTERFACE(IF2→alt1)`**, then run
   iso; stop = alt 0. No vendor "run" request. (kext `InInitHardware`/`OutInitHardware`.)
3. **Phono/thru & mixer/LED params** = **NOT in the kext** — app-defined HID reports through the
   generic `DoAsyncRead` tunnel (`[code][seq][payload]`). Must be mapped from a **bus capture of the
   vendor app** (USBPcap/Wireshark or macOS USB tap). Left stubbed.
4. **`IsC0Device`** = software boot/uninitialized flag (device+0x1e0), **returns 0 on shipping
   hardware**. Not a live descriptor read. Ignorable; true HW-rev would be `bcdDevice`.
5. **Iso framing** = tightly packed **6ch × 3-byte 24-bit LE, 18 B/frame**, variable frames/packet;
   the kext divides each iso packet's byte count by 18 and trusts the iso frame length (no in-band
   header). OUT mirrors IN (`ClipfloatToSInt24LE_4`). → **matches empirical capture.**
6. **64-byte HID reply** is relayed **verbatim** by the kext (`QueueHidDataBlock`); field meanings
   (buttons/switches/deck state) are app-decoded → map empirically from a bus capture.
