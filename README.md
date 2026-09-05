# Rane SL 3 — userland driver

A userland driver for the **Rane SL 3** USB DJ audio interface (VID `0x1CC5`,
PID `1`), replacing the dead `Sl3Driver.10.9.kext`. The device is vendor-class
but UAC2-shaped: 6ch/24-bit audio over vendor isochronous endpoints, plus a
64-byte HID control channel. See [`docs/protocol.md`](docs/protocol.md) for the
reverse-engineered protocol.

## Architecture

The **driver is Rust** (`driver/`). Only the CoreAudio plugin remains in C,
because it is loaded inside the sandboxed `coreaudiod` (which cannot open USB) and
talks to the daemon purely through a shared-memory ring.

All USB I/O goes through one async `UsbTransport` trait (`driver/sl3/src/transport.rs`)
with two compile-time backends, so the same driver core runs natively **and** in the
browser over WebUSB:

```
  driver/sl3      driver core, transport-agnostic:
                    proto.rs             pure HID protocol codec (no I/O, unit-tested)
                    device.rs            Device<T> control ops (async)
                    stream.rs            generic async iso capture/playback pipeline
                    transport/native.rs  rusb + libusb1-sys FFI + event-pump thread
                    transport/webusb.rs  web-sys WebUSB backend (wasm32)
                    wasm.rs              wasm-bindgen JS class (wasm32)
  driver/sl3d     daemon: owns the device, bridges audio <-> shared memory
  api/            drop-in Sl3Api.framework (cdylib, C ABI) — runs the original prefPane
  coreaudio/      AudioServerPlugin "SL3.driver" (C++) — presents the device to CoreAudio,
                  reads/writes the shared-memory ring (driver/sl3d/include/sl3_shm.h)
  web/            WebUSB browser demo (loads the wasm-pack build of driver/sl3)
```

`sl3d` (audio owner, interfaces 1/2) and the control clients (`Sl3Api` / `sl3-ctl`,
interface 3) claim **disjoint USB interfaces**, so they run simultaneously. The driver
API is async; the native tools/daemon/framework drive it with `block_on`, while the
browser awaits it directly.

## Build

The build system is **Bazel** (bzlmod). `rules_rust` builds the Rust crates
(external deps declared in `MODULE.bazel`, with `rusb`'s `vendored` feature so
libusb is compiled from source — no system libusb needed), and `rules_cc` builds
the CoreAudio plugin.

```sh
bazel build //...                    # everything
bazel build //driver/sl3:sl3-ctl     # a single target
bazel run   //driver/sl3:sl3-probe   # build + run
```

## Command-line tools

```sh
bazel-bin/driver/sl3/sl3-probe                 # dump USB descriptors
bazel-bin/driver/sl3/sl3-ctl status            # controls register, rate, overload
bazel-bin/driver/sl3/sl3-ctl output on|off     # aux output (control offset 8)
bazel-bin/driver/sl3/sl3-ctl rate 44100|48000  # sample rate
bazel-bin/driver/sl3/sl3-ctl watch             # live phono/thru/overload events
bazel-bin/driver/sl3/sl3-capture 5 48000 out.wav   # capture 6ch/24-bit to WAV
bazel-bin/driver/sl3/sl3-tone 3 48000 440          # play a sine to deck 1 out
```

## Browser (WebUSB / WASM)

The driver core also compiles to WebAssembly and drives the SL 3 in a
Chromium-family browser over WebUSB — control **and** iso audio, no native install.
Built with cargo + `wasm-pack` (not Bazel):

```sh
wasm-pack build driver/sl3 --target web --out-dir ../../web/pkg
cd web && python3 -m http.server 8000   # then open http://localhost:8000 in Chrome
```

Click **Connect**, pick the SL 3, and drive control ops + live capture levels. The
WebUSB unstable-API cfg is preset in `driver/.cargo/config.toml`. Caveats
(Chromium-only, user gesture, device must be free of other drivers) and the
`Sl3Device` JS API are in [`web/README.md`](web/README.md).

## Install (needs sudo)

```sh
# CoreAudio device: Rust daemon (LaunchDaemon) + C plugin, restarts coreaudiod
sudo ./packaging/install.sh

# Drop-in Sl3Api.framework so the original control panel drives this driver
sudo ./packaging/install-prefpane.sh
open "/Library/PreferencePanes/SL 3 Audio Control Panel.prefPane"
```

## Status

Verified on hardware: descriptor probe, HID control (phono/output/rate/status),
full-duplex 24-bit isochronous audio, the daemon↔plugin shared-memory bridge, and
the `Sl3Api` drop-in (prefPane-mimic smoke test). The CoreAudio install and the
prefPane GUI on macOS 26 depend on OS-level legacy-prefPane / library-validation
behavior (see the install scripts). Firmware update is intentionally stubbed.
