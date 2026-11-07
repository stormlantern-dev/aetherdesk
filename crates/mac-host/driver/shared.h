// What the plug-in and the daemon share: the Mach service name, the
// messages, and the layout of the ring buffer in shared memory. The daemon's
// side is crates/mac-host/src/speaker.rs; keep the two in step.
#pragma once
#include <mach/mach.h>
#include <stdatomic.h>
#include <stdint.h>

#define AD_SERVICE "nl.stormlantern.auroval.audio"
#define AD_MAGIC 0x41444155u // "ADAU"
#define AD_VERSION 2u
#define AD_RATE 48000
#define AD_CHANNELS 2
// About a third of a second at 48 kHz.
#define AD_RING_FRAMES 16384u
// The microphone: mono, its own ring after the speaker's.
#define AD_MIC_CHANNELS 1
#define AD_MIC_FRAMES 16384u

#define AD_MSG_HELLO 0x4144
#define AD_MSG_MEMORY 0x4145

// The plug-in writes the device's mix here, interleaved float32, at
// ring[sample_time % AD_RING_FRAMES]. `written` is the sample time just past
// the newest frame, `host` the mach_absolute_time() of that write.
struct ad_shared {
    uint32_t magic;
    uint32_t version;
    uint32_t frames;
    uint32_t channels;
    _Atomic uint64_t written;
    _Atomic uint64_t host;
    _Atomic uint64_t cycles;
    // The microphone, the other way: the daemon writes the client's frames
    // at mic[n % AD_MIC_FRAMES] and counts them here; the plug-in reads
    // behind it.
    _Atomic uint64_t mic_written;
    uint64_t reserved[2];
    float data[];
};

// Where the microphone's ring starts in `data`.
#define AD_MIC_OFFSET (AD_RING_FRAMES * AD_CHANNELS)

#define AD_SHARED_BYTES                                                                                     \
    ((sizeof(struct ad_shared) +                                                                            \
      (AD_RING_FRAMES * AD_CHANNELS + AD_MIC_FRAMES * AD_MIC_CHANNELS) * sizeof(float) + 16383) &            \
     ~(size_t)16383)

struct ad_hello {
    mach_msg_header_t header;
};

struct ad_memory {
    mach_msg_header_t header;
    mach_msg_body_t body;
    mach_msg_port_descriptor_t entry;
    uint64_t bytes;
};
