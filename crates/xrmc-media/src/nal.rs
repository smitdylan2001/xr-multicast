//! Annex B byte-stream helpers shared by H.264 and H.265.

/// Offsets of NAL unit payloads (just after each `00 00 01` start code) in an
/// Annex B buffer. 4-byte start codes are found too (their last 3 bytes match).
pub fn nal_offsets(data: &[u8]) -> Vec<usize> {
    let mut out = Vec::new();
    let mut i = 0;
    while i + 3 < data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            out.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    out
}

/// Split an Annex B buffer into NAL units (start codes removed). Trailing zero
/// bytes before the next start code (the leading byte of a 4-byte start code,
/// or `trailing_zero_8bits`) are trimmed. Bytes before the first start code
/// are ignored. A NAL unit may be empty.
pub fn split_nals(data: &[u8]) -> Vec<&[u8]> {
    let mut nals = Vec::new();
    let mut start: Option<usize> = None;
    let mut i = 0;
    let n = data.len();
    while i + 2 < n {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            if let Some(s) = start {
                let mut end = i;
                while end > s && data[end - 1] == 0 {
                    end -= 1;
                }
                nals.push(&data[s..end]);
            }
            i += 3;
            start = Some(i);
        } else {
            i += 1;
        }
    }
    if let Some(s) = start
        && s < n
    {
        nals.push(&data[s..n]);
    }
    nals
}

/// Remove emulation-prevention bytes (`00 00 03` -> `00 00`) from a NAL unit,
/// producing its RBSP.
pub fn unescape_rbsp(nal: &[u8]) -> Vec<u8> {
    unescape_rbsp_prefix(nal, usize::MAX)
}

/// Like [`unescape_rbsp`], but stop once `max` RBSP bytes have been produced.
pub fn unescape_rbsp_prefix(nal: &[u8], max: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(nal.len().min(max));
    let mut zeros = 0;
    for &b in nal {
        if out.len() >= max {
            break;
        }
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        out.push(b);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_nal_offsets_with_3_and_4_byte_start_codes() {
        let data = [0, 0, 0, 1, 0x67, 0xaa, 0, 0, 1, 0x68, 0xbb];
        assert_eq!(nal_offsets(&data), vec![4, 9]);
        // A start code at the very end has no payload and is not reported.
        assert_eq!(nal_offsets(&[0, 0, 1]), Vec::<usize>::new());
    }

    #[test]
    fn splits_nals_and_trims_trailing_zeros() {
        let data = [
            9, 9, 0, 0, 0, 1, 0x67, 0xaa, 0, 0, 0, 0, 1, 0x68, 0xbb, 0, 0, 1,
        ];
        let nals = split_nals(&data);
        assert_eq!(nals, vec![&[0x67, 0xaa][..], &[0x68, 0xbb][..]]);
        assert!(split_nals(&[1, 2, 3]).is_empty());
        // Back-to-back start codes give an empty NAL unit.
        assert_eq!(split_nals(&[0, 0, 1, 0, 0, 1, 5]), vec![&[][..], &[5][..]]);
    }

    #[test]
    fn removes_emulation_prevention() {
        assert_eq!(
            unescape_rbsp(&[0x67, 0, 0, 3, 1, 0, 0, 3, 0, 3]),
            vec![0x67, 0, 0, 1, 0, 0, 0, 3]
        );
        assert_eq!(unescape_rbsp_prefix(&[1, 2, 3, 4], 2), vec![1, 2]);
    }
}
