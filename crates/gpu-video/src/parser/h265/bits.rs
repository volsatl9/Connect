//! Reading H.265 syntax elements: emulation prevention removed, then bits, unsigned and
//! signed Exp-Golomb codes. (Connect addition, see VENDORED.md.)

use super::H265ParseError;

/// The NAL unit payload with emulation prevention bytes (`00 00 03`) taken out.
pub(crate) fn rbsp(nal: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(nal.len());
    let mut zeros = 0;
    for &byte in nal {
        if zeros >= 2 && byte == 3 {
            zeros = 0;
            continue;
        }
        zeros = if byte == 0 { zeros + 1 } else { 0 };
        out.push(byte);
    }
    out
}

pub(crate) struct BitReader<'a> {
    data: &'a [u8],
    position: usize,
}

impl<'a> BitReader<'a> {
    pub(crate) fn new(data: &'a [u8]) -> Self {
        Self { data, position: 0 }
    }

    /// Bits read so far.
    pub(crate) fn position(&self) -> usize {
        self.position
    }

    pub(crate) fn bit(&mut self) -> Result<bool, H265ParseError> {
        let byte = *self.data.get(self.position / 8).ok_or(H265ParseError::Truncated)?;
        let bit = (byte >> (7 - self.position % 8)) & 1;
        self.position += 1;
        Ok(bit == 1)
    }

    /// `n` bits (at most 32) as an unsigned number.
    pub(crate) fn bits(&mut self, n: u32) -> Result<u32, H265ParseError> {
        debug_assert!(n <= 32);
        let mut value = 0u64;
        for _ in 0..n {
            value = (value << 1) | u64::from(self.bit()?);
        }
        Ok(value as u32)
    }

    pub(crate) fn skip(&mut self, n: usize) -> Result<(), H265ParseError> {
        if self.position + n > self.data.len() * 8 {
            return Err(H265ParseError::Truncated);
        }
        self.position += n;
        Ok(())
    }

    /// ue(v).
    pub(crate) fn ue(&mut self) -> Result<u32, H265ParseError> {
        let mut zeros = 0;
        while !self.bit()? {
            zeros += 1;
            if zeros > 31 {
                return Err(H265ParseError::Invalid("Exp-Golomb code too long"));
            }
        }
        let rest = self.bits(zeros)?;
        Ok(((1u64 << zeros) - 1 + u64::from(rest)) as u32)
    }

    /// se(v).
    pub(crate) fn se(&mut self) -> Result<i32, H265ParseError> {
        let k = self.ue()?;
        Ok(if k % 2 == 1 { k.div_ceil(2) as i32 } else { -((k / 2) as i32) })
    }

    /// ue(v) that must be at most `max`.
    pub(crate) fn ue_max(&mut self, max: u32, what: &'static str) -> Result<u32, H265ParseError> {
        let value = self.ue()?;
        if value > max {
            return Err(H265ParseError::Invalid(what));
        }
        Ok(value)
    }
}

/// Ceil(Log2(n)), the bit length of indices into `n` items.
pub(crate) fn ceil_log2(n: u32) -> u32 {
    if n <= 1 { 0 } else { 32 - (n - 1).leading_zeros() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emulation_prevention_is_removed() {
        assert_eq!(rbsp(&[0, 0, 3, 1, 0, 0, 3, 0, 0, 3]), [0, 0, 1, 0, 0, 0, 0]);
        assert_eq!(rbsp(&[1, 0, 3, 3]), [1, 0, 3, 3]);
    }

    #[test]
    fn exp_golomb_and_bits() {
        // 1 | 010 | 011 | 00100 | 00101 -> ue 0, 1, 2, 3, se(4) = -2
        let data = [0b1010_0110, 0b0100_0010, 0b1000_0000];
        let mut r = BitReader::new(&data);
        assert_eq!(r.ue().unwrap(), 0);
        assert_eq!(r.ue().unwrap(), 1);
        assert_eq!(r.ue().unwrap(), 2);
        assert_eq!(r.ue().unwrap(), 3);
        assert_eq!(r.se().unwrap(), -2);
        assert_eq!(r.position(), 17);
        assert_eq!(ceil_log2(1), 0);
        assert_eq!(ceil_log2(2), 1);
        assert_eq!(ceil_log2(5), 3);
        assert_eq!(ceil_log2(8), 3);
    }
}
