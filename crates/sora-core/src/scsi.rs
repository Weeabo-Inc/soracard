//! The SCSI command subset a USB card reader needs, plus response parsers.
//!
//! A card reader is a "direct access block device" behind BOT. We build the
//! CDBs ourselves and parse the replies; nothing here allocates, so it is safe
//! to call from a driver at any IRQL.

use crate::be::{read_u32, write_u16, write_u32};

// ---- Opcodes -----------------------------------------------------------------
pub const OP_TEST_UNIT_READY: u8 = 0x00;
pub const OP_REQUEST_SENSE: u8 = 0x03;
pub const OP_INQUIRY: u8 = 0x12;
pub const OP_MODE_SENSE_6: u8 = 0x1A;
pub const OP_START_STOP_UNIT: u8 = 0x1B;
pub const OP_PREVENT_ALLOW_MEDIUM_REMOVAL: u8 = 0x1E;
pub const OP_READ_CAPACITY_10: u8 = 0x25;
pub const OP_READ_10: u8 = 0x28;
pub const OP_WRITE_10: u8 = 0x2A;
pub const OP_SYNCHRONIZE_CACHE_10: u8 = 0x35;
pub const OP_MODE_SENSE_10: u8 = 0x5A;
pub const OP_READ_16: u8 = 0x88;
pub const OP_WRITE_16: u8 = 0x8A;
pub const OP_READ_CAPACITY_16: u8 = 0x9E;

/// Peripheral Device Type for a direct-access (disk) device.
pub const PDT_DIRECT_ACCESS: u8 = 0x00;

/// A 16-byte CDB plus the number of bytes actually used.
pub type Cdb = [u8; 16];

#[must_use]
pub fn test_unit_ready() -> (Cdb, u8) {
    let mut c = [0u8; 16];
    c[0] = OP_TEST_UNIT_READY;
    (c, 6)
}

#[must_use]
pub fn request_sense(alloc_len: u8) -> (Cdb, u8) {
    let mut c = [0u8; 16];
    c[0] = OP_REQUEST_SENSE;
    c[4] = alloc_len;
    (c, 6)
}

#[must_use]
pub fn inquiry(alloc_len: u16) -> (Cdb, u8) {
    let mut c = [0u8; 16];
    c[0] = OP_INQUIRY;
    write_u16(&mut c[3..5], alloc_len);
    (c, 6)
}

#[must_use]
pub fn read_capacity_10() -> (Cdb, u8) {
    let mut c = [0u8; 16];
    c[0] = OP_READ_CAPACITY_10;
    (c, 10)
}

#[must_use]
pub fn read_capacity_16(alloc_len: u32) -> (Cdb, u8) {
    let mut c = [0u8; 16];
    c[0] = OP_READ_CAPACITY_16;
    c[1] = 0x10; // service action: read capacity
    write_u32(&mut c[10..14], alloc_len);
    (c, 16)
}

/// READ(10). `blocks == 0` means 256 blocks.
#[must_use]
pub fn read_10(lba: u32, blocks: u16) -> (Cdb, u8) {
    let mut c = [0u8; 16];
    c[0] = OP_READ_10;
    write_u32(&mut c[2..6], lba);
    write_u16(&mut c[7..9], blocks);
    (c, 10)
}

/// WRITE(10). `blocks == 0` means 256 blocks.
#[must_use]
pub fn write_10(lba: u32, blocks: u16) -> (Cdb, u8) {
    let mut c = [0u8; 16];
    c[0] = OP_WRITE_10;
    write_u32(&mut c[2..6], lba);
    write_u16(&mut c[7..9], blocks);
    (c, 10)
}

/// READ(16) — for cards > 2 TiB.
#[must_use]
pub fn read_16(lba: u64, blocks: u32) -> (Cdb, u8) {
    let mut c = [0u8; 16];
    c[0] = OP_READ_16;
    crate::be::write_u64(&mut c[2..10], lba);
    write_u32(&mut c[10..14], blocks);
    (c, 16)
}

#[must_use]
pub fn synchronize_cache_10(lba: u32, blocks: u16) -> (Cdb, u8) {
    let mut c = [0u8; 16];
    c[0] = OP_SYNCHRONIZE_CACHE_10;
    write_u32(&mut c[2..6], lba);
    write_u16(&mut c[7..9], blocks);
    (c, 10)
}

/// MODE SENSE(6) for one page. Page 0x3F = all pages.
#[must_use]
pub fn mode_sense_6(page: u8, alloc_len: u8) -> (Cdb, u8) {
    let mut c = [0u8; 16];
    c[0] = OP_MODE_SENSE_6;
    c[2] = page & 0x3F;
    c[4] = alloc_len;
    (c, 6)
}

/// START STOP UNIT. `loej` = load/eject, `start` = spin up.
#[must_use]
pub fn start_stop_unit(loej: bool, start: bool) -> (Cdb, u8) {
    let mut c = [0u8; 16];
    c[0] = OP_START_STOP_UNIT;
    c[4] = u8::from(loej) << 1 | u8::from(start);
    (c, 6)
}

#[must_use]
pub fn prevent_allow_medium_removal(prevent: bool) -> (Cdb, u8) {
    let mut c = [0u8; 16];
    c[0] = OP_PREVENT_ALLOW_MEDIUM_REMOVAL;
    c[4] = u8::from(prevent);
    (c, 6)
}

// ---- Response parsers --------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Inquiry {
    pub peripheral_device_type: u8,
    /// The RMB bit — "removable media". This is the bit the Realtek driver is
    /// suspected of getting wrong, so we surface it explicitly.
    pub removable: bool,
    pub scsi_version: u8,
    pub vendor: [u8; 8],
    pub product: [u8; 16],
    pub revision: [u8; 4],
}

/// Parse a standard INQUIRY response (>= 36 bytes).
#[must_use]
pub fn parse_inquiry(b: &[u8]) -> Option<Inquiry> {
    if b.len() < 36 {
        return None;
    }
    let mut vendor = [0u8; 8];
    vendor.copy_from_slice(&b[8..16]);
    let mut product = [0u8; 16];
    product.copy_from_slice(&b[16..32]);
    let mut revision = [0u8; 4];
    revision.copy_from_slice(&b[32..36]);
    Some(Inquiry {
        peripheral_device_type: b[0] & 0x1F,
        removable: b[1] & 0x80 != 0,
        scsi_version: b[2] & 0x07,
        vendor,
        product,
        revision,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sense {
    pub response_code: u8,
    pub sense_key: u8,
    pub asc: u8,
    pub ascq: u8,
}

pub const SENSE_KEY_NOT_READY: u8 = 0x02;
pub const ASC_MEDIUM_NOT_PRESENT: u8 = 0x3A;
pub const ASC_MEDIUM_MAY_HAVE_CHANGED: u8 = 0x28;
pub const ASC_UNIT_ATTENTION: u8 = 0x29; // "POWER ON, RESET, OR BUS DEVICE RESET OCCURRED"

/// Parse fixed-format sense data (0x70/0x71), as returned by REQUEST SENSE.
#[must_use]
pub fn parse_sense(b: &[u8]) -> Option<Sense> {
    if b.len() < 14 {
        return None;
    }
    let rc = b[0] & 0x7F;
    // Only fixed format (0x70/0x71). Descriptor format (0x72) is out of scope
    // for these readers but would be handled here.
    if rc != 0x70 && rc != 0x71 {
        return None;
    }
    Some(Sense {
        response_code: rc,
        sense_key: b[2] & 0x0F,
        asc: b[12],
        ascq: b[13],
    })
}

/// True when the card has been taken out.
#[must_use]
pub fn is_medium_not_present(s: &Sense) -> bool {
    s.sense_key == SENSE_KEY_NOT_READY && s.asc == ASC_MEDIUM_NOT_PRESENT
}

/// True when the card appeared or was swapped underneath us.
#[must_use]
pub fn is_medium_changed(s: &Sense) -> bool {
    s.sense_key == SENSE_KEY_UNIT_ATTENTION
        && (s.asc == ASC_MEDIUM_MAY_HAVE_CHANGED || s.asc == ASC_UNIT_ATTENTION)
}

pub const SENSE_KEY_UNIT_ATTENTION: u8 = 0x06;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capacity {
    pub last_lba: u64,
    pub block_len: u32,
}

impl Capacity {
    /// Total addressable bytes.
    #[must_use]
    pub const fn total_bytes(&self) -> u64 {
        self.last_lba
            .saturating_add(1)
            .saturating_mul(self.block_len as u64)
    }
}

#[must_use]
pub fn parse_read_capacity_10(b: &[u8]) -> Option<Capacity> {
    if b.len() < 8 {
        return None;
    }
    Some(Capacity {
        last_lba: u64::from(read_u32(&b[0..4])),
        block_len: read_u32(&b[4..8]),
    })
}

#[must_use]
pub fn parse_read_capacity_16(b: &[u8]) -> Option<Capacity> {
    if b.len() < 12 {
        return None;
    }
    Some(Capacity {
        last_lba: crate::be::read_u64(&b[0..8]),
        block_len: read_u32(&b[8..12]),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inquiry_rmb_bit() {
        let mut resp = [0u8; 36];
        resp[0] = PDT_DIRECT_ACCESS;
        resp[1] = 0x80; // RMB
        resp[2] = 0x06;
        resp[8..16].copy_from_slice(b"RSUER   ");
        resp[16..32].copy_from_slice(b"RTSUERLUN0      ");
        let inq = parse_inquiry(&resp).unwrap();
        assert_eq!(inq.peripheral_device_type, PDT_DIRECT_ACCESS);
        assert!(inq.removable, "RMB must be set for a card reader");
        assert_eq!(&inq.vendor, b"RSUER   ");
    }

    #[test]
    fn read_capacity_10_math() {
        let mut b = [0u8; 8];
        write_u32(&mut b[0..4], 999); // last LBA
        write_u32(&mut b[4..8], 512);
        let cap = parse_read_capacity_10(&b).unwrap();
        assert_eq!(cap.last_lba, 999);
        assert_eq!(cap.block_len, 512);
        assert_eq!(cap.total_bytes(), 1000 * 512);
    }

    #[test]
    fn read10_lba_and_blocks_are_be() {
        let (c, n) = read_10(0x0012_3456, 1);
        assert_eq!(n, 10);
        assert_eq!(c[0], OP_READ_10);
        assert_eq!(&c[2..6], &[0x00, 0x12, 0x34, 0x56]);
        assert_eq!(&c[7..9], &[0x00, 0x01]);
    }

    #[test]
    fn sense_medium_not_present_and_changed() {
        // 0x70, key NOT READY, ASC 0x3A
        let mut s = [0u8; 18];
        s[0] = 0x70;
        s[2] = SENSE_KEY_NOT_READY;
        s[12] = ASC_MEDIUM_NOT_PRESENT;
        let sense = parse_sense(&s).unwrap();
        assert!(is_medium_not_present(&sense));
        assert!(!is_medium_changed(&sense));

        s[2] = SENSE_KEY_UNIT_ATTENTION;
        s[12] = ASC_MEDIUM_MAY_HAVE_CHANGED;
        let sense = parse_sense(&s).unwrap();
        assert!(is_medium_changed(&sense));
    }
}
