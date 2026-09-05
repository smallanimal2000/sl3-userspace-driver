// SL3Properties.inc — AudioObject property implementation for the SL3 plugin.
// Included from SL3PlugIn.cpp. One core `prop()` serves size + data queries so
// the supported-selector set is defined once; HasProperty delegates to it.

static void FillASBD(AudioStreamBasicDescription *f, Float64 rate) {
    f->mSampleRate       = rate;
    f->mFormatID         = kAudioFormatLinearPCM;
    f->mFormatFlags      = kAudioFormatFlagIsFloat | kAudioFormatFlagIsPacked;
    f->mBytesPerPacket   = kBytesPerFrame;
    f->mFramesPerPacket  = 1;
    f->mBytesPerFrame    = kBytesPerFrame;
    f->mChannelsPerFrame = kChannels;
    f->mBitsPerChannel   = 32;
    f->mReserved         = 0;
}

// Core property accessor. If outData==NULL this is a size-only query.
static OSStatus prop(AudioObjectID obj, pid_t, const AudioObjectPropertyAddress *a,
                     UInt32 /*qualSize*/, const void *qual,
                     UInt32 inDataSize, UInt32 *outSize, void *outData) {
    #define OUT(sz)  do { *outSize = (UInt32)(sz); } while (0)
    #define NEED(sz) do { if (outData && inDataSize < (UInt32)(sz)) return kAudioHardwareBadPropertySizeError; } while (0)

    switch (obj) {
    // ------------------------------------------------------------------ plugin
    case kObjectID_PlugIn:
        switch (a->mSelector) {
        case kAudioObjectPropertyBaseClass:
            OUT(sizeof(AudioClassID)); NEED(sizeof(AudioClassID));
            if (outData) *(AudioClassID *)outData = kAudioObjectClassID; return noErr;
        case kAudioObjectPropertyClass:
            OUT(sizeof(AudioClassID)); NEED(sizeof(AudioClassID));
            if (outData) *(AudioClassID *)outData = kAudioPlugInClassID; return noErr;
        case kAudioObjectPropertyOwner:
            OUT(sizeof(AudioObjectID)); NEED(sizeof(AudioObjectID));
            if (outData) *(AudioObjectID *)outData = kAudioObjectUnknown; return noErr;
        case kAudioObjectPropertyManufacturer:
            OUT(sizeof(CFStringRef)); NEED(sizeof(CFStringRef));
            if (outData) *(CFStringRef *)outData = CFSTR("Rane Corporation"); return noErr;
        case kAudioObjectPropertyOwnedObjects:
        case kAudioPlugInPropertyDeviceList: {
            UInt32 n = 1; OUT(n * sizeof(AudioObjectID));
            if (outData) { UInt32 room = inDataSize / sizeof(AudioObjectID);
                if (room >= 1) ((AudioObjectID *)outData)[0] = kObjectID_Device;
                OUT((room >= 1 ? 1 : 0) * sizeof(AudioObjectID)); }
            return noErr; }
        case kAudioPlugInPropertyTranslateUIDToDevice:
            OUT(sizeof(AudioObjectID)); NEED(sizeof(AudioObjectID));
            if (outData) {
                CFStringRef uid = qual ? *(CFStringRef *)qual : NULL;
                *(AudioObjectID *)outData =
                    (uid && CFStringCompare(uid, CFSTR(kDevice_UID), 0) == kCFCompareEqualTo)
                        ? kObjectID_Device : kAudioObjectUnknown;
            }
            return noErr;
        case kAudioPlugInPropertyResourceBundle:
            OUT(sizeof(CFStringRef)); NEED(sizeof(CFStringRef));
            if (outData) *(CFStringRef *)outData = CFSTR(""); return noErr;
        }
        break;

    // ------------------------------------------------------------------ device
    case kObjectID_Device:
        switch (a->mSelector) {
        case kAudioObjectPropertyBaseClass:
            OUT(sizeof(AudioClassID)); NEED(sizeof(AudioClassID));
            if (outData) *(AudioClassID *)outData = kAudioObjectClassID; return noErr;
        case kAudioObjectPropertyClass:
            OUT(sizeof(AudioClassID)); NEED(sizeof(AudioClassID));
            if (outData) *(AudioClassID *)outData = kAudioDeviceClassID; return noErr;
        case kAudioObjectPropertyOwner:
            OUT(sizeof(AudioObjectID)); NEED(sizeof(AudioObjectID));
            if (outData) *(AudioObjectID *)outData = kObjectID_PlugIn; return noErr;
        case kAudioObjectPropertyName:
            OUT(sizeof(CFStringRef)); NEED(sizeof(CFStringRef));
            if (outData) *(CFStringRef *)outData = CFSTR("Rane SL 3"); return noErr;
        case kAudioObjectPropertyManufacturer:
            OUT(sizeof(CFStringRef)); NEED(sizeof(CFStringRef));
            if (outData) *(CFStringRef *)outData = CFSTR("Rane Corporation"); return noErr;
        case kAudioObjectPropertyOwnedObjects:
        case kAudioDevicePropertyStreams: {
            // scope selects direction; global returns both
            AudioObjectID ids[2]; UInt32 n = 0;
            bool wantIn  = a->mScope == kAudioObjectPropertyScopeGlobal || a->mScope == kAudioObjectPropertyScopeInput;
            bool wantOut = a->mScope == kAudioObjectPropertyScopeGlobal || a->mScope == kAudioObjectPropertyScopeOutput;
            if (a->mSelector == kAudioObjectPropertyOwnedObjects) { wantIn = wantOut = true; }
            if (wantIn)  ids[n++] = kObjectID_Stream_Input;
            if (wantOut) ids[n++] = kObjectID_Stream_Output;
            OUT(n * sizeof(AudioObjectID));
            if (outData) { UInt32 room = inDataSize / sizeof(AudioObjectID);
                UInt32 c = room < n ? room : n;
                for (UInt32 i = 0; i < c; i++) ((AudioObjectID *)outData)[i] = ids[i];
                OUT(c * sizeof(AudioObjectID)); }
            return noErr; }
        case kAudioObjectPropertyControlList:
            OUT(0); return noErr;
        case kAudioDevicePropertyDeviceUID:
            OUT(sizeof(CFStringRef)); NEED(sizeof(CFStringRef));
            if (outData) *(CFStringRef *)outData = CFSTR(kDevice_UID); return noErr;
        case kAudioDevicePropertyModelUID:
            OUT(sizeof(CFStringRef)); NEED(sizeof(CFStringRef));
            if (outData) *(CFStringRef *)outData = CFSTR(kDevice_ModelUID); return noErr;
        case kAudioDevicePropertyTransportType:
            OUT(sizeof(UInt32)); NEED(sizeof(UInt32));
            if (outData) *(UInt32 *)outData = kAudioDeviceTransportTypeUSB; return noErr;
        case kAudioDevicePropertyRelatedDevices: {
            OUT(sizeof(AudioObjectID));
            if (outData && inDataSize >= sizeof(AudioObjectID)) { *(AudioObjectID *)outData = kObjectID_Device; OUT(sizeof(AudioObjectID)); }
            return noErr; }
        case kAudioDevicePropertyClockDomain:
            OUT(sizeof(UInt32)); NEED(sizeof(UInt32));
            if (outData) *(UInt32 *)outData = 0; return noErr;
        case kAudioDevicePropertyDeviceIsAlive:
            OUT(sizeof(UInt32)); NEED(sizeof(UInt32));
            if (outData) *(UInt32 *)outData = 1; return noErr;
        case kAudioDevicePropertyDeviceIsRunning:
            OUT(sizeof(UInt32)); NEED(sizeof(UInt32));
            if (outData) *(UInt32 *)outData = (gIORunning > 0) ? 1 : 0; return noErr;
        case kAudioDevicePropertyDeviceCanBeDefaultDevice:
        case kAudioDevicePropertyDeviceCanBeDefaultSystemDevice:
            OUT(sizeof(UInt32)); NEED(sizeof(UInt32));
            if (outData) *(UInt32 *)outData = 1; return noErr;
        case kAudioDevicePropertyLatency:
            OUT(sizeof(UInt32)); NEED(sizeof(UInt32));
            if (outData) *(UInt32 *)outData = 0; return noErr;
        case kAudioDevicePropertySafetyOffset:
            OUT(sizeof(UInt32)); NEED(sizeof(UInt32));
            if (outData) *(UInt32 *)outData = 0; return noErr;
        case kAudioDevicePropertyNominalSampleRate:
            OUT(sizeof(Float64)); NEED(sizeof(Float64));
            if (outData) *(Float64 *)outData = gSampleRate; return noErr;
        case kAudioDevicePropertyAvailableNominalSampleRates: {
            static const Float64 rates[2] = { 44100.0, 48000.0 };
            OUT(2 * sizeof(AudioValueRange));
            if (outData) { UInt32 room = inDataSize / sizeof(AudioValueRange);
                UInt32 c = room < 2 ? room : 2;
                AudioValueRange *vr = (AudioValueRange *)outData;
                for (UInt32 i = 0; i < c; i++) { vr[i].mMinimum = rates[i]; vr[i].mMaximum = rates[i]; }
                OUT(c * sizeof(AudioValueRange)); }
            return noErr; }
        case kAudioDevicePropertyIsHidden:
            OUT(sizeof(UInt32)); NEED(sizeof(UInt32));
            if (outData) *(UInt32 *)outData = 0; return noErr;
        case kAudioDevicePropertyPreferredChannelsForStereo: {
            OUT(2 * sizeof(UInt32)); NEED(2 * sizeof(UInt32));
            if (outData) { ((UInt32 *)outData)[0] = 1; ((UInt32 *)outData)[1] = 2; }
            return noErr; }
        case kAudioDevicePropertyPreferredChannelLayout: {
            UInt32 sz = offsetof(AudioChannelLayout, mChannelDescriptions) + kChannels * sizeof(AudioChannelDescription);
            OUT(sz); NEED(sz);
            if (outData) { AudioChannelLayout *l = (AudioChannelLayout *)outData;
                memset(l, 0, sz);
                l->mChannelLayoutTag = kAudioChannelLayoutTag_UseChannelDescriptions;
                l->mNumberChannelDescriptions = kChannels;
                for (UInt32 i = 0; i < kChannels; i++) {
                    l->mChannelDescriptions[i].mChannelLabel = kAudioChannelLabel_Discrete_0 + i;
                    l->mChannelDescriptions[i].mChannelFlags = 0; } }
            return noErr; }
        case kAudioDevicePropertyZeroTimeStampPeriod:
            OUT(sizeof(UInt32)); NEED(sizeof(UInt32));
            if (outData) *(UInt32 *)outData = kRingPeriodFrames; return noErr;
        }
        break;

    // ------------------------------------------------------------------ streams
    case kObjectID_Stream_Input:
    case kObjectID_Stream_Output: {
        bool isInput = (obj == kObjectID_Stream_Input);
        switch (a->mSelector) {
        case kAudioObjectPropertyBaseClass:
            OUT(sizeof(AudioClassID)); NEED(sizeof(AudioClassID));
            if (outData) *(AudioClassID *)outData = kAudioObjectClassID; return noErr;
        case kAudioObjectPropertyClass:
            OUT(sizeof(AudioClassID)); NEED(sizeof(AudioClassID));
            if (outData) *(AudioClassID *)outData = kAudioStreamClassID; return noErr;
        case kAudioObjectPropertyOwner:
            OUT(sizeof(AudioObjectID)); NEED(sizeof(AudioObjectID));
            if (outData) *(AudioObjectID *)outData = kObjectID_Device; return noErr;
        case kAudioObjectPropertyOwnedObjects:
            OUT(0); return noErr;
        case kAudioStreamPropertyDirection:
            OUT(sizeof(UInt32)); NEED(sizeof(UInt32));
            if (outData) *(UInt32 *)outData = isInput ? 1 : 0; return noErr;
        case kAudioStreamPropertyTerminalType:
            OUT(sizeof(UInt32)); NEED(sizeof(UInt32));
            if (outData) *(UInt32 *)outData = isInput ? kAudioStreamTerminalTypeLine : kAudioStreamTerminalTypeLine;
            return noErr;
        case kAudioStreamPropertyStartingChannel:
            OUT(sizeof(UInt32)); NEED(sizeof(UInt32));
            if (outData) *(UInt32 *)outData = 1; return noErr;
        case kAudioStreamPropertyLatency:
            OUT(sizeof(UInt32)); NEED(sizeof(UInt32));
            if (outData) *(UInt32 *)outData = 0; return noErr;
        case kAudioStreamPropertyIsActive:
            OUT(sizeof(UInt32)); NEED(sizeof(UInt32));
            if (outData) *(UInt32 *)outData = (isInput ? gInputActive : gOutputActive) ? 1 : 0; return noErr;
        case kAudioStreamPropertyVirtualFormat:
        case kAudioStreamPropertyPhysicalFormat:
            OUT(sizeof(AudioStreamBasicDescription)); NEED(sizeof(AudioStreamBasicDescription));
            if (outData) FillASBD((AudioStreamBasicDescription *)outData, gSampleRate); return noErr;
        case kAudioStreamPropertyAvailableVirtualFormats:
        case kAudioStreamPropertyAvailablePhysicalFormats: {
            static const Float64 rates[2] = { 44100.0, 48000.0 };
            OUT(2 * sizeof(AudioStreamRangedDescription));
            if (outData) { UInt32 room = inDataSize / sizeof(AudioStreamRangedDescription);
                UInt32 c = room < 2 ? room : 2;
                AudioStreamRangedDescription *d = (AudioStreamRangedDescription *)outData;
                for (UInt32 i = 0; i < c; i++) { FillASBD(&d[i].mFormat, rates[i]);
                    d[i].mSampleRateRange.mMinimum = rates[i]; d[i].mSampleRateRange.mMaximum = rates[i]; }
                OUT(c * sizeof(AudioStreamRangedDescription)); }
            return noErr; }
        }
        break; }
    }

    #undef OUT
    #undef NEED
    return kAudioHardwareUnknownPropertyError;
}

static Boolean HasProperty(AudioServerPlugInDriverRef, AudioObjectID obj, pid_t pid, const AudioObjectPropertyAddress *a) {
    UInt32 sz = 0;
    return prop(obj, pid, a, 0, NULL, 0, &sz, NULL) == noErr;
}

static OSStatus IsPropertySettable(AudioServerPlugInDriverRef, AudioObjectID obj, pid_t, const AudioObjectPropertyAddress *a, Boolean *outSettable) {
    Boolean s = false;
    if (obj == kObjectID_Device && a->mSelector == kAudioDevicePropertyNominalSampleRate) s = true;
    else if ((obj == kObjectID_Stream_Input || obj == kObjectID_Stream_Output) &&
             (a->mSelector == kAudioStreamPropertyIsActive ||
              a->mSelector == kAudioStreamPropertyVirtualFormat ||
              a->mSelector == kAudioStreamPropertyPhysicalFormat)) s = true;
    *outSettable = s;
    return noErr;
}

static OSStatus GetPropertyDataSize(AudioServerPlugInDriverRef, AudioObjectID obj, pid_t pid, const AudioObjectPropertyAddress *a, UInt32 qs, const void *q, UInt32 *outSize) {
    return prop(obj, pid, a, qs, q, 0, outSize, NULL);
}

static OSStatus GetPropertyData(AudioServerPlugInDriverRef, AudioObjectID obj, pid_t pid, const AudioObjectPropertyAddress *a, UInt32 qs, const void *q, UInt32 inDataSize, UInt32 *outSize, void *outData) {
    return prop(obj, pid, a, qs, q, inDataSize, outSize, outData);
}

static OSStatus SetPropertyData(AudioServerPlugInDriverRef inDriver, AudioObjectID obj, pid_t, const AudioObjectPropertyAddress *a, UInt32, const void *, UInt32 inDataSize, const void *inData) {
    if (obj == kObjectID_Device && a->mSelector == kAudioDevicePropertyNominalSampleRate) {
        if (inDataSize < sizeof(Float64)) return kAudioHardwareBadPropertySizeError;
        Float64 r = *(const Float64 *)inData;
        if (r != 44100.0 && r != 48000.0) return kAudioHardwareIllegalOperationError;
        if (gHost && r != gSampleRate)
            gHost->RequestDeviceConfigurationChange(gHost, kObjectID_Device, (UInt64)r, NULL);
        return noErr;
    }
    if (obj == kObjectID_Stream_Input || obj == kObjectID_Stream_Output) {
        if (a->mSelector == kAudioStreamPropertyIsActive && inDataSize >= sizeof(UInt32)) {
            bool on = *(const UInt32 *)inData != 0;
            if (obj == kObjectID_Stream_Input) gInputActive = on; else gOutputActive = on;
            return noErr;
        }
        if ((a->mSelector == kAudioStreamPropertyVirtualFormat || a->mSelector == kAudioStreamPropertyPhysicalFormat) &&
            inDataSize >= sizeof(AudioStreamBasicDescription)) {
            const AudioStreamBasicDescription *f = (const AudioStreamBasicDescription *)inData;
            if (f->mSampleRate != 44100.0 && f->mSampleRate != 48000.0) return kAudioHardwareIllegalOperationError;
            if (gHost && f->mSampleRate != gSampleRate)
                gHost->RequestDeviceConfigurationChange(gHost, kObjectID_Device, (UInt64)f->mSampleRate, NULL);
            return noErr;
        }
    }
    return kAudioHardwareUnknownPropertyError;
}
