// Thin UI client. ALL USB work (control, capture, playback) happens in a Web Worker
// (see worker.js) — the main thread only grants device permission (a user gesture)
// and relays button clicks / meter updates over postMessage. Keeping streaming off
// the main thread is what makes WebUSB iso-OUT glitch-free (main-thread jank clicks).

const CHANNELS = 6;
const $ = (id) => document.getElementById(id);
let worker = null;

function log(msg) {
  const el = $('log');
  el.textContent += msg + '\n';
  el.scrollTop = el.scrollHeight;
}

function setConnected(on) {
  $('state').textContent = on ? 'connected (worker)' : 'not connected';
  document.querySelectorAll('.ctl').forEach((b) => (b.disabled = !on));
  $('connect').disabled = on;
}

function buildMeters() {
  const m = $('meters');
  m.innerHTML = '';
  for (let c = 0; c < CHANNELS; c++) {
    const label = document.createElement('div');
    label.textContent = 'CH' + (c + 1);
    const bar = document.createElement('div');
    bar.className = 'bar';
    const fill = document.createElement('span');
    fill.id = 'bar' + c;
    bar.appendChild(fill);
    const db = document.createElement('div');
    db.id = 'db' + c;
    db.textContent = '-inf';
    m.append(label, bar, db);
  }
}

function showLevels(peaks) {
  for (let c = 0; c < CHANNELS; c++) {
    const norm = peaks[c]; // worker posts normalised 0..1 peaks (Float32 path)
    const db = norm > 0 ? 20 * Math.log10(norm) : -Infinity;
    const pct = Math.max(0, Math.min(100, ((db + 60) / 60) * 100));
    $('bar' + c).style.width = pct + '%';
    $('db' + c).textContent = isFinite(db) ? db.toFixed(1) : '-inf';
  }
}

function ensureWorker() {
  if (worker) return worker;
  worker = new Worker(new URL('./worker.js', import.meta.url), { type: 'module' });
  worker.onmessage = (e) => {
    const m = e.data;
    switch (m.type) {
      case 'opened': setConnected(true); log('device opened in worker. rate readback: ' + m.rate + ' Hz'); break;
      case 'log': log(m.msg); break;
      case 'result': log(m.text); break;
      case 'levels': showLevels(m.peaks); break;
    }
  };
  worker.onerror = (e) => log('worker error: ' + e.message);
  return worker;
}

function main() {
  buildMeters();
  log('click Connect and pick the SL 3 (all USB runs in a Web Worker).');

  $('connect').onclick = async () => {
    try {
      // Grant permission on the main thread (WebUSB requires a user gesture); the
      // Worker then opens the same device via getDevices().
      await navigator.usb.requestDevice({ filters: [{ vendorId: 0x1cc5, productId: 0x0001 }] });
    } catch (e) { log('device grant cancelled: ' + e); return; }
    ensureWorker().postMessage({ type: 'open' });
  };

  const send = (msg) => worker && worker.postMessage(msg);
  $('status').onclick = () => send({ type: 'status' });
  $('rate44').onclick = () => send({ type: 'setRate', hz: 44100 });
  $('rate48').onclick = () => send({ type: 'setRate', hz: 48000 });
  $('overload').onclick = () => send({ type: 'overload' });
  $('outon').onclick = () => send({ type: 'aux', on: true });
  $('outoff').onclick = () => send({ type: 'aux', on: false });

  const streamBtns = ['capture', 'playtone', 'duplex'];
  const startStreaming = (type, label) => {
    send({ type });
    $('stop').disabled = false;
    streamBtns.forEach((id) => ($(id).disabled = true));
    log(label);
  };
  $('capture').onclick = () => startStreaming('capture', 'capture started (worker).');
  $('playtone').onclick = () => startStreaming('play', 'playback started (worker).');
  $('duplex').onclick = () => startStreaming('duplex', 'duplex (play + capture) started.');

  $('stop').onclick = () => {
    send({ type: 'stop' });
    $('stop').disabled = true;
    streamBtns.forEach((id) => ($(id).disabled = false));
    log('stop requested.');
  };
}

main();
