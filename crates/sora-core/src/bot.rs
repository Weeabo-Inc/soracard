//! USB Mass Storage **Bulk-Only Transport** (BOT) framing.
//!
//! BOT is three phases per command:
//!   1. host -> device: Command Block Wrapper (CBW), 31 bytes
//!   2. data phase (IN or OUT), optional
//!   3. device -> host: Command Status Wrapper (CSW), 13 bytes
//!
//! This module is pure: it builds CBWs and parses/validates CSWs. It never
//! touches the wire, so it is testable against captured traffic and fuzzable
//! on the host.
//!
//! **Endianness:** unlike SCSI (big-endian), every BOT field is *little-endian*
//! because it rides on USB. Mixing the two is a classic bug — hence the tests.
//!
//! Reference: USB Mass Storage Class — Bulk-Only Transport, rev 1.0.

/// "USBC" as it appears on the wire (little-endian value `0x43425355`).
pub const CBW_SIGNATURE: u32 = 0x4342_5355;
/// "USBS" as it appears on the wire (little-endian value `0x53425355`).
pub const CSW_SIGNATURE: u32 = 0x5342_5355;

pub const CBW_LEN: usize = 31;
pub const CSW_LEN: usize = 13;
pub const MAX_CDB_LEN: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// Host -> device (bulk OUT).
    Out,
    /// Device -> host (bulk IN).
    In,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cbw {
    /// Echoed back in the CSW so the host can match responses to commands.
    pub tag: u32,
    /// Bytes expected in the data phase (0 for no data).
    pub transfer_len: u32,
    pub direction: Direction,
    /// Logical Unit Number, 0..=15.
    pub lun: u8,
    pub cdb: [u8; MAX_CDB_LEN],
    /// Valid CDB length, 1..=16.
    pub cdb_len: u8,
}

impl Cbw {
    /// Build a CBW for `cdb`.
    ///
    /// # Panics
    /// Panics if `cdb` is empty or longer than [`MAX_CDB_LEN`], or if `lun > 15`.
    /// These are programmer errors; the SCSI layer only emits valid CDBs.
    #[must_use]
    pub fn new(tag: u32, direction: Direction, lun: u8, transfer_len: u32, cdb: &[u8]) -> Self {
        assert!(
            !cdb.is_empty() && cdb.len() <= MAX_CDB_LEN,
            "invalid CDB length"
        );
        assert!(lun <= 15, "invalid LUN");
        let mut cdb16 = [0u8; MAX_CDB_LEN];
        cdb16[..cdb.len()].copy_from_slice(cdb);
        Self {
            tag,
            transfer_len,
            direction,
            lun,
            cdb: cdb16,
            #[allow(clippy::cast_possible_truncation)]
            cdb_len: cdb.len() as u8,
        }
    }

    /// Serialise to the 31-byte on-the-wire form (little-endian fields).
    #[must_use]
    pub fn encode(&self) -> [u8; CBW_LEN] {
        let mut b = [0u8; CBW_LEN];
        b[0..4].copy_from_slice(&CBW_SIGNATURE.to_le_bytes());
        b[4..8].copy_from_slice(&self.tag.to_le_bytes());
        b[8..12].copy_from_slice(&self.transfer_len.to_le_bytes());
        // bmCBWFlags: bit 7 = direction (0 = OUT, 1 = IN)
        b[12] = match self.direction {
            Direction::Out => 0x00,
            Direction::In => 0x80,
        };
        b[13] = self.lun & 0x0F; // bCBWLUN, top nibble reserved
        b[14] = self.cdb_len & 0x1F; // bCBWCBLength, top 3 bits reserved
        b[15..15 + MAX_CDB_LEN].copy_from_slice(&self.cdb);
        b
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum CswStatus {
    Passed = 0,
    Failed = 1,
    PhaseError = 2,
}

impl CswStatus {
    #[must_use]
    pub const fn from_byte(b: u8) -> Option<Self> {
        match b {
            0 => Some(Self::Passed),
            1 => Some(Self::Failed),
            2 => Some(Self::PhaseError),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Csw {
    pub tag: u32,
    /// Bytes not transferred. 0 == full transfer. For a short IN, the host must
    /// treat the data as short and the command as failed.
    pub residue: u32,
    pub status: CswStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BotError {
    BadCswSignature,
    BadCswStatus(u8),
    ShortCsw,
}

/// Parse and validate a 13-byte CSW (little-endian fields).
///
/// # Errors
/// The buffer is short, or the signature or status is invalid.
pub fn parse_csw(buf: &[u8]) -> Result<Csw, BotError> {
    if buf.len() < CSW_LEN {
        return Err(BotError::ShortCsw);
    }
    let sig = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
    if sig != CSW_SIGNATURE {
        return Err(BotError::BadCswSignature);
    }
    let tag = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]);
    let residue = u32::from_le_bytes([buf[8], buf[9], buf[10], buf[11]]);
    let status = CswStatus::from_byte(buf[12]).ok_or(BotError::BadCswStatus(buf[12]))?;
    Ok(Csw {
        tag,
        residue,
        status,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cbw_encoding_is_little_endian() {
        // INQUIRY (0x12), allocation length 36, expect 36 bytes IN.
        let cdb = [0x12, 0x00, 0x00, 0x00, 36, 0x00];
        let cbw = Cbw::new(0xDEAD_BEEF, Direction::In, 0, 36, &cdb);
        let b = cbw.encode();
        // "USBC" on the wire, little-endian.
        assert_eq!(&b[0..4], &[0x55, 0x53, 0x42, 0x43]);
        assert_eq!(&b[4..8], &0xDEAD_BEEFu32.to_le_bytes());
        assert_eq!(&b[8..12], &36u32.to_le_bytes());
        assert_eq!(b[12], 0x80); // IN
        assert_eq!(b[13], 0);
        assert_eq!(b[14], 6);
        assert_eq!(&b[15..21], &cdb);
    }

    #[test]
    fn csw_round_trip() {
        let mut b = [0u8; CSW_LEN];
        b[0..4].copy_from_slice(&CSW_SIGNATURE.to_le_bytes());
        b[4..8].copy_from_slice(&0x1234_5678u32.to_le_bytes());
        b[8..12].copy_from_slice(&7u32.to_le_bytes());
        b[12] = 0; // passed
        let csw = parse_csw(&b).unwrap();
        assert_eq!(csw.tag, 0x1234_5678);
        assert_eq!(csw.residue, 7);
        assert_eq!(csw.status, CswStatus::Passed);
    }

    #[test]
    fn csw_rejects_bad_signature_and_status() {
        let mut b = [0u8; CSW_LEN];
        b[0..4].copy_from_slice(&0xDEAD_BEEFu32.to_le_bytes());
        assert_eq!(parse_csw(&b), Err(BotError::BadCswSignature));
        b[0..4].copy_from_slice(&CSW_SIGNATURE.to_le_bytes());
        b[12] = 9;
        assert_eq!(parse_csw(&b), Err(BotError::BadCswStatus(9)));
    }

    #[test]
    fn short_csw_is_rejected() {
        assert_eq!(parse_csw(&[0u8; 12]), Err(BotError::ShortCsw));
    }
}
