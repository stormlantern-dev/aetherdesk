//! The daemon's end of the Auroval audio device (#82). The plug-in
//! (`crates/mac-host/driver`, run by coreaudiod) looks up our Mach service
//! when the device starts; we answer with a memory entry for a ring buffer,
//! which the plug-in fills with everything played to the device. A thread
//! here follows the ring, encodes Opus and sends it to the client of the
//! session that uses the device. The other way, the client's microphone
//! packets are decoded into a second ring, which the device plays as its
//! input.
//!
//! The service is declared in the daemon's launchd job (`MachServices`), so
//! it exists whenever the daemon does, at the login window too. Only
//! coreaudiod's user gets the ring: anyone else could listen to the Mac.
//! The layout must match `driver/shared.h`.

use std::ffi::{CString, c_char};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use auroval_proto::AudioPacket;

use crate::audio::{OpusDecoder, OpusEncoder, Sink};
use crate::session;

const SERVICE: &str = "nl.stormlantern.auroval.audio";
const MAGIC: u32 = 0x4144_4155;
const VERSION: u32 = 2;
const RATE: u64 = 48_000;
const CHANNELS: usize = 2;
const RING_FRAMES: u64 = 16_384;
const MIC_CHANNELS: usize = auroval_proto::MIC_CHANNELS as usize;
const MIC_FRAMES: u64 = 16_384;
const MSG_HELLO: i32 = 0x4144;
const MSG_MEMORY: i32 = 0x4145;

/// The ring's header, as `struct ad_shared`; the samples follow it.
#[repr(C)]
struct Shared {
    magic: u32,
    version: u32,
    frames: u32,
    channels: u32,
    /// Sample time just past the newest frame written.
    written: AtomicU64,
    /// mach_absolute_time() of that write.
    host: AtomicU64,
    cycles: AtomicU64,
    /// Microphone frames written so far; they go at
    /// `mic[n % MIC_FRAMES]`, after the speaker's ring.
    mic_written: AtomicU64,
    reserved: [u64; 2],
}

const SHARED_BYTES: usize = (size_of::<Shared>()
    + (RING_FRAMES as usize * CHANNELS + MIC_FRAMES as usize * MIC_CHANNELS) * 4
    + 16_383)
    & !16_383;

// --- Mach --------------------------------------------------------------

type Port = u32;

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct Header {
    bits: u32,
    size: u32,
    remote: Port,
    local: Port,
    voucher: Port,
    id: i32,
}

/// `struct ad_memory`: the reply, carrying the memory entry.
#[repr(C)]
struct Memory {
    header: Header,
    descriptors: u32,
    name: Port,
    pad1: u32,
    pad2: u16,
    disposition: u8,
    kind: u8,
    bytes: u64,
}

/// A received hello with room for its audit trailer.
#[repr(C)]
struct Incoming {
    header: Header,
    rest: [u32; 64],
}

const MACH_MSG_TYPE_MOVE_SEND_ONCE: u32 = 18;
const MACH_MSG_TYPE_COPY_SEND: u8 = 19;
const MACH_MSGH_BITS_COMPLEX: u32 = 0x8000_0000;
const MACH_MSG_PORT_DESCRIPTOR: u8 = 0;
const MACH_SEND_MSG: i32 = 1;
const MACH_RCV_MSG: i32 = 2;
const MACH_SEND_TIMEOUT: i32 = 0x10;
/// MACH_RCV_TRAILER_ELEMENTS(MACH_RCV_TRAILER_AUDIT): the sender's ids.
const MACH_RCV_TRAILER_AUDIT: i32 = 3 << 24;
const VM_FLAGS_ANYWHERE: i32 = 1;
const VM_PROT_READ_WRITE: i32 = 3;

unsafe extern "C" {
    static bootstrap_port: Port;
    static mach_task_self_: Port;
    fn bootstrap_check_in(bp: Port, name: *const c_char, port: *mut Port) -> i32;
    fn mach_vm_allocate(task: Port, address: *mut u64, size: u64, flags: i32) -> i32;
    fn mach_make_memory_entry_64(
        task: Port,
        size: *mut u64,
        offset: u64,
        permission: i32,
        entry: *mut Port,
        parent: Port,
    ) -> i32;
    fn mach_msg(
        msg: *mut Header,
        option: i32,
        send_size: u32,
        rcv_size: u32,
        rcv_name: Port,
        timeout: u32,
        notify: Port,
    ) -> i32;
    fn mach_msg_destroy(msg: *mut Header);
    fn mach_timebase_info(info: *mut Timebase) -> i32;
}

#[repr(C)]
struct Timebase {
    numer: u32,
    denom: u32,
}

fn task() -> Port {
    unsafe { mach_task_self_ }
}

/// coreaudiod's user, which runs the plug-in.
fn coreaudiod_uid() -> Option<u32> {
    let name = CString::new("_coreaudiod").ok()?;
    let pw = unsafe { libc::getpwnam(name.as_ptr()) };
    (!pw.is_null()).then(|| unsafe { (*pw).pw_uid })
}

// --- The speaker -------------------------------------------------------

/// The device's sound, and where it goes; and the client's microphone.
pub struct Speaker {
    /// The agent whose session plays to the device, and its client.
    target: Mutex<Option<(u64, quinn::Connection)>>,
    ring: Ring,
    /// Decodes the microphone; made with the first packet.
    mic: Mutex<Option<OpusDecoder>>,
}

impl Speaker {
    /// One microphone packet from the client, into the device's input. A
    /// lost packet is a gap the plug-in fills with silence.
    pub fn mic(&self, packet: &AudioPacket) {
        let mut decoder = self.mic.lock().unwrap();
        if decoder.is_none() {
            match OpusDecoder::new(MIC_CHANNELS as u32) {
                Ok(d) => *decoder = Some(d),
                Err(e) => {
                    eprintln!("speaker: microphone: {e}");
                    return;
                }
            }
        }
        let Some(d) = decoder.as_mut() else { return };
        match d.decode(&packet.data) {
            Ok(pcm) => self.ring.write_mic(pcm),
            Err(e) => eprintln!("speaker: microphone: {e}"),
        }
    }

    /// Forgets the decoder: the next session starts fresh.
    pub fn mic_ended(&self) {
        self.mic.lock().unwrap().take();
    }

    /// Sends the device's sound to `conn`: agent `agent` switched the
    /// Mac's output to the device for that client.
    pub fn play_to(&self, agent: u64, conn: quinn::Connection) {
        *self.target.lock().unwrap() = Some((agent, conn));
    }

    /// Stops sending, if `agent` is the one it sends for.
    pub fn stop(&self, agent: u64) {
        let mut target = self.target.lock().unwrap();
        if target.as_ref().is_some_and(|(a, _)| *a == agent) {
            *target = None;
        }
    }
}

/// Checks in the Mach service and starts serving the plug-in. `None`, with
/// the reason printed, when the service isn't ours: not started by launchd
/// with it (`serve`, or a job from before the speaker).
pub fn start() -> Option<Arc<Speaker>> {
    let name = CString::new(SERVICE).unwrap();
    let mut service: Port = 0;
    let kr = unsafe { bootstrap_check_in(bootstrap_port, name.as_ptr(), &mut service) };
    if kr != 0 {
        println!("speaker: no Mach service {SERVICE} ({kr}): reinstall to get the Auroval device");
        return None;
    }
    let Some(audio_uid) = coreaudiod_uid() else {
        eprintln!("speaker: no _coreaudiod user");
        return None;
    };
    let mut address = 0u64;
    let kr =
        unsafe { mach_vm_allocate(task(), &mut address, SHARED_BYTES as u64, VM_FLAGS_ANYWHERE) };
    if kr != 0 {
        eprintln!("speaker: mach_vm_allocate failed ({kr})");
        return None;
    }
    let shared = address as *mut Shared;
    unsafe {
        (*shared).magic = MAGIC;
        (*shared).version = VERSION;
        (*shared).frames = RING_FRAMES as u32;
        (*shared).channels = CHANNELS as u32;
    }
    let mut size = SHARED_BYTES as u64;
    let mut entry: Port = 0;
    let kr = unsafe {
        mach_make_memory_entry_64(
            task(),
            &mut size,
            address,
            VM_PROT_READ_WRITE,
            &mut entry,
            0,
        )
    };
    if kr != 0 || size != SHARED_BYTES as u64 {
        eprintln!("speaker: mach_make_memory_entry_64 failed ({kr})");
        return None;
    }

    // The ring lives as long as the process: never unmapped.
    let speaker = Arc::new(Speaker {
        target: Mutex::default(),
        ring: Ring(shared),
        mic: Mutex::default(),
    });
    std::thread::Builder::new()
        .name("speaker-service".into())
        .spawn(move || serve(service, entry, audio_uid))
        .ok()?;
    let s = speaker.clone();
    std::thread::Builder::new()
        .name("speaker-ring".into())
        .spawn(move || follow(&s))
        .ok()?;
    println!("speaker: serving the Auroval device on {SERVICE}");
    Some(speaker)
}

/// Answers the plug-in's hellos with the ring's memory entry.
fn serve(service: Port, entry: Port, audio_uid: u32) {
    loop {
        let mut msg = Incoming {
            header: Header::default(),
            rest: [0; 64],
        };
        let kr = unsafe {
            mach_msg(
                &mut msg.header,
                MACH_RCV_MSG | MACH_RCV_TRAILER_AUDIT,
                0,
                size_of::<Incoming>() as u32,
                service,
                0,
                0,
            )
        };
        if kr != 0 {
            eprintln!("speaker: receive failed ({kr})");
            std::thread::sleep(Duration::from_secs(1));
            continue;
        }
        // The trailer follows the message: type, size, seqno, then the
        // sender's security token, whose first word is its euid.
        let trailer = msg.header.size as usize / 4 - size_of::<Header>() / 4;
        let euid = msg.rest.get(trailer + 3).copied();
        if msg.header.id != MSG_HELLO || msg.header.remote == 0 || euid != Some(audio_uid) {
            println!(
                "speaker: ignored a message (id {:#x}, uid {euid:?})",
                msg.header.id
            );
            unsafe { mach_msg_destroy(&mut msg.header) };
            continue;
        }
        let mut reply = Memory {
            header: Header {
                bits: MACH_MSG_TYPE_MOVE_SEND_ONCE | MACH_MSGH_BITS_COMPLEX,
                size: size_of::<Memory>() as u32,
                remote: msg.header.remote,
                local: 0,
                voucher: 0,
                id: MSG_MEMORY,
            },
            descriptors: 1,
            name: entry,
            pad1: 0,
            pad2: 0,
            disposition: MACH_MSG_TYPE_COPY_SEND,
            kind: MACH_MSG_PORT_DESCRIPTOR,
            bytes: SHARED_BYTES as u64,
        };
        let kr = unsafe {
            mach_msg(
                &mut reply.header,
                MACH_SEND_MSG | MACH_SEND_TIMEOUT,
                size_of::<Memory>() as u32,
                0,
                0,
                500,
                0,
            )
        };
        if kr == 0 {
            println!("speaker: the Auroval device connected");
        } else {
            eprintln!("speaker: answering the device failed ({kr})");
        }
    }
}

struct Ring(*mut Shared);

// The plug-in writes the speaker's ring and we read it; we write the
// microphone's ring (one writer, behind Speaker::mic's lock) and the
// plug-in reads it. The header's atomics order both.
unsafe impl Send for Ring {}
unsafe impl Sync for Ring {}

impl Ring {
    fn header(&self) -> &Shared {
        unsafe { &*self.0 }
    }

    fn data(&self) -> *mut f32 {
        unsafe { self.0.cast::<u8>().add(size_of::<Shared>()).cast::<f32>() }
    }

    /// Frame `at`'s two samples.
    fn frame(&self, at: u64) -> [f32; 2] {
        let data = self.data();
        let i = (at % RING_FRAMES) as usize * CHANNELS;
        unsafe { [data.add(i).read_volatile(), data.add(i + 1).read_volatile()] }
    }

    /// Appends microphone frames for the plug-in.
    fn write_mic(&self, pcm: &[f32]) {
        let mic = unsafe { self.data().add(RING_FRAMES as usize * CHANNELS) };
        let header = self.header();
        let start = header.mic_written.load(Ordering::Relaxed);
        for (i, &v) in pcm.iter().enumerate() {
            let at = (start + i as u64) % MIC_FRAMES;
            unsafe { mic.add(at as usize).write_volatile(v) };
        }
        header
            .mic_written
            .store(start + pcm.len() as u64, Ordering::Release);
    }
}

fn host_seconds(ticks: u64) -> f64 {
    let mut tb = Timebase { numer: 0, denom: 0 };
    unsafe { mach_timebase_info(&mut tb) };
    ticks as f64 * f64::from(tb.numer) / f64::from(tb.denom) / 1e9
}

/// Follows the ring every 2 ms and passes new frames to the encoder while
/// a session plays to the device.
fn follow(speaker: &Arc<Speaker>) {
    let ring = &speaker.ring;
    let mut pos = 0u64;
    let mut pcm = Vec::<f32>::new();
    let mut encoder: Option<(u64, OpusEncoder)> = None;
    let sink: Sink = {
        let speaker = speaker.clone();
        Box::new(move |packet| {
            if let Some((_, conn)) = speaker.target.lock().unwrap().as_ref() {
                session::send_audio(conn, &packet);
            }
        })
    };
    loop {
        std::thread::sleep(Duration::from_millis(2));
        let written = ring.header().written.load(Ordering::Acquire);
        let agent = speaker.target.lock().unwrap().as_ref().map(|(a, _)| *a);
        let Some(agent) = agent else {
            pos = written;
            encoder = None;
            continue;
        };
        if written < pos || written - pos > RING_FRAMES / 2 {
            // The device started again, or we fell behind: newest frames.
            pos = written.saturating_sub(RING_FRAMES / 4);
        }
        if written == pos {
            continue;
        }
        // A new session gets a new encoder, so its packets count from 0.
        if encoder.as_ref().is_none_or(|(a, _)| *a != agent) {
            match OpusEncoder::new() {
                Ok(e) => encoder = Some((agent, e)),
                Err(e) => {
                    eprintln!("speaker: {e}");
                    std::thread::sleep(Duration::from_secs(1));
                    continue;
                }
            }
        }
        pcm.clear();
        for at in pos..written {
            pcm.extend(ring.frame(at));
        }
        // When the oldest of these frames was written, near enough.
        let at = host_seconds(ring.header().host.load(Ordering::Relaxed))
            - (written - pos) as f64 / RATE as f64;
        pos = written;
        if let Some((_, e)) = encoder.as_mut()
            && let Err(err) = e.push(&pcm, at, &sink)
        {
            eprintln!("speaker: {err}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The same layout as driver/shared.h, which C checks for itself.
    #[test]
    fn layout_matches_the_plug_in() {
        assert_eq!(size_of::<Shared>(), 64);
        assert_eq!(size_of::<Header>(), 24);
        assert_eq!(size_of::<Memory>(), 48);
        assert_eq!(std::mem::offset_of!(Memory, bytes), 40);
        assert_eq!(SHARED_BYTES % 16_384, 0);
        const { assert!(SHARED_BYTES >= 64 + 16_384 * 3 * 4) };
    }
}
