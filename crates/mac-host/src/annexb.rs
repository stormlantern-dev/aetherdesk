//! VideoToolbox hands out H.264 as AVCC: each NAL unit prefixed with its
//! length (usually 4 bytes, big-endian), parameter sets kept separately in
//! the format description. Decoders reading a byte stream, ffmpeg included,
//! want Annex B: NAL units separated by start codes, with SPS/PPS in-band
//! before every keyframe. This module converts; it has no platform code, so
//! it's tested on Linux too.

const START_CODE: [u8; 4] = [0, 0, 0, 1];

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    #[error("NAL length prefix of {0} bytes is not 1, 2 or 4")]
    BadPrefixSize(usize),
    #[error("NAL unit at offset {offset} claims {len} bytes but only {left} are left")]
    Truncated {
        offset: usize,
        len: usize,
        left: usize,
    },
}

/// Appends a parameter set (SPS or PPS) as one Annex B NAL unit.
pub fn push_parameter_set(out: &mut Vec<u8>, nal: &[u8]) {
    out.extend_from_slice(&START_CODE);
    out.extend_from_slice(nal);
}

/// Appends the NAL units in `avcc` to `out`, replacing each length prefix of
/// `prefix_size` bytes with a start code.
pub fn push_avcc(out: &mut Vec<u8>, avcc: &[u8], prefix_size: usize) -> Result<(), Error> {
    if !matches!(prefix_size, 1 | 2 | 4) {
        return Err(Error::BadPrefixSize(prefix_size));
    }
    let mut offset = 0;
    while offset < avcc.len() {
        let left = avcc.len() - offset;
        if left < prefix_size {
            return Err(Error::Truncated {
                offset,
                len: prefix_size,
                left,
            });
        }
        let len = avcc[offset..offset + prefix_size]
            .iter()
            .fold(0usize, |acc, b| (acc << 8) | usize::from(*b));
        offset += prefix_size;
        let left = avcc.len() - offset;
        if len > left {
            return Err(Error::Truncated { offset, len, left });
        }
        out.extend_from_slice(&START_CODE);
        out.extend_from_slice(&avcc[offset..offset + len]);
        offset += len;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_four_byte_prefixes() {
        let avcc = [0, 0, 0, 2, 0x65, 0xAA, 0, 0, 0, 1, 0x41];
        let mut out = Vec::new();
        push_avcc(&mut out, &avcc, 4).unwrap();
        assert_eq!(out, [0, 0, 0, 1, 0x65, 0xAA, 0, 0, 0, 1, 0x41]);
    }

    #[test]
    fn converts_two_byte_prefixes() {
        let avcc = [0, 3, 0x65, 1, 2];
        let mut out = Vec::new();
        push_avcc(&mut out, &avcc, 2).unwrap();
        assert_eq!(out, [0, 0, 0, 1, 0x65, 1, 2]);
    }

    #[test]
    fn keyframe_gets_parameter_sets_first() {
        let mut out = Vec::new();
        push_parameter_set(&mut out, &[0x67, 1]);
        push_parameter_set(&mut out, &[0x68, 2]);
        push_avcc(&mut out, &[0, 0, 0, 1, 0x65], 4).unwrap();
        assert_eq!(
            out,
            [0, 0, 0, 1, 0x67, 1, 0, 0, 0, 1, 0x68, 2, 0, 0, 0, 1, 0x65]
        );
    }

    #[test]
    fn empty_input_is_fine() {
        let mut out = Vec::new();
        push_avcc(&mut out, &[], 4).unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn rejects_truncated_nal() {
        let mut out = Vec::new();
        let err = push_avcc(&mut out, &[0, 0, 0, 5, 0x65, 0xAA], 4).unwrap_err();
        assert_eq!(
            err,
            Error::Truncated {
                offset: 4,
                len: 5,
                left: 2
            }
        );
    }

    #[test]
    fn rejects_truncated_prefix() {
        let mut out = Vec::new();
        assert!(matches!(
            push_avcc(&mut out, &[0, 0], 4),
            Err(Error::Truncated { .. })
        ));
    }

    #[test]
    fn rejects_odd_prefix_size() {
        assert_eq!(
            push_avcc(&mut Vec::new(), &[], 3),
            Err(Error::BadPrefixSize(3))
        );
    }
}
