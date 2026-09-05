// sl3-record — reliable CoreAudio (AudioQueue) recorder + 6-channel input meter
// for the Rane SL 3. Records from the named input device straight through the HAL
// plugin's ReadInput path, writes a proper WAV, and reports exact frames/s and
// per-channel RMS. More trustworthy than ffmpeg/avfoundation for this device
// (avfoundation drops multichannel samples and mangles timestamps).
//
//   usage: sl3-record [secs] [out.wav] [device-name-substring]
//   e.g.:  sl3-record 8 /tmp/deck1.wav "Rane SL 3"
#include <AudioToolbox/AudioToolbox.h>
#include <CoreAudio/CoreAudio.h>
#include <cmath>
#include <cstdint>
#include <cstdio>
#include <cstring>
#include <unistd.h>

static const int NCH = 6;            // SL 3 presents 6 input channels
static const double SR = 48000.0;
static long g_frames = 0;
static double g_sq[NCH];
static FILE *g_wav;
static uint32_t g_data_bytes;

static void write_wav_header(FILE *f, uint32_t data_bytes) {
    uint32_t byte_rate = (uint32_t)SR * NCH * 2, riff = 36 + data_bytes;
    uint16_t block = NCH * 2, bits = 16, fmt = 1, ch = NCH, sr = (uint32_t)SR;
    fwrite("RIFF", 1, 4, f); fwrite(&riff, 4, 1, f); fwrite("WAVE", 1, 4, f);
    fwrite("fmt ", 1, 4, f); uint32_t sixteen = 16; fwrite(&sixteen, 4, 1, f);
    fwrite(&fmt, 2, 1, f); fwrite(&ch, 2, 1, f);
    uint32_t srr = (uint32_t)SR; fwrite(&srr, 4, 1, f); fwrite(&byte_rate, 4, 1, f);
    fwrite(&block, 2, 1, f); fwrite(&bits, 2, 1, f);
    fwrite("data", 1, 4, f); fwrite(&data_bytes, 4, 1, f);
    (void)sr;
}

static void cb(void *, AudioQueueRef q, AudioQueueBufferRef b, const AudioTimeStamp *,
               UInt32, const AudioStreamPacketDescription *) {
    const int16_t *s = (const int16_t *)b->mAudioData;
    UInt32 frames = b->mAudioDataByteSize / (sizeof(int16_t) * NCH);
    for (UInt32 i = 0; i < frames; i++)
        for (int c = 0; c < NCH; c++) { double v = s[i * NCH + c] / 32768.0; g_sq[c] += v * v; }
    g_frames += frames;
    if (g_wav) { fwrite(b->mAudioData, 1, b->mAudioDataByteSize, g_wav); g_data_bytes += b->mAudioDataByteSize; }
    AudioQueueEnqueueBuffer(q, b, 0, NULL);
}

static AudioDeviceID find_input(const char *name, CFStringRef *uidOut) {
    AudioObjectPropertyAddress a = { kAudioHardwarePropertyDevices,
        kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyElementMain };
    UInt32 sz = 0; AudioObjectGetPropertyDataSize(kAudioObjectSystemObject, &a, 0, NULL, &sz);
    int n = sz / sizeof(AudioDeviceID); AudioDeviceID *ids = (AudioDeviceID *)malloc(sz);
    AudioObjectGetPropertyData(kAudioObjectSystemObject, &a, 0, NULL, &sz, ids);
    AudioDeviceID found = 0;
    for (int i = 0; i < n; i++) {
        CFStringRef nm = NULL; UInt32 s2 = sizeof(nm);
        AudioObjectPropertyAddress na = { kAudioObjectPropertyName,
            kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyElementMain };
        AudioObjectGetPropertyData(ids[i], &na, 0, NULL, &s2, &nm);
        char buf[256] = "?"; if (nm) { CFStringGetCString(nm, buf, 256, kCFStringEncodingUTF8); CFRelease(nm); }
        AudioObjectPropertyAddress sa = { kAudioDevicePropertyStreams,
            kAudioObjectPropertyScopeInput, kAudioObjectPropertyElementMain };
        UInt32 ss = 0; AudioObjectGetPropertyDataSize(ids[i], &sa, 0, NULL, &ss);
        if (ss > 0 && strstr(buf, name)) {
            found = ids[i]; UInt32 us = sizeof(*uidOut);
            AudioObjectPropertyAddress ua = { kAudioDevicePropertyDeviceUID,
                kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyElementMain };
            AudioObjectGetPropertyData(ids[i], &ua, 0, NULL, &us, uidOut);
            break;
        }
    }
    free(ids); return found;
}

int main(int argc, char **argv) {
    int secs = argc > 1 ? atoi(argv[1]) : 8;
    const char *out = argc > 2 ? argv[2] : "/tmp/sl3-record.wav";
    const char *name = argc > 3 ? argv[3] : "Rane SL 3";

    CFStringRef uid = NULL;
    AudioDeviceID dev = find_input(name, &uid);
    if (!dev) { fprintf(stderr, "input device '%s' not found\n", name); return 1; }

    AudioStreamBasicDescription fmt = {0};
    fmt.mSampleRate = SR; fmt.mFormatID = kAudioFormatLinearPCM;
    fmt.mFormatFlags = kLinearPCMFormatFlagIsSignedInteger | kLinearPCMFormatFlagIsPacked;
    fmt.mBitsPerChannel = 16; fmt.mChannelsPerFrame = NCH;
    fmt.mFramesPerPacket = 1; fmt.mBytesPerFrame = 2 * NCH; fmt.mBytesPerPacket = 2 * NCH;

    g_wav = fopen(out, "wb");
    if (g_wav) write_wav_header(g_wav, 0); // placeholder; patched at the end

    AudioQueueRef q;
    OSStatus st = AudioQueueNewInput(&fmt, cb, NULL, NULL, NULL, 0, &q);
    if (st) { fprintf(stderr, "AudioQueueNewInput err %d\n", (int)st); return 2; }
    if (uid) AudioQueueSetProperty(q, kAudioQueueProperty_CurrentDevice, &uid, sizeof(uid));
    for (int i = 0; i < 4; i++) {
        AudioQueueBufferRef b; AudioQueueAllocateBuffer(q, fmt.mBytesPerFrame * 4800, &b);
        AudioQueueEnqueueBuffer(q, b, 0, NULL);
    }
    st = AudioQueueStart(q, NULL);
    if (st) { fprintf(stderr, "AudioQueueStart err %d (mic permission?)\n", (int)st); return 3; }
    printf("recording %ds from '%s' -> %s\n", secs, name, out);
    sleep(secs);
    AudioQueueStop(q, true);

    if (g_wav) { fseek(g_wav, 0, SEEK_SET); write_wav_header(g_wav, g_data_bytes); fclose(g_wav); }
    printf("captured %ld frames = %.1f frames/s (expect ~48000)\n", g_frames, (double)g_frames / secs);
    printf("per-channel RMS:");
    for (int c = 0; c < NCH; c++) printf(" ch%d=%.4f", c + 1, sqrt(g_sq[c] / (g_frames ? g_frames : 1)));
    printf("\n");
    return 0;
}
