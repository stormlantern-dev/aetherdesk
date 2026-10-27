//! USB HID keyboard usages (page 0x07, what SDL scancodes are) to macOS
//! virtual key codes (Carbon `kVK_*`). Mapping is by physical position, so
//! the Mac's own keyboard layout decides which character a key produces.
//! Super/GUI becomes Command and Alt becomes Option, as on a Mac keyboard.

/// The macOS virtual key code for a HID usage, if the Mac has such a key.
pub fn keycode(hid: u16) -> Option<u16> {
    Some(match hid {
        // Letters a–z.
        0x04 => 0x00, // a
        0x05 => 0x0B, // b
        0x06 => 0x08, // c
        0x07 => 0x02, // d
        0x08 => 0x0E, // e
        0x09 => 0x03, // f
        0x0A => 0x05, // g
        0x0B => 0x04, // h
        0x0C => 0x22, // i
        0x0D => 0x26, // j
        0x0E => 0x28, // k
        0x0F => 0x25, // l
        0x10 => 0x2E, // m
        0x11 => 0x2D, // n
        0x12 => 0x1F, // o
        0x13 => 0x23, // p
        0x14 => 0x0C, // q
        0x15 => 0x0F, // r
        0x16 => 0x01, // s
        0x17 => 0x11, // t
        0x18 => 0x20, // u
        0x19 => 0x09, // v
        0x1A => 0x0D, // w
        0x1B => 0x07, // x
        0x1C => 0x10, // y
        0x1D => 0x06, // z
        // Digits 1–9, 0.
        0x1E => 0x12,
        0x1F => 0x13,
        0x20 => 0x14,
        0x21 => 0x15,
        0x22 => 0x17,
        0x23 => 0x16,
        0x24 => 0x1A,
        0x25 => 0x1C,
        0x26 => 0x19,
        0x27 => 0x1D,
        0x28 => 0x24,        // Return
        0x29 => 0x35,        // Escape
        0x2A => 0x33,        // Backspace (kVK_Delete)
        0x2B => 0x30,        // Tab
        0x2C => 0x31,        // Space
        0x2D => 0x1B,        // -
        0x2E => 0x18,        // =
        0x2F => 0x21,        // [
        0x30 => 0x1E,        // ]
        0x31 | 0x32 => 0x2A, // \ (and the non-US # key in the same spot)
        0x33 => 0x29,        // ;
        0x34 => 0x27,        // '
        0x35 => 0x32,        // `
        0x36 => 0x2B,        // ,
        0x37 => 0x2F,        // .
        0x38 => 0x2C,        // /
        0x39 => 0x39,        // Caps Lock
        // F1–F12.
        0x3A => 0x7A,
        0x3B => 0x78,
        0x3C => 0x63,
        0x3D => 0x76,
        0x3E => 0x60,
        0x3F => 0x61,
        0x40 => 0x62,
        0x41 => 0x64,
        0x42 => 0x65,
        0x43 => 0x6D,
        0x44 => 0x67,
        0x45 => 0x6F,
        // Print Screen, Scroll Lock, Pause sit where a Mac has F13–F15.
        0x46 => 0x69,
        0x47 => 0x6B,
        0x48 => 0x71,
        0x49 => 0x72, // Insert → Help
        0x4A => 0x73, // Home
        0x4B => 0x74, // Page Up
        0x4C => 0x75, // Delete (forward)
        0x4D => 0x77, // End
        0x4E => 0x79, // Page Down
        0x4F => 0x7C, // Right
        0x50 => 0x7B, // Left
        0x51 => 0x7D, // Down
        0x52 => 0x7E, // Up
        0x53 => 0x47, // Num Lock → keypad Clear
        0x54 => 0x4B, // keypad /
        0x55 => 0x43, // keypad *
        0x56 => 0x4E, // keypad -
        0x57 => 0x45, // keypad +
        0x58 => 0x4C, // keypad Enter
        0x59 => 0x53, // keypad 1
        0x5A => 0x54,
        0x5B => 0x55,
        0x5C => 0x56,
        0x5D => 0x57,
        0x5E => 0x58,
        0x5F => 0x59,
        0x60 => 0x5B,
        0x61 => 0x5C, // keypad 9
        0x62 => 0x52, // keypad 0
        0x63 => 0x41, // keypad .
        0x64 => 0x0A, // ISO key next to left Shift (kVK_ISO_Section)
        0x65 => 0x6E, // Menu / Application
        0x67 => 0x51, // keypad =
        // F13–F20.
        0x68 => 0x69,
        0x69 => 0x6B,
        0x6A => 0x71,
        0x6B => 0x6A,
        0x6C => 0x40,
        0x6D => 0x4F,
        0x6E => 0x50,
        0x6F => 0x5A,
        0x7F => 0x4A, // Mute
        0x80 => 0x48, // Volume Up
        0x81 => 0x49, // Volume Down
        0x87 => 0x5E, // JIS _ (International1)
        0x89 => 0x5D, // JIS ¥ (International3)
        0x90 => 0x68, // JIS Kana (LANG1)
        0x91 => 0x66, // JIS Eisu (LANG2)
        // Modifiers.
        0xE0 => 0x3B, // left Ctrl → Control
        0xE1 => 0x38, // left Shift
        0xE2 => 0x3A, // left Alt → Option
        0xE3 => 0x37, // left Super → Command
        0xE4 => 0x3E, // right Ctrl
        0xE5 => 0x3C, // right Shift
        0xE6 => 0x3D, // right Alt → right Option
        0xE7 => 0x36, // right Super → right Command
        _ => return None,
    })
}

/// HID usage of Caps Lock.
pub const CAPS_LOCK: u16 = 0x39;

/// Bit index (0–7) for the eight HID modifier usages 0xE0–0xE7.
pub fn modifier_bit(hid: u16) -> Option<u8> {
    (0xE0..=0xE7).contains(&hid).then(|| (hid - 0xE0) as u8)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn letters_digits_and_common_keys() {
        assert_eq!(keycode(0x04), Some(0x00)); // a
        assert_eq!(keycode(0x1D), Some(0x06)); // z
        assert_eq!(keycode(0x27), Some(0x1D)); // 0
        assert_eq!(keycode(0x28), Some(0x24)); // Return
        assert_eq!(keycode(0x2C), Some(0x31)); // Space
        assert_eq!(keycode(0x52), Some(0x7E)); // Up
    }

    #[test]
    fn super_is_command_and_alt_is_option() {
        assert_eq!(keycode(0xE3), Some(0x37));
        assert_eq!(keycode(0xE7), Some(0x36));
        assert_eq!(keycode(0xE2), Some(0x3A));
        assert_eq!(keycode(0xE6), Some(0x3D));
    }

    #[test]
    fn every_letter_and_digit_is_mapped_once() {
        let codes: Vec<u16> = (0x04..=0x27).map(|h| keycode(h).unwrap()).collect();
        let mut unique = codes.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), codes.len(), "two keys share a macOS key code");
    }

    #[test]
    fn unknown_usages_are_ignored() {
        assert_eq!(keycode(0x00), None);
        assert_eq!(keycode(0x66), None); // Power: not something to forward
        assert_eq!(keycode(0xFFFF), None);
    }

    #[test]
    fn modifier_bits() {
        assert_eq!(modifier_bit(0xE0), Some(0));
        assert_eq!(modifier_bit(0xE7), Some(7));
        assert_eq!(modifier_bit(0x04), None);
    }
}
