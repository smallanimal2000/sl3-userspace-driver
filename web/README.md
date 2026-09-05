# SL 3 — WebUSB / WASM

Runs the Rust `sl3` driver in the browser over **WebUSB** — no native install. The
same driver core (`driver/sl3`) is compiled to WebAssembly; USB I/O goes through
the async `UsbTransport` trait, implemented here by the `web-sys` WebUSB backend.

## Architecture: everything runs in a Web Worker

All USB work (control, capture, playback) runs in a dedicated **Web Worker**
(`worker.js`); `main.js` is a thin UI client that only grants device permission and
relays button clicks / meter updates over `postMessage`. This is not cosmetic:
main-thread event-loop jank stalls iso-OUT resubmission by up to ~48 ms, starving the
endpoint and clicking. In a Worker the callback gap stays a few ms, and combined with
**full-duplex implicit feedback** (`startPlaybackFd`, mirroring the capture endpoint's
per-packet frame counts onto OUT so playback tracks the device clock), output is
glitch-free. WebUSB device *selection* (`requestDevice`) is main-thread-only, so the
main thread grants once and the Worker opens the same device via `getDevices()`.

## Requirements / caveats

- **Chromium-family browser only** (Chrome, Edge, Opera). Firefox/Safari/iOS do
  not implement WebUSB.
- Must be served over a **secure context**: `https://` or `http://localhost`.
- Device selection requires a **user gesture** — hence the Connect button.
- The SL 3 must **not be claimed by another driver**. On macOS, stop the `sl3d`
  LaunchDaemon first (`sudo launchctl bootout system/com.rane.sl3d`), or the
  browser's `open()`/`claimInterface()` will fail.
- Isochronous audio over WebUSB is supported by Chromium but is bandwidth- and
  timing-sensitive; sustained 6ch/24-bit @ 48 kHz is best-effort.

## Run (Vite — recommended)

A plain static server (or `file://`) trips ES-module / `.wasm` CORS errors. Vite
serves everything over one `localhost` origin (a secure context, which WebUSB
requires) with correct MIME types, and rebuilds the wasm for you:

```sh
cd web
npm install
npm run dev        # runs wasm-pack, then starts Vite and opens the browser
```

`npm run dev` runs `wasm-pack build` first (the `wasm` script), so you need
`wasm-pack` on PATH. It opens `http://localhost:5173`. Click **Connect**, pick the
SL 3, then use the control buttons (status, rate, overload, aux output) and
**Start capture** to watch live per-channel levels.

`npm run build` produces a static bundle in `web/dist/` (also runs wasm-pack first).

### Just the wasm, no Vite

The WebUSB bindings in `web-sys` are gated behind an unstable cfg, preset for the
wasm target in `driver/.cargo/config.toml` (`--cfg=web_sys_unstable_apis`), so:

```sh
# from the repo root — writes JS glue + .wasm to web/pkg/
wasm-pack build driver/sl3 --target web --out-dir ../../web/pkg
```

`main.js` imports `./pkg/sl3.js` and the `.wasm` via `./pkg/sl3_bg.wasm?url` (Vite
resolves the asset URL). If you serve `web/` with your own tooling instead of Vite,
make sure it serves `.wasm` as `application/wasm` over http(s), not `file://`.

## JS API (`Sl3Device`)

```js
import init, { Sl3Device } from './pkg/sl3.js';
await init();
const dev = await Sl3Device.open();      // main thread: prompts picker, opens, claims
// In a Worker (no picker available) open an already-granted device instead:
// const dev = await Sl3Device.openGranted();

await dev.setSampleRate(48000);
await dev.sampleRate();                   // -> number
await dev.getStatus();                    // -> number (status byte)
await dev.getOverload();                  // -> Uint8Array(6)
await dev.getControls(0, 22);             // -> Uint8Array
await dev.setControls(8, new Uint8Array([1]));  // aux output on

dev.startCapture((samples /*Int32Array, interleaved 6ch*/, frames) => {
  // ... return truthy to stop
  return false;
});
dev.startPlayback((frames) => {
  // return an Int32Array of frames*6 interleaved samples, or null to stop
  return buf;
});
// Preferred for clean audio: full-duplex playback with implicit feedback (locks OUT
// to the device clock instead of a drifting nominal rate). Same callback contract.
dev.startPlaybackFd((frames) => { return buf; });
// Simultaneous capture + playback (the SL 3 has one iso IN endpoint, so this is the
// only way to do both at once — capture and playback cannot be separate streams):
dev.startDuplex(
  (samples, frames) => { /* capture: Int32Array, return truthy to stop */ return false; },
  (frames) => { /* playback: return Int32Array of frames*6, or null to stop */ return buf; },
);
dev.stop();
```

### Float32 variants (Web-Audio-native)

Every streaming endpoint has a `…F32` twin that uses `Float32Array` in `[-1, 1)` instead
of sign-extended 24-bit `Int32Array` — the driver converts internally, so you can wire
straight to Web Audio without scaling:

```js
dev.startCaptureF32((samples /*Float32Array*/, frames) => { /* … */ return false; });
dev.startPlaybackF32((frames) => { /* return Float32Array(frames*6) */ return buf; });
dev.startDuplexF32(
  (samples, frames) => { /* Float32Array capture */ return false; },
  (frames) => { /* return Float32Array(frames*6) */ return buf; },
);
```
