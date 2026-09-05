// The Worker owns the SL 3 entirely — control, capture, AND playback all run off the
// main thread. Main-thread event-loop jank starves the iso-OUT endpoint (~48 ms
// stalls → clicks); a Worker keeps callback gaps to a few ms. The main thread only
// grants device permission (requestDevice, a user gesture); this Worker opens the
// same device via getDevices() and services commands over postMessage.
//
// Uses the Float32 endpoints (Web-Audio-native; the driver converts i24<->f32).
import init, { Sl3Device } from './pkg/sl3.js';
import wasmUrl from './pkg/sl3_bg.wasm?url';

const CHANNELS = 6;
let dev = null;
let ready = false;

const log = (msg) => self.postMessage({ type: 'log', msg });

async function ensureOpen() {
  if (!ready) {
    await init(wasmUrl);
    dev = await Sl3Device.openGranted();
    ready = true;
  }
  return dev;
}

self.onmessage = async (e) => {
  const m = e.data;
  try {
    switch (m.type) {
      case 'open':
        await ensureOpen();
        self.postMessage({ type: 'opened', rate: await dev.sampleRate() });
        break;
      case 'status':
        self.postMessage({ type: 'result',
          text: 'status = 0x' + (await dev.getStatus()).toString(16).padStart(2, '0') });
        break;
      case 'setRate':
        await dev.setSampleRate(m.hz); log('rate -> ' + m.hz);
        break;
      case 'overload': {
        const o = await dev.getOverload();
        self.postMessage({ type: 'result',
          text: 'overload = ' + Array.from(o).map((b) => b.toString(16).padStart(2, '0')).join(' ') });
        break;
      }
      case 'aux':
        await dev.setControls(8, new Uint8Array([m.on ? 1 : 0])); log('aux out ' + (m.on ? 'ON' : 'off'));
        break;
      case 'capture': startCapture(); break;
      case 'play': await startPlay(); break;
      case 'duplex': await startDuplex(); break;
      case 'stop': if (dev) dev.stop(); break;
    }
  } catch (err) {
    log('error: ' + err);
  }
};

// Peak-meter helper: samples are Float32 in [-1,1); post normalised 0..1 peaks,
// throttled to ~20 Hz.
function meter(peaks, lastPost) {
  return (samples, frames) => {
    for (let c = 0; c < CHANNELS; c++) peaks[c] = 0;
    for (let i = 0; i < frames; i++)
      for (let c = 0; c < CHANNELS; c++) {
        const v = Math.abs(samples[i * CHANNELS + c]);
        if (v > peaks[c]) peaks[c] = v;
      }
    const now = performance.now();
    if (now - lastPost.t >= 50) { self.postMessage({ type: 'levels', peaks: peaks.slice() }); lastPost.t = now; }
    return false;
  };
}

// 440 Hz sine, Float32 in [-1,1), on all 6 channels. Also reports OUT throughput to
// the page log once/second so we can tell whether the stream is actually running.
function tone() {
  const step = (2 * Math.PI * 440) / 48000, amp = 0.2;
  let phase = 0, cbs = 0, framesTot = 0, last = performance.now(), worst = 0, tWin = last;
  return (frames) => {
    const now = performance.now();
    const gap = now - last; last = now; if (gap > worst) worst = gap;
    cbs++; framesTot += frames;
    if (now - tWin >= 1000) {
      log(`OUT: ${cbs}/s, ${framesTot} frames/s (want ~48000), worst gap ${worst.toFixed(1)}ms`);
      cbs = 0; framesTot = 0; worst = 0; tWin = now;
    }
    const arr = new Float32Array(frames * CHANNELS);
    for (let i = 0; i < frames; i++) {
      const s = Math.sin(phase) * amp;
      phase += step; if (phase > 2 * Math.PI) phase -= 2 * Math.PI;
      for (let c = 0; c < CHANNELS; c++) arr[i * CHANNELS + c] = s;
    }
    return arr;
  };
}

function startCapture() {
  log('capture started (Float32).');
  dev.startCaptureF32(meter(new Array(CHANNELS).fill(0), { t: 0 }));
}

// The SL 3's output routing (audio-controls offset 8) defaults to OFF on power-up,
// so enable it or playback reaches the device but never leaves its outputs.
async function enableOutput() {
  try { await dev.setControls(8, new Uint8Array([1])); } catch (e) { log('enable output: ' + e); }
}

async function startPlay() {
  try { await dev.setSampleRate(48000); } catch (e) { log('setSampleRate: ' + e); return; }
  await enableOutput();
  log('playing 440 Hz — full-duplex implicit feedback (Float32).');
  dev.startPlaybackF32(tone());
}

// Simultaneous playback + capture in one full-duplex stream (one iso IN serves both).
async function startDuplex() {
  try { await dev.setSampleRate(48000); } catch (e) { log('setSampleRate: ' + e); return; }
  await enableOutput();
  log('duplex: playing 440 Hz + capturing meters (Float32).');
  dev.startDuplexF32(meter(new Array(CHANNELS).fill(0), { t: 0 }), tone());
}
