// sl3_shm.h — shared-memory ABI between the sl3d daemon (owns the USB device)
// and the SL3 AudioServerPlugin (runs inside coreaudiod, which cannot touch USB).
//
// One POSIX shared-memory object holds a header plus two single-producer/
// single-consumer float32 rings:
//   - capture ring:  daemon = producer, plugin = consumer  (device -> apps)
//   - playback ring:  plugin = producer, daemon = consumer  (apps -> device)
//
// Both processes are the same architecture, so a std::atomic<uint64_t> placed in
// shared memory is valid and lock-free for cross-process index exchange.
#ifndef SL3_SHM_H
#define SL3_SHM_H

#include <stdint.h>
#include <atomic>

#define SL3_SHM_NAME     "/sl3_audio"
#define SL3_SHM_MAGIC    0x334C5341u   // 'ASL3'
#define SL3_SHM_VERSION  2

#define SL3_SHM_CHANNELS 6
// Ring capacity in frames per direction (power of two for cheap masking).
// 32768 frames ≈ 0.68 s at 48 kHz — ample slack to absorb USB/host clock drift.
#define SL3_RING_FRAMES  32768u
#define SL3_RING_MASK    (SL3_RING_FRAMES - 1u)
#define SL3_RING_SAMPLES (SL3_RING_FRAMES * SL3_SHM_CHANNELS)

struct sl3_shm {
    uint32_t magic;
    uint32_t version;
    uint32_t channels;                 // == SL3_SHM_CHANNELS
    std::atomic<uint32_t> sample_rate; // current device rate (44100/48000)
    std::atomic<uint32_t> device_present;   // daemon sets 1 when SL3 is open+streaming
    std::atomic<uint64_t> daemon_heartbeat; // daemon bumps each IO cycle (liveness)

    // Capture ring (daemon -> plugin). Indices are absolute frame counts.
    std::atomic<uint64_t> cap_write;   // producer: daemon
    std::atomic<uint64_t> cap_read;    // consumer: plugin
    // Playback ring (plugin -> daemon).
    std::atomic<uint64_t> play_write;  // producer: plugin
    std::atomic<uint64_t> play_read;   // consumer: daemon

    // Interleaved float32, SL3_SHM_CHANNELS per frame.
    float cap[SL3_RING_SAMPLES];
    float play[SL3_RING_SAMPLES];

    // Device-clock anchor: mach_absolute_time() sampled when cap_write last
    // advanced. Plugin uses (cap_write, clock_host) in GetZeroTimeStamp so
    // coreaudiod tracks the REAL device rate instead of a fake host-locked 48000.
    std::atomic<uint64_t> clock_host;

    // Client IO activity, published by the plugin: 1 while coreaudiod is running
    // IO against this device (>=1 client), 0 when fully idle. The daemon watches
    // this to suspend the (always-on, ~20% CPU) USB isochronous stream when no
    // app is using the device, and to resume it on the next StartIO. An explicit
    // flag is required because a suspended daemon produces no capture data, so the
    // ring indices never move for an input-only client — they can't signal resume.
    std::atomic<uint32_t> io_running;
};

// 24-bit int (sign-extended in int32) <-> float32 [-1,1) conversions.
static inline float sl3_i24_to_f32(int32_t s) { return (float)s / 8388608.0f; } // /2^23
static inline int32_t sl3_f32_to_i24(float f) {
    float v = f * 8388608.0f;
    if (v >  8388607.0f) v =  8388607.0f;
    if (v < -8388608.0f) v = -8388608.0f;
    return (int32_t)v;
}

#endif // SL3_SHM_H
