// Spike for #82: the smallest Audio Server plug-in that shows up as an
// output device, "Auroval", and hands what apps play to another process.
//
// coreaudiod loads it from /Library/Audio/Plug-Ins/HAL/Auroval.driver.
// The device has one stereo float32 output stream at 48 kHz and keeps time
// with the host clock. On StartIO it looks up the reader's Mach service
// (allowed by AudioServerPlugIn_MachServices in Info.plist), gets a memory
// entry back and maps it; WriteMix copies each cycle's mix into that ring.
// Without a reader the device still runs; the sound just goes nowhere.

#include <CoreAudio/AudioServerPlugIn.h>
#include <mach/mach_time.h>
#include <mach/mach_vm.h>
#include <os/log.h>
#include <pthread.h>
#include <servers/bootstrap.h>
#include <string.h>

#include "shared.h"

enum {
    kPlugIn = kAudioObjectPlugInObject,
    kDevice = 2,
    kStream = 3,
};

#define kDeviceUID "nl.stormlantern.Auroval.speaker"
#define kModelUID "nl.stormlantern.Auroval"
// Frames between two zero time stamps.
#define kZeroPeriod 16384u

static os_log_t gLog;
static AudioServerPlugInHostRef gHost;
static UInt32 gRefCount;
static pthread_mutex_t gLock = PTHREAD_MUTEX_INITIALIZER;

static UInt32 gIOClients;
static Boolean gStreamActive = true;
static Float64 gTicksPerFrame;
static UInt64 gAnchor;
static UInt64 gPeriods;

static mach_port_t gServer = MACH_PORT_NULL;
static _Atomic(struct ad_shared *) gShared;

static const AudioStreamBasicDescription kFormat = {
    .mSampleRate = AD_RATE,
    .mFormatID = kAudioFormatLinearPCM,
    .mFormatFlags = kAudioFormatFlagIsFloat | kAudioFormatFlagsNativeEndian | kAudioFormatFlagIsPacked,
    .mBytesPerPacket = 4 * AD_CHANNELS,
    .mFramesPerPacket = 1,
    .mBytesPerFrame = 4 * AD_CHANNELS,
    .mChannelsPerFrame = AD_CHANNELS,
    .mBitsPerChannel = 32,
};

// --- The reader ---------------------------------------------------------

// Asks the reader for the ring and maps it. Called outside the IO thread.
static void connect_reader(void) {
    if (gServer != MACH_PORT_NULL) {
        mach_port_type_t type = 0;
        if (mach_port_type(mach_task_self(), gServer, &type) == KERN_SUCCESS &&
            !(type & MACH_PORT_TYPE_DEAD_NAME) && atomic_load(&gShared) != NULL)
            return;
        mach_port_deallocate(mach_task_self(), gServer);
        gServer = MACH_PORT_NULL;
    }

    mach_port_t server = MACH_PORT_NULL;
    kern_return_t kr = bootstrap_look_up(bootstrap_port, AD_SERVICE, &server);
    if (kr != KERN_SUCCESS) {
        os_log(gLog, "no reader: bootstrap_look_up failed (%{public}d)", kr);
        return;
    }
    mach_port_t reply = MACH_PORT_NULL;
    mach_port_allocate(mach_task_self(), MACH_PORT_RIGHT_RECEIVE, &reply);

    union {
        struct ad_hello hello;
        struct {
            struct ad_memory memory;
            mach_msg_trailer_t trailer;
        } in;
    } msg;
    memset(&msg, 0, sizeof msg);
    msg.hello.header.msgh_bits = MACH_MSGH_BITS(MACH_MSG_TYPE_COPY_SEND, MACH_MSG_TYPE_MAKE_SEND_ONCE);
    msg.hello.header.msgh_size = sizeof msg.hello;
    msg.hello.header.msgh_remote_port = server;
    msg.hello.header.msgh_local_port = reply;
    msg.hello.header.msgh_id = AD_MSG_HELLO;
    kr = mach_msg(&msg.hello.header, MACH_SEND_MSG | MACH_RCV_MSG | MACH_SEND_TIMEOUT | MACH_RCV_TIMEOUT,
                  sizeof msg.hello, sizeof msg, reply, 500, MACH_PORT_NULL);
    mach_port_mod_refs(mach_task_self(), reply, MACH_PORT_RIGHT_RECEIVE, -1);
    if (kr != KERN_SUCCESS || msg.in.memory.header.msgh_id != AD_MSG_MEMORY ||
        !(msg.in.memory.header.msgh_bits & MACH_MSGH_BITS_COMPLEX) ||
        msg.in.memory.bytes != AD_SHARED_BYTES) {
        os_log(gLog, "no reader: hello failed (%{public}d)", kr);
        mach_port_deallocate(mach_task_self(), server);
        return;
    }

    mach_port_t entry = msg.in.memory.entry.name;
    mach_vm_address_t address = 0;
    kr = mach_vm_map(mach_task_self(), &address, AD_SHARED_BYTES, 0, VM_FLAGS_ANYWHERE, entry, 0, false,
                     VM_PROT_READ | VM_PROT_WRITE, VM_PROT_READ | VM_PROT_WRITE, VM_INHERIT_NONE);
    mach_port_deallocate(mach_task_self(), entry);
    if (kr != KERN_SUCCESS) {
        os_log(gLog, "no reader: mapping the ring failed (%{public}d)", kr);
        mach_port_deallocate(mach_task_self(), server);
        return;
    }
    struct ad_shared *shared = (struct ad_shared *)address;
    if (shared->magic != AD_MAGIC || shared->version != AD_VERSION) {
        os_log(gLog, "no reader: unexpected ring header");
        mach_vm_deallocate(mach_task_self(), address, AD_SHARED_BYTES);
        mach_port_deallocate(mach_task_self(), server);
        return;
    }
    // Only called while no IO runs, so nothing uses the old ring any more.
    struct ad_shared *old = atomic_exchange(&gShared, shared);
    if (old)
        mach_vm_deallocate(mach_task_self(), (mach_vm_address_t)old, AD_SHARED_BYTES);
    gServer = server;
    os_log(gLog, "connected to the reader");
}

// --- Properties ---------------------------------------------------------

#define ANSWER(type, value)                                                                        \
    do {                                                                                           \
        *outSize = sizeof(type);                                                                   \
        if (out) {                                                                                 \
            if (inSize < sizeof(type))                                                             \
                return kAudioHardwareBadPropertySizeError;                                         \
            *(type *)out = (value);                                                                \
        }                                                                                          \
        return noErr;                                                                              \
    } while (0)

static OSStatus answer_ids(const AudioObjectID *ids, UInt32 count, UInt32 inSize, UInt32 *outSize,
                           void *out) {
    if (out) {
        UInt32 n = inSize / sizeof(AudioObjectID);
        if (n > count)
            n = count;
        memcpy(out, ids, n * sizeof(AudioObjectID));
        *outSize = n * sizeof(AudioObjectID);
    } else {
        *outSize = count * sizeof(AudioObjectID);
    }
    return noErr;
}

// One place for every property: with `out` NULL it only reports the size.
static OSStatus property(AudioObjectID object, const AudioObjectPropertyAddress *a, UInt32 qualifierSize,
                         const void *qualifier, UInt32 inSize, UInt32 *outSize, void *out) {
    AudioObjectPropertySelector s = a->mSelector;
    Boolean input = a->mScope == kAudioObjectPropertyScopeInput;

    switch (object) {
    case kPlugIn:
        switch (s) {
        case kAudioObjectPropertyBaseClass:
            ANSWER(AudioClassID, kAudioObjectClassID);
        case kAudioObjectPropertyClass:
            ANSWER(AudioClassID, kAudioPlugInClassID);
        case kAudioObjectPropertyOwner:
            ANSWER(AudioObjectID, kAudioObjectUnknown);
        case kAudioObjectPropertyManufacturer:
            ANSWER(CFStringRef, CFSTR("Stormlantern"));
        case kAudioObjectPropertyOwnedObjects:
        case kAudioPlugInPropertyDeviceList: {
            AudioObjectID ids[] = {kDevice};
            return answer_ids(ids, 1, inSize, outSize, out);
        }
        case kAudioPlugInPropertyTranslateUIDToDevice: {
            AudioObjectID found = kAudioObjectUnknown;
            if (out && qualifierSize == sizeof(CFStringRef) && qualifier &&
                CFStringCompare(*(CFStringRef *)qualifier, CFSTR(kDeviceUID), 0) == kCFCompareEqualTo)
                found = kDevice;
            ANSWER(AudioObjectID, found);
        }
        case kAudioPlugInPropertyResourceBundle:
            ANSWER(CFStringRef, CFSTR(""));
        }
        break;

    case kDevice:
        switch (s) {
        case kAudioObjectPropertyBaseClass:
            ANSWER(AudioClassID, kAudioObjectClassID);
        case kAudioObjectPropertyClass:
            ANSWER(AudioClassID, kAudioDeviceClassID);
        case kAudioObjectPropertyOwner:
            ANSWER(AudioObjectID, kPlugIn);
        case kAudioObjectPropertyName:
            ANSWER(CFStringRef, CFSTR("Auroval"));
        case kAudioObjectPropertyManufacturer:
            ANSWER(CFStringRef, CFSTR("Stormlantern"));
        case kAudioObjectPropertyOwnedObjects:
        case kAudioDevicePropertyStreams: {
            AudioObjectID ids[] = {kStream};
            return answer_ids(ids, input ? 0 : 1, inSize, outSize, out);
        }
        case kAudioObjectPropertyControlList:
            return answer_ids(NULL, 0, inSize, outSize, out);
        case kAudioDevicePropertyDeviceUID:
            ANSWER(CFStringRef, CFSTR(kDeviceUID));
        case kAudioDevicePropertyModelUID:
            ANSWER(CFStringRef, CFSTR(kModelUID));
        case kAudioDevicePropertyTransportType:
            ANSWER(UInt32, kAudioDeviceTransportTypeVirtual);
        case kAudioDevicePropertyRelatedDevices: {
            AudioObjectID ids[] = {kDevice};
            return answer_ids(ids, 1, inSize, outSize, out);
        }
        case kAudioDevicePropertyClockDomain:
            ANSWER(UInt32, 0);
        case kAudioDevicePropertyDeviceIsAlive:
            ANSWER(UInt32, 1);
        case kAudioDevicePropertyDeviceIsRunning: {
            pthread_mutex_lock(&gLock);
            UInt32 running = gIOClients > 0;
            pthread_mutex_unlock(&gLock);
            ANSWER(UInt32, running);
        }
        case kAudioDevicePropertyDeviceCanBeDefaultDevice:
        case kAudioDevicePropertyDeviceCanBeDefaultSystemDevice:
            ANSWER(UInt32, input ? 0 : 1);
        case kAudioDevicePropertyLatency:
        case kAudioDevicePropertySafetyOffset:
            ANSWER(UInt32, 0);
        case kAudioDevicePropertyNominalSampleRate:
            ANSWER(Float64, AD_RATE);
        case kAudioDevicePropertyAvailableNominalSampleRates: {
            AudioValueRange range = {AD_RATE, AD_RATE};
            ANSWER(AudioValueRange, range);
        }
        case kAudioDevicePropertyIsHidden:
            ANSWER(UInt32, 0);
        case kAudioDevicePropertyZeroTimeStampPeriod:
            ANSWER(UInt32, kZeroPeriod);
        case kAudioDevicePropertyPreferredChannelsForStereo: {
            if (input)
                break;
            *outSize = 2 * sizeof(UInt32);
            if (out) {
                if (inSize < *outSize)
                    return kAudioHardwareBadPropertySizeError;
                ((UInt32 *)out)[0] = 1;
                ((UInt32 *)out)[1] = 2;
            }
            return noErr;
        }
        }
        break;

    case kStream:
        switch (s) {
        case kAudioObjectPropertyBaseClass:
            ANSWER(AudioClassID, kAudioObjectClassID);
        case kAudioObjectPropertyClass:
            ANSWER(AudioClassID, kAudioStreamClassID);
        case kAudioObjectPropertyOwner:
            ANSWER(AudioObjectID, kDevice);
        case kAudioObjectPropertyOwnedObjects:
            return answer_ids(NULL, 0, inSize, outSize, out);
        case kAudioObjectPropertyName:
            ANSWER(CFStringRef, CFSTR("Auroval speaker"));
        case kAudioStreamPropertyIsActive: {
            pthread_mutex_lock(&gLock);
            UInt32 active = gStreamActive;
            pthread_mutex_unlock(&gLock);
            ANSWER(UInt32, active);
        }
        case kAudioStreamPropertyDirection:
            ANSWER(UInt32, 0); // output
        case kAudioStreamPropertyTerminalType:
            ANSWER(UInt32, kAudioStreamTerminalTypeSpeaker);
        case kAudioStreamPropertyStartingChannel:
            ANSWER(UInt32, 1);
        case kAudioStreamPropertyLatency:
            ANSWER(UInt32, 0);
        case kAudioStreamPropertyVirtualFormat:
        case kAudioStreamPropertyPhysicalFormat:
            ANSWER(AudioStreamBasicDescription, kFormat);
        case kAudioStreamPropertyAvailableVirtualFormats:
        case kAudioStreamPropertyAvailablePhysicalFormats: {
            AudioStreamRangedDescription d = {kFormat, {AD_RATE, AD_RATE}};
            ANSWER(AudioStreamRangedDescription, d);
        }
        }
        break;

    default:
        return kAudioHardwareBadObjectError;
    }
    return kAudioHardwareUnknownPropertyError;
}

// --- The driver interface -----------------------------------------------

static AudioServerPlugInDriverRef gDriverRef;

static HRESULT QueryInterface(void *driver, REFIID uuid, LPVOID *outInterface) {
    if (driver != gDriverRef || !outInterface)
        return kAudioHardwareIllegalOperationError;
    CFUUIDRef asked = CFUUIDCreateFromUUIDBytes(NULL, uuid);
    Boolean known = CFEqual(asked, IUnknownUUID) || CFEqual(asked, kAudioServerPlugInDriverInterfaceUUID);
    CFRelease(asked);
    if (!known)
        return E_NOINTERFACE;
    pthread_mutex_lock(&gLock);
    gRefCount++;
    pthread_mutex_unlock(&gLock);
    *outInterface = gDriverRef;
    return S_OK;
}

static ULONG AddRef(void *driver) {
    pthread_mutex_lock(&gLock);
    ULONG n = ++gRefCount;
    pthread_mutex_unlock(&gLock);
    return n;
}

static ULONG Release(void *driver) {
    pthread_mutex_lock(&gLock);
    ULONG n = gRefCount > 0 ? --gRefCount : 0;
    pthread_mutex_unlock(&gLock);
    return n;
}

static OSStatus Initialize(AudioServerPlugInDriverRef driver, AudioServerPlugInHostRef host) {
    gHost = host;
    mach_timebase_info_data_t tb;
    mach_timebase_info(&tb);
    Float64 ticksPerSecond = 1e9 * tb.denom / tb.numer;
    gTicksPerFrame = ticksPerSecond / AD_RATE;
    os_log(gLog, "loaded");
    return noErr;
}

static OSStatus CreateDevice(AudioServerPlugInDriverRef d, CFDictionaryRef desc,
                             const AudioServerPlugInClientInfo *client, AudioObjectID *out) {
    return kAudioHardwareUnsupportedOperationError;
}

static OSStatus DestroyDevice(AudioServerPlugInDriverRef d, AudioObjectID device) {
    return kAudioHardwareUnsupportedOperationError;
}

static OSStatus AddDeviceClient(AudioServerPlugInDriverRef d, AudioObjectID device,
                                const AudioServerPlugInClientInfo *client) {
    return noErr;
}

static OSStatus RemoveDeviceClient(AudioServerPlugInDriverRef d, AudioObjectID device,
                                   const AudioServerPlugInClientInfo *client) {
    return noErr;
}

static OSStatus PerformDeviceConfigurationChange(AudioServerPlugInDriverRef d, AudioObjectID device,
                                                 UInt64 action, void *info) {
    return noErr;
}

static OSStatus AbortDeviceConfigurationChange(AudioServerPlugInDriverRef d, AudioObjectID device,
                                               UInt64 action, void *info) {
    return noErr;
}

static Boolean HasProperty(AudioServerPlugInDriverRef d, AudioObjectID object, pid_t pid,
                           const AudioObjectPropertyAddress *a) {
    UInt32 size = 0;
    return property(object, a, 0, NULL, 0, &size, NULL) == noErr;
}

static OSStatus IsPropertySettable(AudioServerPlugInDriverRef d, AudioObjectID object, pid_t pid,
                                   const AudioObjectPropertyAddress *a, Boolean *out) {
    UInt32 size = 0;
    OSStatus err = property(object, a, 0, NULL, 0, &size, NULL);
    if (err)
        return err;
    *out = object == kStream &&
           (a->mSelector == kAudioStreamPropertyIsActive || a->mSelector == kAudioStreamPropertyVirtualFormat ||
            a->mSelector == kAudioStreamPropertyPhysicalFormat);
    return noErr;
}

static OSStatus GetPropertyDataSize(AudioServerPlugInDriverRef d, AudioObjectID object, pid_t pid,
                                    const AudioObjectPropertyAddress *a, UInt32 qualifierSize,
                                    const void *qualifier, UInt32 *outSize) {
    return property(object, a, qualifierSize, qualifier, 0, outSize, NULL);
}

static OSStatus GetPropertyData(AudioServerPlugInDriverRef d, AudioObjectID object, pid_t pid,
                                const AudioObjectPropertyAddress *a, UInt32 qualifierSize, const void *qualifier,
                                UInt32 inSize, UInt32 *outSize, void *out) {
    return property(object, a, qualifierSize, qualifier, inSize, outSize, out);
}

static OSStatus SetPropertyData(AudioServerPlugInDriverRef d, AudioObjectID object, pid_t pid,
                                const AudioObjectPropertyAddress *a, UInt32 qualifierSize, const void *qualifier,
                                UInt32 inSize, const void *data) {
    if (object != kStream)
        return kAudioHardwareUnknownPropertyError;
    switch (a->mSelector) {
    case kAudioStreamPropertyIsActive:
        if (inSize != sizeof(UInt32))
            return kAudioHardwareBadPropertySizeError;
        pthread_mutex_lock(&gLock);
        gStreamActive = *(const UInt32 *)data != 0;
        pthread_mutex_unlock(&gLock);
        return noErr;
    case kAudioStreamPropertyVirtualFormat:
    case kAudioStreamPropertyPhysicalFormat: {
        if (inSize != sizeof(AudioStreamBasicDescription))
            return kAudioHardwareBadPropertySizeError;
        const AudioStreamBasicDescription *f = data;
        if (f->mSampleRate != AD_RATE || f->mFormatID != kFormat.mFormatID ||
            f->mChannelsPerFrame != AD_CHANNELS || f->mBitsPerChannel != 32 ||
            !(f->mFormatFlags & kAudioFormatFlagIsFloat))
            return kAudioDeviceUnsupportedFormatError;
        return noErr;
    }
    }
    return kAudioHardwareUnknownPropertyError;
}

static OSStatus StartIO(AudioServerPlugInDriverRef d, AudioObjectID device, UInt32 client) {
    pthread_mutex_lock(&gLock);
    if (gIOClients == 0) {
        connect_reader();
        gAnchor = mach_absolute_time();
        gPeriods = 0;
        os_log(gLog, "IO started");
    }
    gIOClients++;
    pthread_mutex_unlock(&gLock);
    return noErr;
}

static OSStatus StopIO(AudioServerPlugInDriverRef d, AudioObjectID device, UInt32 client) {
    pthread_mutex_lock(&gLock);
    if (gIOClients > 0 && --gIOClients == 0)
        os_log(gLog, "IO stopped");
    pthread_mutex_unlock(&gLock);
    return noErr;
}

// The device's clock is the host clock: one zero time stamp per kZeroPeriod
// frames since IO started.
static OSStatus GetZeroTimeStamp(AudioServerPlugInDriverRef d, AudioObjectID device, UInt32 client,
                                 Float64 *outSampleTime, UInt64 *outHostTime, UInt64 *outSeed) {
    Float64 ticksPerPeriod = gTicksPerFrame * kZeroPeriod;
    UInt64 now = mach_absolute_time();
    UInt64 next = gAnchor + (UInt64)((gPeriods + 1) * ticksPerPeriod);
    if (next <= now)
        gPeriods++;
    *outSampleTime = (Float64)(gPeriods * kZeroPeriod);
    *outHostTime = gAnchor + (UInt64)(gPeriods * ticksPerPeriod);
    *outSeed = 1;
    return noErr;
}

static OSStatus WillDoIOOperation(AudioServerPlugInDriverRef d, AudioObjectID device, UInt32 client,
                                  UInt32 op, Boolean *outWillDo, Boolean *outInPlace) {
    *outWillDo = op == kAudioServerPlugInIOOperationWriteMix;
    *outInPlace = true;
    return noErr;
}

static OSStatus BeginIOOperation(AudioServerPlugInDriverRef d, AudioObjectID device, UInt32 client, UInt32 op,
                                 UInt32 frames, const AudioServerPlugInIOCycleInfo *cycle) {
    return noErr;
}

static OSStatus DoIOOperation(AudioServerPlugInDriverRef d, AudioObjectID device, AudioObjectID stream,
                              UInt32 client, UInt32 op, UInt32 frames, const AudioServerPlugInIOCycleInfo *cycle,
                              void *main, void *secondary) {
    if (op != kAudioServerPlugInIOOperationWriteMix || !main)
        return noErr;
    struct ad_shared *shared = atomic_load_explicit(&gShared, memory_order_acquire);
    if (!shared)
        return noErr;
    UInt64 start = (UInt64)cycle->mOutputTime.mSampleTime;
    const float *src = main;
    UInt32 pos = (UInt32)(start % AD_RING_FRAMES);
    UInt32 first = AD_RING_FRAMES - pos;
    if (first > frames)
        first = frames;
    memcpy(shared->data + pos * AD_CHANNELS, src, first * AD_CHANNELS * sizeof(float));
    if (frames > first)
        memcpy(shared->data, src + first * AD_CHANNELS, (frames - first) * AD_CHANNELS * sizeof(float));
    atomic_store_explicit(&shared->host, mach_absolute_time(), memory_order_relaxed);
    atomic_fetch_add_explicit(&shared->cycles, 1, memory_order_relaxed);
    atomic_store_explicit(&shared->written, start + frames, memory_order_release);
    return noErr;
}

static OSStatus EndIOOperation(AudioServerPlugInDriverRef d, AudioObjectID device, UInt32 client, UInt32 op,
                               UInt32 frames, const AudioServerPlugInIOCycleInfo *cycle) {
    return noErr;
}

static AudioServerPlugInDriverInterface gInterface = {
    ._reserved = NULL,
    .QueryInterface = QueryInterface,
    .AddRef = AddRef,
    .Release = Release,
    .Initialize = Initialize,
    .CreateDevice = CreateDevice,
    .DestroyDevice = DestroyDevice,
    .AddDeviceClient = AddDeviceClient,
    .RemoveDeviceClient = RemoveDeviceClient,
    .PerformDeviceConfigurationChange = PerformDeviceConfigurationChange,
    .AbortDeviceConfigurationChange = AbortDeviceConfigurationChange,
    .HasProperty = HasProperty,
    .IsPropertySettable = IsPropertySettable,
    .GetPropertyDataSize = GetPropertyDataSize,
    .GetPropertyData = GetPropertyData,
    .SetPropertyData = SetPropertyData,
    .StartIO = StartIO,
    .StopIO = StopIO,
    .GetZeroTimeStamp = GetZeroTimeStamp,
    .WillDoIOOperation = WillDoIOOperation,
    .BeginIOOperation = BeginIOOperation,
    .DoIOOperation = DoIOOperation,
    .EndIOOperation = EndIOOperation,
};
static AudioServerPlugInDriverInterface *gInterfacePtr = &gInterface;

// Named in Info.plist under CFPlugInFactories.
__attribute__((visibility("default"))) void *Auroval_Create(CFAllocatorRef allocator, CFUUIDRef type) {
    if (!CFEqual(type, kAudioServerPlugInTypeUUID))
        return NULL;
    gLog = os_log_create("nl.stormlantern.auroval", "driver");
    gDriverRef = &gInterfacePtr;
    return gDriverRef;
}
