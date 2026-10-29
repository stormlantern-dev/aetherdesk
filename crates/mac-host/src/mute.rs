//! Keeps the Mac quiet while its sound plays on the client: the default
//! output device is muted for the session and unmuted after, but only if we
//! were the ones who muted it. A marker file covers a crash in between: the
//! next start finds it and unmutes ([`recover`]).

use std::path::PathBuf;
use std::ptr::NonNull;

use objc2_core_audio::{
    AudioObjectGetPropertyData, AudioObjectID, AudioObjectIsPropertySettable,
    AudioObjectPropertyAddress, AudioObjectSetPropertyData, kAudioDevicePropertyMute,
    kAudioHardwarePropertyDefaultOutputDevice, kAudioObjectPropertyElementMain,
    kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyScopeOutput, kAudioObjectSystemObject,
};

fn address(selector: u32, scope: u32) -> AudioObjectPropertyAddress {
    AudioObjectPropertyAddress {
        mSelector: selector,
        mScope: scope,
        mElement: kAudioObjectPropertyElementMain,
    }
}

fn get_u32(object: AudioObjectID, addr: &AudioObjectPropertyAddress) -> Option<u32> {
    let mut value = 0u32;
    let mut size = 4u32;
    let status = unsafe {
        AudioObjectGetPropertyData(
            object,
            NonNull::from(addr),
            0,
            std::ptr::null(),
            NonNull::from(&mut size),
            NonNull::from(&mut value).cast(),
        )
    };
    (status == 0).then_some(value)
}

fn set_u32(object: AudioObjectID, addr: &AudioObjectPropertyAddress, value: u32) -> bool {
    let status = unsafe {
        AudioObjectSetPropertyData(
            object,
            NonNull::from(addr),
            0,
            std::ptr::null(),
            4,
            NonNull::from(&value).cast(),
        )
    };
    status == 0
}

fn mute_address() -> AudioObjectPropertyAddress {
    address(kAudioDevicePropertyMute, kAudioObjectPropertyScopeOutput)
}

fn default_output() -> Option<AudioObjectID> {
    let addr = address(
        kAudioHardwarePropertyDefaultOutputDevice,
        kAudioObjectPropertyScopeGlobal,
    );
    get_u32(kAudioObjectSystemObject as AudioObjectID, &addr).filter(|&d| d != 0)
}

/// Where we note a device we muted: per user, since every session has its
/// own agent and its own temporary folder.
fn marker() -> PathBuf {
    std::env::temp_dir().join("aetherdesk-muted")
}

/// The Mac's output, muted by us until this is dropped.
pub struct Muted {
    device: AudioObjectID,
}

/// Mutes the default output, unless it's muted already or can't be.
/// Says why not, when not.
pub fn mute() -> Option<Muted> {
    let Some(device) = default_output() else {
        println!("sound: no output device to mute");
        return None;
    };
    let addr = mute_address();
    let mut settable = 0u8;
    let ok = unsafe {
        AudioObjectIsPropertySettable(device, NonNull::from(&addr), NonNull::from(&mut settable))
    };
    if ok != 0 || settable == 0 {
        println!("sound: the Mac's output can't be muted; it keeps playing");
        return None;
    }
    if get_u32(device, &addr) == Some(1) {
        println!("sound: the Mac's output was muted already");
        return None;
    }
    if !set_u32(device, &addr, 1) {
        println!("sound: muting the Mac's output failed; it keeps playing");
        return None;
    }
    let _ = std::fs::write(marker(), device.to_string());
    println!("sound: muted the Mac's output for the session");
    Some(Muted { device })
}

fn unmute(device: AudioObjectID) {
    if set_u32(device, &mute_address(), 0) {
        println!("sound: unmuted the Mac's output");
    } else {
        // Unplugged in the meantime, most likely: nothing left to unmute.
        println!("sound: couldn't unmute output device {device} (gone?)");
    }
}

impl Drop for Muted {
    fn drop(&mut self) {
        unmute(self.device);
        let _ = std::fs::remove_file(marker());
    }
}

/// Unmutes what a previous run muted and couldn't unmute (it crashed or
/// was killed mid-session). Call once at start.
pub fn recover() {
    let path = marker();
    let Ok(text) = std::fs::read_to_string(&path) else {
        return;
    };
    let _ = std::fs::remove_file(&path);
    if let Ok(device) = text.trim().parse::<AudioObjectID>() {
        println!("sound: a previous run left the Mac muted");
        unmute(device);
    }
}
