// What the plug-in and the reader share: the Mach service name, the
// messages, and the layout of the ring buffer in shared memory.
#pragma once
#include <mach/mach.h>
#include <stdatomic.h>
#include <stdint.h>

#define AD_SERVICE "nl.stormlantern.auroval.audio"
#define AD_MAGIC 0x41444155u // "ADAU"
#define AD_VERSION 1u
#define AD_RATE 48000
#define AD_CHANNELS 2
// About a third of a second at 48 kHz.
#define AD_RING_FRAMES 16384u

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
    uint64_t reserved[3];
    float data[];
};

#define AD_SHARED_BYTES \
    ((sizeof(struct ad_shared) + AD_RING_FRAMES * AD_CHANNELS * sizeof(float) + 16383) & ~(size_t)16383)

struct ad_hello {
    mach_msg_header_t header;
};

struct ad_memory {
    mach_msg_header_t header;
    mach_msg_body_t body;
    mach_msg_port_descriptor_t entry;
    uint64_t bytes;
};
