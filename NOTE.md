# NOTE.md — Rane SL 3 userland driver: findings, pitfalls, and lessons

Engineering log distilled from the bring-up and the long glitch hunt. Read this
before touching the audio pipeline — most of it is non-obvious and cost real time
to discover.

---

## 1. The device (verified facts)

- **Rane SL 3** (Serato Scratch Live USB DJ interface). USB high-speed (480 Mbps).
- VID `0x1CC5` (7365), PID `0x0001`, serial `SL3.01.00`, `bDeviceProtocol 255`.
- **Vendor-specific class `0xFF/0xFF/0xFF`** — NOT USB-Audio-Class compliant. It
  carries UAC2-*shaped* class-specific descriptors under a vendor class, but you
  cannot drive it with Apple's UAC driver. Nothing binds the interfaces on modern
  macOS (the old kext is dead), so we claim them cleanly from userland.
- **Audio:** 6 channels each direction (3 stereo decks), **24-bit LE**, 44.1 / 48 kHz.
- **Interfaces:**
  - IF0 — AudioControl (descriptors only)
  - IF1 — playback; alt 1 → **iso OUT EP 0x06**, asynchronous
  - IF2 — capture; alt 1 → **iso IN EP 0x82**, *implicit feedback*
  - IF3 — HID-style control; interrupt **IN 0x81 / OUT 0x01**, 64-byte reports
- **The device powers up at 44.1 kHz.** You must explicitly set the rate.

### HID control protocol (IF3)
Reports are `[code:1][seq:LE32][payload…]` zero-padded to 64 bytes; the reply is a
64-byte IN report with payload starting at byte 5. **Every OUT must be followed by
draining its one IN reply, or the next OUT stalls.**

| code | meaning | notes |
|------|---------|-------|
| 0x30 | get rate | reply rate BE16 at `[5..6]` |
| 0x31 | **set rate** | payload = rate BE16 (`0xBB80`=48000, `0xAC44`=44100) |
| 0x32 | get 22-byte audio-controls register | |
| 0x33 | set audio-controls | payload `[offset][count][data]` (aux-output at offset 8) |
| 0x35 | get overload (6B) | |
| 0x0a | get status | |
| 0x34/0x38/0x39 | unsolicited: overload(6B) / phono(3B) / thru(4B) | |

---

## 2. Architecture

macOS gives us no DriverKit entitlement, so audio is exposed via an
**AudioServerPlugin (HAL)** that runs inside sandboxed `coreaudiod` — which *can*
open POSIX shm but must NOT touch USB. So:

```
 USB device ⇄  sl3d daemon  ⇄  /sl3_audio shm ring  ⇄  SL3.driver plugin  ⇄ coreaudiod ⇄ apps
              (owns libusb,        (lock-free            (in coreaudiod)
               real-time)          SPSC rings)
```

- **`sl3d`** (Rust) owns the device on one libusb context, full-duplex, and bridges
  it to the plugin through a shared-memory ring. Isochronous streaming lives in
  `driver/sl3/src/native_audio.rs`.
- **`SL3.driver`** (`coreaudio/SL3PlugIn.cpp`) is the HAL plugin: `ReadInput` pulls
  the capture ring, `WriteMix` fills the playback ring, `GetZeroTimeStamp` provides
  the device clock.
- **shm ABI** is byte-identical between `driver/sl3d/src/shm.rs` and
  `driver/sl3d/include/sl3_shm.h`; `assert_layout()` guards it at daemon startup.

### Isochronous streaming model (the parts that MUST stay)
- **libusb via rusb + raw `libusb1-sys` FFI** — `nusb` has no macOS iso support.
- **Fixed pool of transfers, resubmitted inside the completion callback**, on one
  event-loop thread. NUM=8 transfers × PKTS=16 packets each direction. Allocating /
  freeing transfers per cycle causes `LIBUSB_ERROR_OTHER (-99)`; zero-length iso OUT
  transfers are rejected at submit.
- **Implicit feedback:** the OUT endpoint is async with implicit feedback from the
  IN endpoint. We mirror IN's *exact per-microframe frame counts* onto OUT (via a
  small `VecDeque<[u8; PKTS]>`). An *averaged* rate drifts against the device crystal
  and clicks. This is load-bearing (proven: disabling it clicks).
- **Real-time thread scheduling** (mach `THREAD_TIME_CONSTRAINT_POLICY`) on the
  event-loop thread. Load-bearing (proven). **No I/O or logging on that thread** —
  even an `eprintln!` stalls it.

---

## 3. The great glitch hunt — five stacked root causes

CoreAudio playback was noisy/glitchy for a long time. It was **five separate bugs**,
each masking the next. Every premature "it's fixed" came from peeling one layer.

1. **Device sample-rate mismatch (the first real bug).**
   The daemon never sent `CMD_SET_RATE`, so the hardware free-ran at its 44.1 kHz
   default while CoreAudio fed 48000/s → ~8.8% chronic ring overflow → a hard drop
   every ~140 ms → continuous noise.
   *Fix:* `native_audio` sends `CMD_SET_RATE` at stream start (briefly claims/releases
   IF3); the plugin publishes its chosen rate into `shm.sample_rate`; the daemon
   adopts it and re-locks the device if it changes mid-stream.

2. **coreaudiod produced on a fake clock.**
   `GetZeroTimeStamp` reported a *perfect host-locked 48000*. The device runs on its
   own crystal (48000 ± ppm), so coreaudiod and the device slowly drifted; the ring
   backlog walked to an edge and the daemon's `MAXLAT` hard-drop fired periodically =
   worsening glitches at inconsistent intervals.
   *Fix:* **device-clock anchor.** The daemon publishes `(cap_write, clock_host)` —
   `cap_write` ticks on the hardware clock, `clock_host` is `mach_absolute_time()`
   sampled at that instant. `GetZeroTimeStamp` reports the real device rate from that
   pair, so coreaudiod produces at exactly the device rate and the backlog stays
   centered.

3. **launchd throttled the daemon (the big hidden one).**
   The LaunchDaemon plist had no `ProcessType`, so launchd ran `sl3d` in a throttled
   scheduling/QoS band. Throttled, its event-loop thread dropped **~12% of iso USB
   transfers** (500 → 440 completions/s; `cap_write` ~48000 → ~42300/s), starving the
   device. **A process-level launchd throttle overrides per-thread RT scheduling.**
   This is why hand-run/`natone` tests were always clean but the installed daemon
   glitched.
   *Fix:* `<key>ProcessType</key><string>Interactive</string>` in the plist.

4. **shm orphan on daemon restart.**
   To handle the shm growing by 8 bytes (the anchor field), an early fix did
   `shm_unlink` unconditionally on daemon start. That orphaned any plugin already
   mapped to the old segment — the daemon and plugin ended up on *different* shm
   objects (`play_write` advancing, `play_read` stuck at 0, ring overflowing).
   *Fix:* unlink only when the existing segment's size actually differs.

5. **Multiple daemons fighting for the device.**
   A hand-run test daemon plus the launchd daemon both tried to claim the interfaces;
   **neither streamed** (`present=0`, `cap_write=0`, ring overflowing). Nothing drains
   the ring, so `play_write` climbs unbounded.
   *Fix:* `install.sh` now `pkill -9 -f /usr/local/bin/sl3d` before bootstrapping.

---

## 4. Pitfalls (things that actively fooled us)

- **`natone` masks rate bugs.** The `natone` CLI synthesizes a tone to fill exactly
  whatever frames the device asks for, so a wrong *absolute* rate is just an inaudible
  pitch shift of a clean tone. It is NOT a valid rate-correctness test. It *is* a good
  isolation harness for OUT-pacing/feedback/RT (those produce audible clicks).

- **Wrong-sink false positives.** `install.sh` runs `killall coreaudiod`, and macOS
  then reverts the default output to the built-in speakers. Every "it works" / "still
  broken" that didn't first confirm the sink was meaningless — the audio was going to
  (or the silence was) the Mac speakers, not the SL3. **Always force/confirm the sink.**
  A tiny helper that sets `kAudioHardwarePropertyDefaultOutputDevice` by device name
  removes the ambiguity (`scratchpad/setdefault.c`). `install.sh` prints a loud
  reminder (we chose reminder over auto-hijacking the default).

- **`shmtap`'s "per second" is not exactly one second.** It `nanosleep(1s)` + does
  work, so its window is slightly long. Read its rates as approximate, and always
  cross-check against a same-timebase reference (e.g. `cap_write` = the pure device
  clock) rather than trusting an absolute number.

- **`play_read` is a skewed counter, not the device consumption rate.** The daemon
  advances `play_read` by `min(avail, frames)` and the `MAXLAT` drop jumps it forward,
  so it under-reports during ring churn. Don't diagnose device behavior from it —
  measure the actual OUT frames sent (or use `natone` counters).

- **Too-short A/B tests give wrong verdicts.** The `SL3_RT=0` "no clicks" result came
  from a listen shorter than the ~8 s instability cycle; RT is actually required.
  Real-time behavior must be judged over **minutes**, not seconds.

- **ffmpeg's avfoundation input silently drops multichannel samples.** Recording the
  SL3 (6ch/48k) with `ffmpeg -f avfoundation` produced a WAV with only ~35% of the
  samples (2.13 s file for 6 s wall) and a bogus 12162 s start timestamp — an ffmpeg/
  avfoundation bug, NOT our capture. The ring (`cap_read` keeping pace with
  `cap_write`) and a proper AudioQueue recorder both showed full-rate 48000/s. **Use
  `tools/sl3-record` (AudioQueue), not ffmpeg, to verify this device's capture.**

- **launchd ≠ hand-run environment.** The same binary behaves differently under
  launchd (throttled) vs run from a shell (full priority). When a daemon is worse
  under launchd, suspect `ProcessType` before touching code.

- **macOS shm can only be `ftruncate`d once.** A stale segment from an older struct
  layout will fault a larger mapping. Resize via conditional unlink; never
  unconditionally, or you orphan mapped readers across a restart.

- **RT thread + I/O don't mix.** An `eprintln!`/`syslog` on the streaming thread stalls
  it. All diagnostics must be counters read/printed off the hot path.

- **Output routing defaults OFF on power-up.** The SL 3's audio-controls register at
  **offset 8** gates whether software playback reaches the outputs. It defaults to 0 on
  a freshly powered device, so playback streams into the device but produces **silence
  at the jacks**. Symptom that fooled us: after a device replug, audio "stopped" even
  though the stream ran perfectly (OUT ~48000 frames/s) — the register had reset. The
  driver now sets offset 8 = 1 at stream start (`CMD_SET_CONTROLS` payload
  `[8][1][1]`): native in `native_audio::set_device_config`, web in `worker.js`
  `enableOutput()`. Earlier "it worked" runs only worked because an `sl3-ctl output on`
  / prefPane test had set the bit and the device held it until the next power-cycle.
- Assorted: `set_alt` for iso OUT must use IOKit `SetAlternateInterface`
  (`handle.set_alternate_setting`), not a raw ep0 control transfer (fails `-99`). shm
  must be `fchmod(0666)` so `_coreaudiod` (different uid) can `O_RDWR` (umask would
  drop it to 0644). Case-insensitive APFS: the `sl3` lib and `SL3` plugin targets
  collide — the plugin target is `sl3_plugin`.

---

## 5. Diagnostic techniques that worked

- **`sl3-shmtap`** — attach read-only to the shm and watch `cap_write`, `play_write`,
  `play_read`, backlog, and per-channel RMS. The single most useful tool. Healthy
  playback = `cap_write` ≈ `play_write` ≈ `play_read` ≈ 48000/s with a **small stable
  backlog** (~1024). Overflow-and-dump, a stuck `play_write=0`, or a slow `cap_write`
  each point at a different layer.
- **Run the daemon by hand as your user** (once the LaunchDaemon is booted out) to
  isolate code from the launchd environment: `./bazel-bin/driver/sl3d/sl3d 48000`.
- **`natone` with temporary counters** — `in_xfers/out_xfers/in_frames/out_frames/
  fb_drops/nominal_used` proved the device streaming was flawless (500/s each way,
  zero drops) and pushed the fault out to the daemon↔coreaudiod bridge.
- **Measure the device's real clock early** — a two-minute `cap_write` delta. Had we
  done this first, we'd have found the 44.1k default immediately instead of piling on
  buffering/RT machinery to mask it.
- **`tools/sl3-record`** — AudioQueue recorder + 6-channel input meter; records the SL3
  input through the HAL plugin to a WAV and reports exact frames/s + per-channel RMS.
  The trustworthy way to verify capture end-to-end (ffmpeg lies here).

---

## 6. Lessons to NOTE

1. **Measure the hardware clock before adding machinery.** The rate mismatch made
   every experiment inconclusive, which *drove* the over-engineering. One clean
   measurement up front beats ten hypotheses.
2. **A real-time USB/audio/DSP daemon on macOS needs BOTH** per-thread RT scheduling
   *and* `ProcessType=Interactive`. Either alone is insufficient under launchd.
3. **The HAL plugin's `GetZeroTimeStamp` must reflect the real hardware clock**, not a
   host-locked nominal rate. This is the canonical fix for producer/consumer drift in
   a shm-bridged HAL driver.
4. **Confirm the sink, the build, and the process count before believing any audio
   test.** Most false readings came from testing the wrong sink or a stale/duplicate
   daemon, not from the code.
5. **Don't strip "suspect" complexity without a hardware A/B at the right timescale.**
   Implicit feedback and RT scheduling both looked removable and both were essential.
6. **Keep diagnostics off the real-time thread**; make them counters, print at teardown.
7. **"Idle" CPU is a design choice, not a leak.** sl3d's ~20% at rest was the
   always-on full-duplex iso stream, not a busy-wait. The fix is to *stop streaming*
   when idle, not to shave the hot path. Sample the actual stacks before optimizing —
   the trivial per-sample conversion was never the cost.
8. **A suspended producer can't be woken by watching ring indices.** While sl3d stops,
   it writes no capture frames, so `cap_read` never advances for an input-only client
   (`avail==0 → consumed==0`). Resume needs an *explicit* activity flag published by
   the plugin (`io_running`), not inferred counter movement. Output-only would have
   moved `play_write`, but input-only is silent — the general rule is: don't infer
   liveness from a channel the suspended side has frozen.

---

## 7. Still open

- **Input: FULLY VERIFIED (plumbing + real signal).** `tools/sl3-record` captures the
  SL3 at a true ~48048 frames/s, 6 channels, full-length WAV, zero drops (plugin
  `ReadInput` → coreaudiod → app; ring `cap_read` keeps pace with `cap_write`). A
  Serato control vinyl on **Deck 1** produced a healthy stereo timecode signal (−17 dB
  peak) whose carrier read **~1040 Hz** (Serato nominal 1000 Hz; the ~4% is turntable
  platter speed, NOT a rate error — a 44.1k-vs-48k bug would read ~1088 Hz). Correct
  rate, intact/decodable signal, confirmed.
  - **Capture channel map (empirical):**
    - ch1/2 = **Aux** (line input) — inferred (sat at noise floor during both deck
      tests; no source available to positively confirm)
    - ch3/4 = **Deck 1** (phono) — verified with control vinyl
    - ch5/6 = **Deck 2** (phono) — verified with control vinyl
    Note the deck order is rotated relative to the channel index (Deck 1 is NOT ch1/2).
- **WebUSB output: CLEAN, glitch-free — solved (Worker + implicit feedback).** Both
  pieces are required; each alone fails:
  1. **Run streaming in a dedicated Web Worker** (`web/worker.js`, opens the device via
     `getDevices()` — the main thread grants it once via `requestDevice`). On the main
     thread, Chrome gates iso-OUT transmission on JS resubmitting and the main thread
     janks up to ~48 ms → the OUT endpoint underruns → clicks. In a Worker the
     worst callback gap drops to ~2–12 ms (measured), fully absorbed by the buffer.
     The Worker alone still clicks though — from drift.
  2. **Full-duplex implicit feedback** (`stream::playback_fd`): run the capture IN
     endpoint purely to read the device's per-packet frame counts and mirror them onto
     OUT, locking OUT to the device clock (nominal pacing drifts and clicks). Same fix
     as native. On the main thread this couldn't help because the 48 ms gaps dominated;
     in the Worker it's the last mile to clean.
  Result: 48000 frames/s, worst-gap 2–12 ms, **no clicks**. Needs 32 transfers in
  flight (`NUM_TRANSFERS`) for buffer. (The earlier "browser iso-OUT timing floor /
  use the native driver" conclusion was WRONG — it was main-thread jank + drift, both
  fixable.) WebUSB control + capture also work fine.
- **M6d**: verify the original `SL 3 Audio Control Panel.prefPane` drives the userland
  driver via the drop-in `Sl3Api.framework` (macOS legacy-prefPane / library-validation
  caveats noted).

---

## 8. Idle CPU: suspend the USB stream when no client is doing IO

**Symptom.** sl3d burned a steady ~20% CPU (PID sample: ~36 min CPU over ~3 h) even
with nothing playing.

**Root cause (not a bug).** The daemon ran a full-duplex isochronous stream *at all
times* — capture IN and playback OUT both live regardless of activity — because
(a) implicit feedback locks OUT to the IN clock, so OUT must always mirror IN, and
(b) the plugin's `GetZeroTimeStamp` relies on the `(cap_write, clock_host)` anchor
that only the running capture stream advances. At 48 kHz that's ~8000 iso packets/s
each direction → ~1000 transfer-completion callbacks/s serviced by libusb/IOKit. The
cost is that servicing, **not** the per-sample s24↔f32 conversion (which is trivial).
Not a busy-wait: `libusb_handle_events_timeout_completed` blocks in poll.

**Fix (this change).** Stream only while a CoreAudio client is doing IO; otherwise drop
the stream and light-poll. Idle now costs ~0% instead of ~20%.

- **New shm field `io_running`** (`sl3_shm.h` / `shm.rs`, **version bumped 1→2**). The
  plugin sets it `1` in `StartIO` (first client) and `0` in `StopIO` (last client) —
  it just mirrors its existing `gIORunning` refcount.
- **Daemon** (`sl3d/src/main.rs`): the device stays *open* across idle (interfaces
  claimed, **alt-setting 0 → zero USB bandwidth**); the iso stream runs only while
  `io_running != 0`. Idle → `sleep(10 ms)` poll loop. The capture callback counts
  frames seen while `io_running == 0` and stops the stream after ~1 s of idle
  (debounces apps that briefly stop/restart IO). On suspend it clears `clock_host` so
  the plugin's `GetZeroTimeStamp` takes its host-locked fallback instead of a stale
  anchor during the resume gap.

**Why an explicit flag (not counter-watching).** While suspended the daemon produces
no capture data, so `cap_read` never advances for an **input-only** client
(`avail==0 → consumed==0`). There is no ring movement to signal resume — hence the
plugin must publish activity explicitly. (See lesson 8.)

**ABI note.** The field is appended after `clock_host`, so existing offsets are
unchanged (an un-recompiled `sl3-shmtap` still reads fine). Size grows +8 (u32 + 8-byte
trailing pad): C `sizeof=1572944`, `io_running` at offset `1572936`; matches Rust
`assert_layout` (`64 + 2·RING_SAMPLES·4 + 16`). The version bump forces the daemon to
recreate the segment on install — **plugin and daemon must be reinstalled together**;
a v1 plugin against a v2 daemon would misread the layout.

**Trade-off.** First ~10–80 ms after playback/record starts runs on the host-locked
clock fallback + primed silence — a brief one-time transient at stream start, masked by
the playback cushion. Accepted as the price of ~0% idle. Implicit feedback and RT
scheduling are untouched (they only run while actually streaming now).

**Status: built, ABI-verified, NOT yet hardware-tested.** Needs on-device confirmation
after installing both components: idle → ~0% (`top -pid <sl3d>`), glitch-free resume on
play *and* record, and input-only wake. `sl3-shmtap` will read 0 heartbeats while
suspended — that is now correct (idle), not a hang.
