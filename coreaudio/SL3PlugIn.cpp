// SL3PlugIn.cpp — CoreAudio AudioServerPlugin exposing the Rane SL 3 as a
// 6-in / 6-out audio device. Runs inside coreaudiod; the USB device itself is
// owned by the sl3d daemon, which this plugin reaches through a shared-memory
// ring (see driver/sl3d/include/sl3_shm.h). IO is a simple FIFO against those rings.
//
// Structure follows Apple's NullAudio sample: a single plug-in object owns one
// device, which owns one input stream and one output stream. Object IDs are fixed.
#include <CoreAudio/AudioServerPlugIn.h>
#include <mach/mach_time.h>
#include <pthread.h>
#include <fcntl.h>
#include <sys/mman.h>
#include <unistd.h>
#include <string.h>
#include <stdint.h>
#include <syslog.h>
#include <errno.h>

#include "sl3_shm.h"

// ----- fixed object IDs -----
enum {
    kObjectID_PlugIn        = kAudioObjectPlugInObject,  // 1
    kObjectID_Device        = 2,
    kObjectID_Stream_Input  = 3,
    kObjectID_Stream_Output = 4,
};

#define kDevice_UID        "RaneSL3:0"
#define kDevice_ModelUID   "RaneSL3:Model"
#define kChannels          SL3_SHM_CHANNELS
#define kBytesPerFrame     (kChannels * (UInt32)sizeof(Float32))
#define kRingPeriodFrames  512u    // zero-timestamp period

// ----- plugin-wide state -----
static AudioServerPlugInHostRef gHost = NULL;
static pthread_mutex_t gMutex = PTHREAD_MUTEX_INITIALIZER;
static UInt32   gRefCount = 0;
static Float64  gSampleRate = 48000.0;
static bool     gInputActive = true;
static bool     gOutputActive = true;

// IO timing
static UInt64   gAnchorHostTime = 0;
static Float64  gHostTicksPerFrame = 0.0;
static UInt32   gIORunning = 0;

// shared memory
static sl3_shm *gShm = NULL;

static void ensure_shm() {
    if (gShm) return;
    int fd = shm_open(SL3_SHM_NAME, O_RDWR, 0666);
    if (fd < 0) { syslog(LOG_NOTICE, "SL3plugin: shm_open failed errno=%d (%s)", errno, strerror(errno)); return; }
    void *p = mmap(NULL, sizeof(sl3_shm), PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    close(fd);
    if (p == MAP_FAILED) { syslog(LOG_NOTICE, "SL3plugin: mmap failed errno=%d", errno); return; }
    sl3_shm *s = (sl3_shm *)p;
    if (s->magic != SL3_SHM_MAGIC) { syslog(LOG_NOTICE, "SL3plugin: bad magic 0x%x", s->magic); munmap(p, sizeof(sl3_shm)); return; }
    gShm = s;
}

// ============================ COM plumbing =================================
static HRESULT QueryInterface(void *inDriver, REFIID inUUID, LPVOID *outInterface);
static ULONG   AddRef(void *inDriver);
static ULONG   Release(void *inDriver);
static OSStatus Initialize(AudioServerPlugInDriverRef, AudioServerPlugInHostRef);
static OSStatus CreateDevice(AudioServerPlugInDriverRef, CFDictionaryRef, const AudioServerPlugInClientInfo *, AudioObjectID *);
static OSStatus DestroyDevice(AudioServerPlugInDriverRef, AudioObjectID);
static OSStatus AddDeviceClient(AudioServerPlugInDriverRef, AudioObjectID, const AudioServerPlugInClientInfo *);
static OSStatus RemoveDeviceClient(AudioServerPlugInDriverRef, AudioObjectID, const AudioServerPlugInClientInfo *);
static OSStatus PerformDeviceConfigurationChange(AudioServerPlugInDriverRef, AudioObjectID, UInt64, void *);
static OSStatus AbortDeviceConfigurationChange(AudioServerPlugInDriverRef, AudioObjectID, UInt64, void *);
static Boolean  HasProperty(AudioServerPlugInDriverRef, AudioObjectID, pid_t, const AudioObjectPropertyAddress *);
static OSStatus IsPropertySettable(AudioServerPlugInDriverRef, AudioObjectID, pid_t, const AudioObjectPropertyAddress *, Boolean *);
static OSStatus GetPropertyDataSize(AudioServerPlugInDriverRef, AudioObjectID, pid_t, const AudioObjectPropertyAddress *, UInt32, const void *, UInt32 *);
static OSStatus GetPropertyData(AudioServerPlugInDriverRef, AudioObjectID, pid_t, const AudioObjectPropertyAddress *, UInt32, const void *, UInt32, UInt32 *, void *);
static OSStatus SetPropertyData(AudioServerPlugInDriverRef, AudioObjectID, pid_t, const AudioObjectPropertyAddress *, UInt32, const void *, UInt32, const void *);
static OSStatus StartIO(AudioServerPlugInDriverRef, AudioObjectID, UInt32);
static OSStatus StopIO(AudioServerPlugInDriverRef, AudioObjectID, UInt32);
static OSStatus GetZeroTimeStamp(AudioServerPlugInDriverRef, AudioObjectID, UInt32, Float64 *, UInt64 *, UInt64 *);
static OSStatus WillDoIOOperation(AudioServerPlugInDriverRef, AudioObjectID, UInt32, UInt32, Boolean *, Boolean *);
static OSStatus BeginIOOperation(AudioServerPlugInDriverRef, AudioObjectID, UInt32, UInt32, UInt32, const AudioServerPlugInIOCycleInfo *);
static OSStatus DoIOOperation(AudioServerPlugInDriverRef, AudioObjectID, AudioObjectID, UInt32, UInt32, UInt32, const AudioServerPlugInIOCycleInfo *, void *, void *);
static OSStatus EndIOOperation(AudioServerPlugInDriverRef, AudioObjectID, UInt32, UInt32, UInt32, const AudioServerPlugInIOCycleInfo *);

static AudioServerPlugInDriverInterface gInterface = {
    NULL, QueryInterface, AddRef, Release,
    Initialize, CreateDevice, DestroyDevice, AddDeviceClient, RemoveDeviceClient,
    PerformDeviceConfigurationChange, AbortDeviceConfigurationChange,
    HasProperty, IsPropertySettable, GetPropertyDataSize, GetPropertyData, SetPropertyData,
    StartIO, StopIO, GetZeroTimeStamp, WillDoIOOperation, BeginIOOperation, DoIOOperation, EndIOOperation
};
static AudioServerPlugInDriverInterface *gInterfacePtr = &gInterface;
static AudioServerPlugInDriverRef gDriverRef = &gInterfacePtr;

// The CFPlugIn factory entry point (referenced from Info.plist).
extern "C" void *SL3_Create(CFAllocatorRef, CFUUIDRef requestedTypeUUID);
void *SL3_Create(CFAllocatorRef, CFUUIDRef requestedTypeUUID) {
    if (CFEqual(requestedTypeUUID, kAudioServerPlugInTypeUUID)) { AddRef(gDriverRef); return gDriverRef; }
    return NULL;
}

static HRESULT QueryInterface(void *, REFIID inUUID, LPVOID *outInterface) {
    CFUUIDRef u = CFUUIDCreateFromUUIDBytes(NULL, inUUID);
    HRESULT res = E_NOINTERFACE;
    if (CFEqual(u, IUnknownUUID) || CFEqual(u, kAudioServerPlugInDriverInterfaceUUID)) {
        AddRef(gDriverRef); *outInterface = gDriverRef; res = S_OK;
    }
    CFRelease(u);
    return res;
}
static ULONG AddRef(void *)  { pthread_mutex_lock(&gMutex); ULONG r = ++gRefCount; pthread_mutex_unlock(&gMutex); return r; }
static ULONG Release(void *) { pthread_mutex_lock(&gMutex); ULONG r = gRefCount ? --gRefCount : 0; pthread_mutex_unlock(&gMutex); return r; }

static OSStatus Initialize(AudioServerPlugInDriverRef, AudioServerPlugInHostRef inHost) {
    gHost = inHost;
    mach_timebase_info_data_t tb; mach_timebase_info(&tb);
    double hostClockHz = (1.0e9 * tb.denom) / tb.numer;   // host ticks per second
    gHostTicksPerFrame = hostClockHz / gSampleRate;
    ensure_shm();
    return noErr;
}

// Devices are static; creation/destruction are unsupported.
static OSStatus CreateDevice(AudioServerPlugInDriverRef, CFDictionaryRef, const AudioServerPlugInClientInfo *, AudioObjectID *) { return kAudioHardwareUnsupportedOperationError; }
static OSStatus DestroyDevice(AudioServerPlugInDriverRef, AudioObjectID) { return kAudioHardwareUnsupportedOperationError; }
static OSStatus AddDeviceClient(AudioServerPlugInDriverRef, AudioObjectID, const AudioServerPlugInClientInfo *) { return noErr; }
static OSStatus RemoveDeviceClient(AudioServerPlugInDriverRef, AudioObjectID, const AudioServerPlugInClientInfo *) { return noErr; }
static OSStatus PerformDeviceConfigurationChange(AudioServerPlugInDriverRef, AudioObjectID, UInt64 inChangeAction, void *) {
    if (inChangeAction == 44100 || inChangeAction == 48000) {
        pthread_mutex_lock(&gMutex);
        gSampleRate = (Float64)inChangeAction;
        mach_timebase_info_data_t tb; mach_timebase_info(&tb);
        double hostClockHz = (1.0e9 * tb.denom) / tb.numer;
        gHostTicksPerFrame = hostClockHz / gSampleRate;
        ensure_shm();
        if (gShm) gShm->sample_rate.store((uint32_t)gSampleRate); // daemon re-locks the device
        pthread_mutex_unlock(&gMutex);
    }
    return noErr;
}
static OSStatus AbortDeviceConfigurationChange(AudioServerPlugInDriverRef, AudioObjectID, UInt64, void *) { return noErr; }

// ============================ IO ==========================================
static OSStatus StartIO(AudioServerPlugInDriverRef, AudioObjectID inDeviceID, UInt32) {
    if (inDeviceID != kObjectID_Device) return kAudioHardwareBadObjectError;
    pthread_mutex_lock(&gMutex);
    if (gIORunning++ == 0) {
        gAnchorHostTime = mach_absolute_time();
        ensure_shm();
        if (gShm) {   // start reading/writing from "now"
            gShm->sample_rate.store((uint32_t)gSampleRate); // tell the daemon our rate
            gShm->cap_read.store(gShm->cap_write.load());
            gShm->play_write.store(gShm->play_read.load());
        }
    }
    pthread_mutex_unlock(&gMutex);
    return noErr;
}
static OSStatus StopIO(AudioServerPlugInDriverRef, AudioObjectID inDeviceID, UInt32) {
    if (inDeviceID != kObjectID_Device) return kAudioHardwareBadObjectError;
    pthread_mutex_lock(&gMutex);
    if (gIORunning > 0) gIORunning--;
    pthread_mutex_unlock(&gMutex);
    return noErr;
}

static OSStatus GetZeroTimeStamp(AudioServerPlugInDriverRef, AudioObjectID, UInt32,
                                 Float64 *outSampleTime, UInt64 *outHostTime, UInt64 *outSeed) {
    // Lock to the REAL device clock: the daemon publishes (cap_write, clock_host),
    // a true correlation between the hardware sample count and host time. Report
    // the most recent ring-period boundary at/below cap_write with its host time,
    // so coreaudiod produces at the device's actual rate (no drift, no ring dumps).
    if (gShm) {
        uint64_t F = gShm->cap_write.load(std::memory_order_acquire);
        uint64_t H = gShm->clock_host.load(std::memory_order_acquire);
        if (F > 0 && H > 0) {
            uint64_t periods = F / kRingPeriodFrames;
            uint64_t boundary = periods * kRingPeriodFrames;
            // Interpolate the host time back to the boundary (< one period away).
            uint64_t framesPast = F - boundary;
            *outSampleTime = (Float64)boundary;
            *outHostTime   = H - (UInt64)((Float64)framesPast * gHostTicksPerFrame);
            *outSeed       = 1;
            return noErr;
        }
    }
    // Fallback until the anchor is live (device not yet streaming): host-locked.
    UInt64 now = mach_absolute_time();
    Float64 ticksPerPeriod = gHostTicksPerFrame * kRingPeriodFrames;
    UInt64 periods = (UInt64)(((Float64)(now - gAnchorHostTime)) / ticksPerPeriod);
    *outSampleTime = (Float64)(periods * kRingPeriodFrames);
    *outHostTime   = gAnchorHostTime + (UInt64)(periods * ticksPerPeriod);
    *outSeed       = 1;
    return noErr;
}

static OSStatus WillDoIOOperation(AudioServerPlugInDriverRef, AudioObjectID, UInt32,
                                  UInt32 inOperationID, Boolean *outWillDo, Boolean *outWillDoInPlace) {
    bool will = (inOperationID == kAudioServerPlugInIOOperationReadInput ||
                 inOperationID == kAudioServerPlugInIOOperationWriteMix);
    if (outWillDo) *outWillDo = will;
    if (outWillDoInPlace) *outWillDoInPlace = true;
    return noErr;
}
static OSStatus BeginIOOperation(AudioServerPlugInDriverRef, AudioObjectID, UInt32, UInt32, UInt32, const AudioServerPlugInIOCycleInfo *) { return noErr; }
static OSStatus EndIOOperation(AudioServerPlugInDriverRef, AudioObjectID, UInt32, UInt32, UInt32, const AudioServerPlugInIOCycleInfo *) { return noErr; }

static OSStatus DoIOOperation(AudioServerPlugInDriverRef, AudioObjectID, AudioObjectID,
                              UInt32 /*inClientID*/, UInt32 inOperationID, UInt32 inIOBufferFrameSize,
                              const AudioServerPlugInIOCycleInfo *, void *ioMainBuffer, void *) {
    if (!gShm) { ensure_shm(); }
    Float32 *buf = (Float32 *)ioMainBuffer;

    if (inOperationID == kAudioServerPlugInIOOperationReadInput) {
        // capture ring (daemon producer) -> app. FIFO: take latest available frames.
        if (!gShm) { memset(buf, 0, inIOBufferFrameSize * kBytesPerFrame); return noErr; }
        uint64_t r = gShm->cap_read.load(std::memory_order_relaxed);
        uint64_t w = gShm->cap_write.load(std::memory_order_acquire);
        uint64_t avail = w - r;
        for (UInt32 i = 0; i < inIOBufferFrameSize; i++) {
            Float32 *dst = buf + (size_t)i * kChannels;
            if (i < avail) memcpy(dst, &gShm->cap[(uint32_t)((r + i) & SL3_RING_MASK) * kChannels], kBytesPerFrame);
            else memset(dst, 0, kBytesPerFrame);
        }
        uint64_t consumed = avail < inIOBufferFrameSize ? avail : inIOBufferFrameSize;
        gShm->cap_read.store(r + consumed, std::memory_order_release);
    } else if (inOperationID == kAudioServerPlugInIOOperationWriteMix) {
        // app -> playback ring (daemon consumer).
        if (!gShm) return noErr;
        uint64_t wr = gShm->play_write.load(std::memory_order_relaxed);
        for (UInt32 i = 0; i < inIOBufferFrameSize; i++)
            memcpy(&gShm->play[(uint32_t)((wr + i) & SL3_RING_MASK) * kChannels],
                   buf + (size_t)i * kChannels, kBytesPerFrame);
        gShm->play_write.store(wr + inIOBufferFrameSize, std::memory_order_release);
    }
    return noErr;
}

// Property helpers and StartIO/property table continue in SL3Properties.inc
#include "SL3Properties.inc"

// ============================ Entry point =================================
// (SL3_Create above is the factory named by the Info.plist CFPlugInFactories.)
