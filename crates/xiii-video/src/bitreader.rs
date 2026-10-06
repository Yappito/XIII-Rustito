//! LSB-first bit reader over the video packet.
//!
//! The MultimediaWiki *Bink Video* description states the bitstream is read LSB-first from
//! 32-bit little-endian words. Reading a byte stream one bit at a time starting at the least
//! significant bit of each byte is equivalent, and is what this reader does. All reads are
//! bounded: consuming past the end sets a sticky `overflow` flag and returns zero bits, so a
//! malformed frame cannot panic.

/// Bounded LSB-first bit reader.
#[derive(Debug, Clone)]
pub struct BitReader<'a> {
    data: &'a [u8],
    bit_pos: usize,
    overflow: bool,
}

impl<'a> BitReader<'a> {
    /// Creates a reader over `data`.
    pub fn new(data: &'a [u8]) -> Self {
        Self {
            data,
            bit_pos: 0,
            overflow: false,
        }
    }

    /// Number of bits consumed so far.
    pub fn bits_read(&self) -> usize {
        self.bit_pos
    }

    /// Number of whole bytes consumed so far (rounded up).
    pub fn bytes_read(&self) -> usize {
        self.bit_pos.div_ceil(8)
    }

    /// Total bit capacity of the underlying buffer.
    pub fn total_bits(&self) -> usize {
        self.data.len() * 8
    }

    /// True if a read ran past the end of the buffer.
    pub fn overflowed(&self) -> bool {
        self.overflow
    }

    /// True when fewer than `n` bits remain.
    pub fn short(&self, n: usize) -> bool {
        self.bit_pos + n > self.data.len() * 8
    }

    /// Reads `n` bits (`n <= 32`) LSB-first as an unsigned integer. Short reads return zero.
    pub fn read(&mut self, n: usize) -> u32 {
        debug_assert!(n <= 32);
        if n == 0 {
            return 0;
        }
        if self.short(n) {
            self.overflow = true;
            self.bit_pos = self.data.len() * 8;
            return 0;
        }
        let mut v: u32 = 0;
        for i in 0..n {
            let p = self.bit_pos + i;
            let bit = (self.data[p >> 3] >> (p & 7)) & 1;
            v |= u32::from(bit) << i;
        }
        self.bit_pos += n;
        v
    }

    /// Peeks `n` bits (`n <= 32`) without consuming them. A short peek returns 0.
    pub fn peek(&self, n: usize) -> u32 {
        if n == 0 || self.short(n) {
            return 0;
        }
        let mut v: u32 = 0;
        for i in 0..n {
            let p = self.bit_pos + i;
            let bit = (self.data[p >> 3] >> (p & 7)) & 1;
            v |= u32::from(bit) << i;
        }
        v
    }

    /// Reads one bit.
    pub fn bit(&mut self) -> u32 {
        self.read(1)
    }

    /// Reads `n` bits and sign-extends the result from `n` bits (two's complement).
    pub fn read_signed(&mut self, n: usize) -> i32 {
        if n == 0 {
            return 0;
        }
        let v = self.read(n);
        let shift = 32 - n;
        ((v << shift) as i32) >> shift
    }

    /// Skips `n` bits.
    pub fn skip(&mut self, n: usize) {
        if self.short(n) {
            self.overflow = true;
            self.bit_pos = self.data.len() * 8;
        } else {
            self.bit_pos += n;
        }
    }

    /// Advances to the next byte boundary.
    pub fn align_byte(&mut self) {
        self.bit_pos = (self.bit_pos + 7) & !7;
    }

    /// Advances to the next 32-bit word boundary (used between planes in some revisions).
    pub fn align_word(&mut self) {
        self.bit_pos = (self.bit_pos + 31) & !31;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_lsb_first() {
        let data = [0b1011_0100u8, 0xFF];
        let mut r = BitReader::new(&data);
        assert_eq!(r.read(3), 0b100);
        assert_eq!(r.read(5), 0b10110);
        assert_eq!(r.read(4), 0xF);
        assert_eq!(r.bits_read(), 12);
    }

    #[test]
    fn signed_extends() {
        let data = [0xFF];
        let mut r = BitReader::new(&data);
        assert_eq!(r.read_signed(4), -1);
    }

    #[test]
    fn detects_overrun_without_panicking() {
        let data = [0x00];
        let mut r = BitReader::new(&data);
        assert_eq!(r.read(9), 0);
        assert!(r.overflowed());
        assert!(!r.short(0));
        assert_eq!(r.read(1), 0);
    }

    #[test]
    fn reads_across_word_boundaries() {
        // 0x11223344 little-endian => bits LSB first: 0x44,0x33,0x22,0x11
        let data = [0x44u8, 0x33, 0x22, 0x11];
        let mut r = BitReader::new(&data);
        assert_eq!(r.read(8), 0x44);
        assert_eq!(r.read(24), 0x112233);
    }
}
