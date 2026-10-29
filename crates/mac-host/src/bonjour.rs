//! Announcing the host on the LAN with Bonjour, so clients find it by its
//! certificate instead of a remembered address. Uses `DNSServiceRegister`
//! from libSystem (mDNSResponder); nothing to install.

use std::ffi::{CString, c_char, c_void};

use aetherdesk_proto::discovery;
use aetherdesk_proto::trust::Fingerprint;

type DnsServiceRef = *mut c_void;

/// `kDNSServiceErr_NoError`.
const NO_ERROR: i32 = 0;

unsafe extern "C" {
    fn DNSServiceRegister(
        sd_ref: *mut DnsServiceRef,
        flags: u32,
        interface_index: u32,
        name: *const c_char,
        regtype: *const c_char,
        domain: *const c_char,
        host: *const c_char,
        port: u16,
        txt_len: u16,
        txt_record: *const c_void,
        callback: *const c_void,
        context: *mut c_void,
    ) -> i32;
    fn DNSServiceRefDeallocate(sd_ref: DnsServiceRef);
}

/// A live announcement; dropping it withdraws it.
pub struct Announcement(DnsServiceRef);

// The ref is only used again to deallocate it.
unsafe impl Send for Announcement {}

impl Drop for Announcement {
    fn drop(&mut self) {
        unsafe { DNSServiceRefDeallocate(self.0) };
    }
}

/// Announces [`discovery::SERVICE`] on `port`, under the Mac's computer name
/// (mDNSResponder renames on a clash), on every interface.
pub fn announce(port: u16, fingerprint: &Fingerprint) -> Result<Announcement, String> {
    let regtype = CString::new(discovery::SERVICE).expect("no NUL");
    let txt = discovery::txt_record(fingerprint);
    let txt_len = u16::try_from(txt.len()).map_err(|_| "TXT record too long")?;
    let mut sd_ref: DnsServiceRef = std::ptr::null_mut();
    // No callback: we don't need to hear the name it settled on, and the
    // registration lives as long as the ref.
    let err = unsafe {
        DNSServiceRegister(
            &mut sd_ref,
            0,
            0,
            std::ptr::null(),
            regtype.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            port.to_be(),
            txt_len,
            txt.as_ptr().cast(),
            std::ptr::null(),
            std::ptr::null_mut(),
        )
    };
    if err != NO_ERROR || sd_ref.is_null() {
        return Err(format!("DNSServiceRegister failed ({err})"));
    }
    Ok(Announcement(sd_ref))
}
