//! Big-endian ("network order") accessors. Every multi-byte SCSI field is BE.

#[must_use]
pub const fn read_u16(b: &[u8]) -> u16 {
    ((b[0] as u16) << 8) | b[1] as u16
}

#[must_use]
pub const fn read_u24(b: &[u8]) -> u32 {
    ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32
}

#[must_use]
pub const fn read_u32(b: &[u8]) -> u32 {
    ((b[0] as u32) << 24) | ((b[1] as u32) << 16) | ((b[2] as u32) << 8) | b[3] as u32
}

#[must_use]
pub const fn read_u64(b: &[u8]) -> u64 {
    ((b[0] as u64) << 56)
        | ((b[1] as u64) << 48)
        | ((b[2] as u64) << 40)
        | ((b[3] as u64) << 32)
        | ((b[4] as u64) << 24)
        | ((b[5] as u64) << 16)
        | ((b[6] as u64) << 8)
        | b[7] as u64
}

pub const fn write_u16(dst: &mut [u8], v: u16) {
    let b = v.to_be_bytes();
    dst[0] = b[0];
    dst[1] = b[1];
}

pub const fn write_u24(dst: &mut [u8], v: u32) {
    let b = v.to_be_bytes();
    dst[0] = b[1];
    dst[1] = b[2];
    dst[2] = b[3];
}

pub const fn write_u32(dst: &mut [u8], v: u32) {
    let b = v.to_be_bytes();
    let mut i = 0;
    while i < 4 {
        dst[i] = b[i];
        i += 1;
    }
}

pub const fn write_u64(dst: &mut [u8], v: u64) {
    let b = v.to_be_bytes();
    let mut i = 0;
    while i < 8 {
        dst[i] = b[i];
        i += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let mut b = [0u8; 8];
        write_u64(&mut b, 0x0102_0304_0506_0708);
        assert_eq!(b, [1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(read_u64(&b), 0x0102_0304_0506_0708);
        assert_eq!(read_u32(&b), 0x0102_0304);
        assert_eq!(read_u24(&b), 0x0001_0203);
        assert_eq!(read_u16(&b), 0x0102);
    }
}
