//! MSB-first bit reader with Exp-Golomb support (H.264/H.265) and the AV1
//! `uvlc()`/`leb128()` codes.

use crate::MediaError;

/// Reads bits MSB-first from a byte slice (an RBSP for H.264/H.265, or an AV1
/// OBU payload). Every read is bounds-checked: running off the end returns
/// [`MediaError::Truncated`] instead of panicking.
#[derive(Debug, Clone)]
pub struct BitReader<'a> {
    data: &'a [u8],
    /// Position in bits. May point past the end after [`BitReader::skip`];
    /// the next read then fails.
    pos: usize,
}

impl<'a> BitReader<'a> {
    /// A reader positioned at the first bit of `data`.
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    /// Current position in bits from the start.
    pub fn position(&self) -> usize {
        self.pos
    }

    /// Bits left before the end of the data (0 if a skip went past the end).
    pub fn remaining(&self) -> usize {
        (self.data.len() * 8).saturating_sub(self.pos)
    }

    /// Read one bit.
    pub fn bit(&mut self) -> Result<u32, MediaError> {
        let byte = *self.data.get(self.pos >> 3).ok_or(MediaError::Truncated)?;
        let v = (byte >> (7 - (self.pos & 7))) & 1;
        self.pos += 1;
        Ok(u32::from(v))
    }

    /// Read one bit as a flag.
    pub fn flag(&mut self) -> Result<bool, MediaError> {
        Ok(self.bit()? == 1)
    }

    /// Read `n` bits (at most 32) as an unsigned big-endian number.
    pub fn bits(&mut self, n: u32) -> Result<u32, MediaError> {
        if n > 32 {
            return Err(MediaError::Invalid("bit field wider than 32 bits"));
        }
        if self.remaining() < n as usize {
            return Err(MediaError::Truncated);
        }
        let mut v: u64 = 0;
        for _ in 0..n {
            v = (v << 1) | u64::from(self.bit()?);
        }
        Ok(v as u32)
    }

    /// Skip `n` bits. Skipping past the end is allowed; the next read fails.
    pub fn skip(&mut self, n: usize) {
        self.pos = self.pos.saturating_add(n);
    }

    /// Unsigned Exp-Golomb code `ue(v)`.
    pub fn ue(&mut self) -> Result<u32, MediaError> {
        let mut zeros = 0u32;
        while self.bit()? == 0 {
            zeros += 1;
            if zeros > 31 {
                return Err(MediaError::Invalid("bad Exp-Golomb code"));
            }
        }
        let v = (1u64 << zeros) - 1 + u64::from(self.bits(zeros)?);
        Ok(v as u32)
    }

    /// Signed Exp-Golomb code `se(v)`.
    pub fn se(&mut self) -> Result<i32, MediaError> {
        let k = i64::from(self.ue()?);
        let v = if k & 1 == 1 { (k + 1) / 2 } else { -k / 2 };
        Ok(v as i32)
    }

    /// AV1 variable-length unsigned code `uvlc()`.
    pub fn uvlc(&mut self) -> Result<u32, MediaError> {
        let mut zeros = 0u32;
        while !self.flag()? {
            zeros += 1;
            if zeros >= 32 {
                return Ok(u32::MAX);
            }
        }
        let v = (1u64 << zeros) - 1 + u64::from(self.bits(zeros)?);
        Ok(v.min(u64::from(u32::MAX)) as u32)
    }
}

/// Read an AV1 `leb128()` value from the start of `data`.
/// Returns the value and the number of bytes it used.
pub fn leb128(data: &[u8]) -> Result<(u64, usize), MediaError> {
    let mut value = 0u64;
    for i in 0..8 {
        let byte = *data.get(i).ok_or(MediaError::Truncated)?;
        value |= u64::from(byte & 0x7f) << (i * 7);
        if byte & 0x80 == 0 {
            return Ok((value, i + 1));
        }
    }
    Err(MediaError::Invalid("leb128 longer than 8 bytes"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_bits_and_exp_golomb() {
        // 1 | 010 | 011 | 00100 | 00101 | 1 ...
        // ue: 0, 1, 2, 3, 4 ; then a 1-bit
        let data = [0b1010_0110, 0b0100_0010, 0b1100_0000];
        let mut r = BitReader::new(&data);
        assert_eq!(r.ue().unwrap(), 0);
        assert_eq!(r.ue().unwrap(), 1);
        assert_eq!(r.ue().unwrap(), 2);
        assert_eq!(r.ue().unwrap(), 3);
        assert_eq!(r.ue().unwrap(), 4);
        assert_eq!(r.bit().unwrap(), 1);
        assert_eq!(r.remaining(), 6);
    }

    #[test]
    fn signed_exp_golomb() {
        // ue values 1,2,3,4 -> se +1,-1,+2,-2
        let data = [0b0100_1100, 0b1000_0101, 0b0000_0000];
        let mut r = BitReader::new(&data);
        assert_eq!(r.se().unwrap(), 1);
        assert_eq!(r.se().unwrap(), -1);
        assert_eq!(r.se().unwrap(), 2);
        assert_eq!(r.se().unwrap(), -2);
    }

    #[test]
    fn truncation_is_an_error_not_a_panic() {
        let mut r = BitReader::new(&[0xff]);
        assert_eq!(r.bits(8).unwrap(), 0xff);
        assert!(matches!(r.bit(), Err(MediaError::Truncated)));
        let mut r = BitReader::new(&[0, 0, 0, 0, 0]);
        assert!(r.ue().is_err());
        let mut r = BitReader::new(&[0xff]);
        r.skip(100);
        assert!(r.bit().is_err());
        assert!(BitReader::new(&[0; 8]).bits(33).is_err());
    }

    #[test]
    fn bits_32_wide() {
        let mut r = BitReader::new(&[0xde, 0xad, 0xbe, 0xef]);
        assert_eq!(r.bits(32).unwrap(), 0xdead_beef);
    }

    #[test]
    fn leb128_values() {
        assert_eq!(leb128(&[0x05]).unwrap(), (5, 1));
        assert_eq!(leb128(&[0x80 | 0x2c, 0x02]).unwrap(), (300, 2));
        assert!(leb128(&[0x80]).is_err());
        assert!(leb128(&[0x80; 9]).is_err());
    }
}
