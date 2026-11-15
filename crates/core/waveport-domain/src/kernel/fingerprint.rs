//! A hash of kernel state that is the same on every platform.

use core::hash::{Hash, Hasher};

/// FNV-1a over the bytes a value hashes, with integers written
/// little-endian whatever the platform, and `usize` as 64 bits.
struct Fnv(u64);

impl Hasher for Fnv {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.0 ^= u64::from(byte);
            self.0 = self.0.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }

    fn write_u16(&mut self, value: u16) {
        self.write(&value.to_le_bytes());
    }

    fn write_u32(&mut self, value: u32) {
        self.write(&value.to_le_bytes());
    }

    fn write_u64(&mut self, value: u64) {
        self.write(&value.to_le_bytes());
    }

    fn write_usize(&mut self, value: usize) {
        self.write_u64(u64::try_from(value).unwrap_or(u64::MAX));
    }

    fn write_isize(&mut self, value: isize) {
        self.write(&i64::try_from(value).unwrap_or(i64::MAX).to_le_bytes());
    }
}

/// The fingerprint of `value`.
pub(super) fn of<T: Hash>(value: &T) -> u64 {
    let mut hasher = Fnv(0xcbf2_9ce4_8422_2325);
    value.hash(&mut hasher);
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use core::hash::{Hash, Hasher};

    use super::of;

    /// Hashes as its bytes and nothing else.
    struct Raw<const N: usize>([u8; N]);

    impl<const N: usize> Hash for Raw<N> {
        fn hash<H: Hasher>(&self, state: &mut H) {
            state.write(&self.0);
        }
    }

    #[test]
    fn the_hash_is_fnv_1a() {
        // Published test vectors for 64-bit FNV-1a.
        assert_eq!(of(&Raw([])), 0xcbf2_9ce4_8422_2325);
        assert_eq!(of(&Raw(*b"a")), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(of(&Raw(*b"foobar")), 0x8594_4171_f739_67e8);
    }

    #[test]
    fn integers_hash_the_same_on_every_platform() {
        assert_eq!(of(&0x0102_u16), of(&Raw([2, 1])));
        assert_eq!(of(&0x0102_0304_u32), of(&Raw([4, 3, 2, 1])));
        assert_eq!(of(&7_u64), of(&Raw([7, 0, 0, 0, 0, 0, 0, 0])));
        assert_eq!(of(&7_usize), of(&7_u64));
        assert_eq!(of(&-2_isize), of(&-2_i64));
    }
}
