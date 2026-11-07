// Spike for #82: measures how long sound takes from an app to the reader.
//
// Plays a short click into the Auroval device once a second, each with
// its own loudness so the reader's log can tell them apart, and notes when
// it handed each click to Core Audio. Afterwards it finds the reader's
// "click" lines in its log and prints the delay per click.
//
//   probe [--clicks N] [--buffer FRAMES] [--log PATH]

#include <AudioToolbox/AudioToolbox.h>
#include <CoreAudio/CoreAudio.h>
#include <mach/mach_time.h>
#include <math.h>
#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

#define RATE 48000
#define CLICK_FRAMES 48
#define MAX_CLICKS 30

struct click {
    float amp;
    uint64_t rendered; // when Core Audio asked us for the buffer with the click
    uint64_t plays;    // when that buffer is due at the device
};

static struct click clicks[MAX_CLICKS];
static _Atomic int done;
static int wanted = 10;
static uint64_t total;
static double ticks_per_frame;
static mach_timebase_info_data_t timebase;

static double ms(int64_t ticks) {
    return (double)ticks * timebase.numer / timebase.denom / 1e6;
}

static float amp_of(int n) {
    return 0.05f + 0.01f * (float)n;
}

static OSStatus render(void *ref, AudioUnitRenderActionFlags *flags, const AudioTimeStamp *ts, UInt32 bus,
                       UInt32 frames, AudioBufferList *data) {
    uint64_t now = mach_absolute_time();
    float *out = data->mBuffers[0].mData;
    for (UInt32 i = 0; i < frames; i++) {
        uint64_t f = total + i;
        float x = 0;
        // Start after half a second, then one click per second.
        if (f >= RATE / 2) {
            uint64_t n = (f - RATE / 2) / RATE, at = (f - RATE / 2) % RATE;
            if ((int)n < wanted && at < CLICK_FRAMES) {
                x = amp_of((int)n);
                if (at == 0) {
                    clicks[n].amp = x;
                    clicks[n].rendered = now;
                    clicks[n].plays = (ts->mFlags & kAudioTimeStampHostTimeValid)
                                          ? ts->mHostTime + (uint64_t)(i * ticks_per_frame)
                                          : 0;
                    atomic_store(&done, (int)n + 1);
                }
            }
        }
        out[2 * i] = out[2 * i + 1] = x;
    }
    total += frames;
    return noErr;
}

static AudioObjectID find_device(void) {
    CFStringRef uid = CFSTR("nl.stormlantern.Auroval.speaker");
    AudioObjectID device = kAudioObjectUnknown;
    UInt32 size = sizeof device;
    AudioObjectPropertyAddress a = {kAudioHardwarePropertyTranslateUIDToDevice, kAudioObjectPropertyScopeGlobal,
                                    kAudioObjectPropertyElementMain};
    AudioObjectGetPropertyData(kAudioObjectSystemObject, &a, sizeof uid, &uid, &size, &device);
    return device;
}

static void check(OSStatus err, const char *what) {
    if (err) {
        fprintf(stderr, "%s failed: %d\n", what, (int)err);
        exit(1);
    }
}

static int by_value(const void *a, const void *b) {
    double x = *(const double *)a, y = *(const double *)b;
    return x < y ? -1 : x > y;
}

int main(int argc, char **argv) {
    const char *log = "/tmp/auroval-audio-spike.log";
    UInt32 buffer = 0;
    for (int i = 1; i + 1 < argc; i += 2) {
        if (!strcmp(argv[i], "--clicks"))
            wanted = atoi(argv[i + 1]);
        else if (!strcmp(argv[i], "--buffer"))
            buffer = (UInt32)atoi(argv[i + 1]);
        else if (!strcmp(argv[i], "--log"))
            log = argv[i + 1];
    }
    if (wanted < 1 || wanted > MAX_CLICKS)
        wanted = 10;
    mach_timebase_info(&timebase);
    ticks_per_frame = 1e9 * timebase.denom / timebase.numer / RATE;

    AudioObjectID device = find_device();
    if (device == kAudioObjectUnknown) {
        fprintf(stderr, "no Auroval device: is the plug-in installed?\n");
        return 1;
    }
    if (buffer) {
        AudioObjectPropertyAddress a = {kAudioDevicePropertyBufferFrameSize, kAudioObjectPropertyScopeGlobal,
                                        kAudioObjectPropertyElementMain};
        check(AudioObjectSetPropertyData(device, &a, 0, NULL, sizeof buffer, &buffer), "setting the buffer size");
    }
    {
        UInt32 frames = 0, size = sizeof frames;
        AudioObjectPropertyAddress a = {kAudioDevicePropertyBufferFrameSize, kAudioObjectPropertyScopeGlobal,
                                        kAudioObjectPropertyElementMain};
        AudioObjectGetPropertyData(device, &a, 0, NULL, &size, &frames);
        printf("device buffer: %u frames (%.1f ms)\n", frames, frames * 1000.0 / RATE);
    }

    AudioComponentDescription desc = {kAudioUnitType_Output, kAudioUnitSubType_HALOutput,
                                      kAudioUnitManufacturer_Apple, 0, 0};
    AudioComponent comp = AudioComponentFindNext(NULL, &desc);
    AudioUnit unit;
    check(AudioComponentInstanceNew(comp, &unit), "opening the output unit");
    check(AudioUnitSetProperty(unit, kAudioOutputUnitProperty_CurrentDevice, kAudioUnitScope_Global, 0, &device,
                               sizeof device),
          "choosing the device");
    AudioStreamBasicDescription f = {RATE, kAudioFormatLinearPCM,
                                     kAudioFormatFlagIsFloat | kAudioFormatFlagsNativeEndian |
                                         kAudioFormatFlagIsPacked,
                                     8, 1, 8, 2, 32, 0};
    check(AudioUnitSetProperty(unit, kAudioUnitProperty_StreamFormat, kAudioUnitScope_Input, 0, &f, sizeof f),
          "setting the format");
    AURenderCallbackStruct cb = {render, NULL};
    check(AudioUnitSetProperty(unit, kAudioUnitProperty_SetRenderCallback, kAudioUnitScope_Input, 0, &cb,
                               sizeof cb),
          "setting the callback");
    check(AudioUnitInitialize(unit), "initialising");
    check(AudioOutputUnitStart(unit), "starting");

    printf("playing %d clicks...\n", wanted);
    while (atomic_load(&done) < wanted)
        usleep(100000);
    usleep(500000);
    AudioOutputUnitStop(unit);
    AudioComponentInstanceDispose(unit);

    // Match the reader's clicks by loudness.
    FILE *in = fopen(log, "r");
    if (!in) {
        fprintf(stderr, "can't read %s\n", log);
        return 1;
    }
    uint64_t seen[MAX_CLICKS] = {0};
    char line[256];
    while (fgets(line, sizeof line, in)) {
        float amp;
        unsigned long long t;
        if (sscanf(line, "click amp=%f seen=%llu", &amp, &t) != 2)
            continue;
        for (int n = 0; n < wanted; n++)
            if (fabsf(amp - clicks[n].amp) < 0.004f && t >= clicks[n].rendered && t - clicks[n].rendered < 1000000000ull)
                seen[n] = t;
    }
    fclose(in);

    double app[MAX_CLICKS], dev[MAX_CLICKS];
    int found = 0;
    for (int n = 0; n < wanted; n++) {
        if (!seen[n]) {
            printf("click %2d: not seen by the reader\n", n);
            continue;
        }
        app[found] = ms((int64_t)(seen[n] - clicks[n].rendered));
        dev[found] = clicks[n].plays ? ms((int64_t)seen[n] - (int64_t)clicks[n].plays) : 0;
        printf("click %2d: %.2f ms after the app rendered it, %+.2f ms from when it would play\n", n, app[found],
               dev[found]);
        found++;
    }
    if (found) {
        qsort(app, found, sizeof(double), by_value);
        printf("app to reader: min %.2f, median %.2f, max %.2f ms (%d of %d clicks)\n", app[0], app[found / 2],
               app[found - 1], found, wanted);
    }
    return found == wanted ? 0 : 2;
}
