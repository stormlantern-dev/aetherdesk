// Spike for #82: the other end of the Auroval device. Runs as a launchd
// daemon that owns the Mach service the plug-in looks up, hands the plug-in
// a shared ring buffer, and reads the device's sound from it.
//
// It logs once a second whether sound flows (level, cycles), and every
// click the probe plays, with the moment it saw it, so the probe's own times
// give the latency (both use mach_absolute_time).

#include <math.h>
#include <mach/mach_time.h>
#include <mach/mach_vm.h>
#include <pthread.h>
#include <servers/bootstrap.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>

#include "shared.h"

static struct ad_shared *shared;
static mach_timebase_info_data_t timebase;

static double ms(uint64_t ticks) {
    return (double)ticks * timebase.numer / timebase.denom / 1e6;
}

// Follows the plug-in's writes, 1 ms at a time.
static void *read_ring(void *unused) {
    uint64_t pos = 0, last_report = mach_absolute_time(), quiet = 0, cycles_seen = 0;
    double sum = 0;
    uint64_t counted = 0;
    float peak = 0;
    for (;;) {
        usleep(1000);
        uint64_t written = atomic_load_explicit(&shared->written, memory_order_acquire);
        uint64_t now = mach_absolute_time();
        if (written < pos || written - pos > AD_RING_FRAMES / 2) {
            // Started again, or we fell behind: jump to the newest frames.
            pos = written > AD_RING_FRAMES / 4 ? written - AD_RING_FRAMES / 4 : 0;
        }
        for (; pos < written; pos++) {
            const float *frame = shared->data + (pos % AD_RING_FRAMES) * AD_CHANNELS;
            float x = fabsf(frame[0]);
            sum += (double)frame[0] * frame[0];
            counted++;
            if (x > peak)
                peak = x;
            if (x < 0.001f) {
                quiet++;
            } else {
                if (x > 0.01f && quiet >= AD_RATE / 10) {
                    // A click after silence. `ahead` is how far this frame
                    // lies before the newest one written.
                    printf("click amp=%.3f seen=%llu ahead=%.2fms\n", x, now,
                           (double)(written - pos) * 1000.0 / AD_RATE);
                }
                quiet = 0;
            }
        }
        if (now - last_report >= (uint64_t)(1e9 * timebase.denom / timebase.numer)) {
            uint64_t cycles = atomic_load(&shared->cycles);
            if (cycles != cycles_seen)
                printf("sound: %llu cycles, rms %.4f, peak %.3f, last write %.1f ms ago\n",
                       cycles - cycles_seen, counted ? sqrt(sum / counted) : 0.0, peak,
                       ms(now - atomic_load(&shared->host)));
            cycles_seen = cycles;
            sum = 0;
            counted = 0;
            peak = 0;
            last_report = now;
        }
    }
    return NULL;
}

int main(void) {
    setvbuf(stdout, NULL, _IOLBF, 0);
    mach_timebase_info(&timebase);

    mach_port_t service = MACH_PORT_NULL;
    kern_return_t kr = bootstrap_check_in(bootstrap_port, AD_SERVICE, &service);
    if (kr != KERN_SUCCESS) {
        fprintf(stderr, "bootstrap_check_in(%s) failed: %d (not started by launchd?)\n", AD_SERVICE, kr);
        return 1;
    }

    mach_vm_address_t address = 0;
    kr = mach_vm_allocate(mach_task_self(), &address, AD_SHARED_BYTES, VM_FLAGS_ANYWHERE);
    if (kr != KERN_SUCCESS) {
        fprintf(stderr, "mach_vm_allocate failed: %d\n", kr);
        return 1;
    }
    shared = (struct ad_shared *)address;
    shared->magic = AD_MAGIC;
    shared->version = AD_VERSION;
    shared->frames = AD_RING_FRAMES;
    shared->channels = AD_CHANNELS;

    memory_object_size_t size = AD_SHARED_BYTES;
    mach_port_t entry = MACH_PORT_NULL;
    kr = mach_make_memory_entry_64(mach_task_self(), &size, address, VM_PROT_READ | VM_PROT_WRITE, &entry,
                                   MACH_PORT_NULL);
    if (kr != KERN_SUCCESS || size != AD_SHARED_BYTES) {
        fprintf(stderr, "mach_make_memory_entry_64 failed: %d\n", kr);
        return 1;
    }

    pthread_t reader;
    pthread_create(&reader, NULL, read_ring, NULL);
    printf("waiting for the plug-in on %s\n", AD_SERVICE);

    for (;;) {
        struct {
            struct ad_hello hello;
            mach_msg_trailer_t trailer;
        } in;
        memset(&in, 0, sizeof in);
        kr = mach_msg(&in.hello.header, MACH_RCV_MSG, 0, sizeof in, service, MACH_MSG_TIMEOUT_NONE,
                      MACH_PORT_NULL);
        if (kr != KERN_SUCCESS) {
            fprintf(stderr, "receive failed: %d\n", kr);
            continue;
        }
        if (in.hello.header.msgh_id != AD_MSG_HELLO || in.hello.header.msgh_remote_port == MACH_PORT_NULL) {
            mach_msg_destroy(&in.hello.header);
            continue;
        }
        struct ad_memory out;
        memset(&out, 0, sizeof out);
        out.header.msgh_bits = MACH_MSGH_BITS(MACH_MSG_TYPE_MOVE_SEND_ONCE, 0) | MACH_MSGH_BITS_COMPLEX;
        out.header.msgh_size = sizeof out;
        out.header.msgh_remote_port = in.hello.header.msgh_remote_port;
        out.header.msgh_id = AD_MSG_MEMORY;
        out.body.msgh_descriptor_count = 1;
        out.entry.name = entry;
        out.entry.disposition = MACH_MSG_TYPE_COPY_SEND;
        out.entry.type = MACH_MSG_PORT_DESCRIPTOR;
        out.bytes = AD_SHARED_BYTES;
        kr = mach_msg(&out.header, MACH_SEND_MSG | MACH_SEND_TIMEOUT, sizeof out, 0, MACH_PORT_NULL, 500,
                      MACH_PORT_NULL);
        printf(kr == KERN_SUCCESS ? "plug-in connected\n" : "reply to plug-in failed: %d\n", kr);
    }
}
