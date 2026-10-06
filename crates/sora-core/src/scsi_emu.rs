//! Commands the driver answers itself instead of forwarding to the reader.
//!
//! Card-reader firmware implements a small SCSI subset and is known to hang or
//! stall on commands outside it. The Windows storage stack probes with a few of
//! those (`REPORT LUNS`, `INQUIRY` VPD pages), so — like the inbox `USBSTOR` —
//! we answer them locally and pass everything else through untouched.
//!
//! Also here: the fixed-format sense builder for locally failed commands and
//! the `INQUIRY` removable-media (RMB) override the RTS5129 needs so Windows
//! treats the LUN as removable and runs media-change detection on it.

use crate::be::{read_u16, read_u32, write_u32};
use crate::scsi::OP_INQUIRY;

pub const OP_REPORT_LUNS: u8 = 0xA0;

pub const SENSE_KEY_ILLEGAL_REQUEST: u8 = 0x05;
pub const ASC_INVALID_FIELD_IN_CDB: u8 = 0x24;
pub const SENSE_KEY_DATA_PROTECT: u8 = 0x07;
pub const ASC_WRITE_PROTECTED: u8 = 0x27;

/// Fixed-format sense data length we produce and request.
pub const SENSE_LEN: usize = 18;

/// What the driver should do with a CDB.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    /// Send it to the device over BOT.
    PassThrough,
    /// Answer `REPORT LUNS` locally: a single LUN 0.
    ReportLuns,
    /// Answer `INQUIRY` VPD page 0x00 (supported pages) locally.
    VpdSupportedPages,
    /// Fail locally with ILLEGAL REQUEST / INVALID FIELD IN CDB.
    Reject,
}

/// Decide how to handle `cdb`.
#[must_use]
pub fn route(cdb: &[u8]) -> Route {
    match cdb.first() {
        Some(&OP_REPORT_LUNS) => Route::ReportLuns,
        Some(&OP_INQUIRY) if cdb.len() >= 6 => {
            let evpd = cdb[1] & 0x01 != 0;
            let page = cdb[2];
            match (evpd, page) {
                (false, 0) => Route::PassThrough,
                (true, 0) => Route::VpdSupportedPages,
                _ => Route::Reject,
            }
        }
        _ => Route::PassThrough,
    }
}

/// Allocation length of a locally answered CDB (0 if not applicable).
#[must_use]
pub fn allocation_len(cdb: &[u8]) -> u32 {
    match cdb.first() {
        Some(&OP_REPORT_LUNS) if cdb.len() >= 10 => read_u32(&cdb[6..10]),
        Some(&OP_INQUIRY) if cdb.len() >= 5 => u32::from(read_u16(&cdb[3..5])),
        _ => 0,
    }
}

/// Copy as much of `src` as fits in `dst` and `alloc_len`; return bytes written.
fn emit(dst: &mut [u8], src: &[u8], alloc_len: u32) -> usize {
    let n = src
        .len()
        .min(dst.len())
        .min(usize::try_from(alloc_len).unwrap_or(usize::MAX));
    dst[..n].copy_from_slice(&src[..n]);
    n
}

/// `REPORT LUNS` response listing only LUN 0. Returns bytes written.
pub fn report_luns(dst: &mut [u8], alloc_len: u32) -> usize {
    let mut r = [0u8; 16];
    write_u32(&mut r[0..4], 8); // LUN list length: one 8-byte entry
    emit(dst, &r, alloc_len)
}

/// `INQUIRY` VPD page 0x00 advertising only itself. Returns bytes written.
pub fn vpd_supported_pages(dst: &mut [u8], alloc_len: u32, pdt: u8) -> usize {
    let r = [pdt & 0x1F, 0x00, 0x00, 0x01, 0x00];
    emit(dst, &r, alloc_len)
}

/// Fixed-format (0x70, current) sense data.
#[must_use]
pub fn fixed_sense(key: u8, asc: u8, ascq: u8) -> [u8; SENSE_LEN] {
    let mut s = [0u8; SENSE_LEN];
    s[0] = 0x70;
    s[2] = key & 0x0F;
    #[allow(clippy::cast_possible_truncation)]
    {
        s[7] = (SENSE_LEN - 8) as u8; // additional sense length
    }
    s[12] = asc;
    s[13] = ascq;
    s
}

/// Set the RMB (removable medium) bit in standard `INQUIRY` data in place.
/// No-op if the buffer is too short to hold byte 1.
pub fn set_removable(inquiry: &mut [u8]) {
    if let Some(b) = inquiry.get_mut(1) {
        *b |= 0x80;
    }
}

/// True for commands that modify the medium (used to enforce read-only mode).
#[must_use]
pub const fn is_write_command(op: u8) -> bool {
    matches!(
        op,
        0x04 // FORMAT UNIT
            | 0x0A // WRITE(6)
            | 0x2A // WRITE(10)
            | 0x2E // WRITE AND VERIFY(10)
            | 0x41 // WRITE SAME(10)
            | 0x42 // UNMAP
            | 0x8A // WRITE(16)
            | 0x8E // WRITE AND VERIFY(16)
            | 0x93 // WRITE SAME(16)
            | 0xAA // WRITE(12)
            | 0xAE // WRITE AND VERIFY(12)
    )
}

/// Set the write-protect (WP) bit in a MODE SENSE(6)/(10) response header in
/// place, so Windows mounts the medium read-only. No-op for other opcodes or
/// a buffer too short to hold the header byte.
pub fn set_mode_sense_wp(op: u8, data: &mut [u8]) {
    let idx = match op {
        crate::scsi::OP_MODE_SENSE_6 => 2,
        crate::scsi::OP_MODE_SENSE_10 => 3,
        _ => return,
    };
    if let Some(b) = data.get_mut(idx) {
        *b |= 0x80;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes() {
        assert_eq!(route(&[0x12, 0, 0, 0, 36, 0]), Route::PassThrough);
        assert_eq!(route(&[0x12, 1, 0, 0, 0xFF, 0]), Route::VpdSupportedPages);
        assert_eq!(route(&[0x12, 1, 0x80, 0, 0xFF, 0]), Route::Reject);
        assert_eq!(route(&[0x12, 0, 0x80, 0, 0xFF, 0]), Route::Reject);
        assert_eq!(
            route(&[0xA0, 0, 0, 0, 0, 0, 0, 0, 0x10, 0, 0, 0]),
            Route::ReportLuns
        );
        assert_eq!(
            route(&[0x28, 0, 0, 0, 0, 0, 0, 0, 1, 0]),
            Route::PassThrough
        );
        assert_eq!(route(&[0x00, 0, 0, 0, 0, 0]), Route::PassThrough);
        assert_eq!(route(&[]), Route::PassThrough);
    }

    #[test]
    fn report_luns_is_single_lun0_and_respects_alloc_len() {
        let cdb = [0xA0, 0, 0, 0, 0, 0, 0, 0, 0, 16, 0, 0];
        assert_eq!(allocation_len(&cdb), 16);
        let mut buf = [0xEEu8; 32];
        assert_eq!(report_luns(&mut buf, 16), 16);
        assert_eq!(
            &buf[..16],
            &[0, 0, 0, 8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]
        );
        assert_eq!(buf[16], 0xEE);
        assert_eq!(report_luns(&mut buf, 4), 4);
        assert_eq!(report_luns(&mut buf[..2], 16), 2);
    }

    #[test]
    fn vpd_page0() {
        let cdb = [0x12, 1, 0, 0, 0xFF, 0];
        assert_eq!(allocation_len(&cdb), 0xFF);
        let mut buf = [0u8; 64];
        assert_eq!(vpd_supported_pages(&mut buf, 0xFF, 0), 5);
        assert_eq!(&buf[..5], &[0, 0, 0, 1, 0]);
    }

    #[test]
    fn sense_layout() {
        let s = fixed_sense(SENSE_KEY_ILLEGAL_REQUEST, ASC_INVALID_FIELD_IN_CDB, 0);
        let parsed = crate::scsi::parse_sense(&s).unwrap();
        assert_eq!(parsed.sense_key, 5);
        assert_eq!(parsed.asc, 0x24);
        assert_eq!(s[7], 10);
    }

    #[test]
    fn write_protect_helpers() {
        assert!(is_write_command(0x2A));
        assert!(is_write_command(0x8A));
        assert!(!is_write_command(0x28));
        assert!(!is_write_command(0x35)); // SYNCHRONIZE CACHE is harmless
        let mut ms6 = [3u8, 0, 0, 0];
        set_mode_sense_wp(0x1A, &mut ms6);
        assert_eq!(ms6[2], 0x80);
        let mut ms10 = [0u8, 6, 0, 0, 0, 0, 0, 0];
        set_mode_sense_wp(0x5A, &mut ms10);
        assert_eq!(ms10[3], 0x80);
        let mut other = [0u8; 4];
        set_mode_sense_wp(0x28, &mut other);
        assert_eq!(other, [0; 4]);
        set_mode_sense_wp(0x1A, &mut [0u8; 2]);
    }

    #[test]
    fn rmb_patch() {
        let mut inq = [0u8, 0x00, 0x02, 0x02, 31];
        set_removable(&mut inq);
        assert_eq!(inq[1], 0x80);
        let mut tiny = [0u8; 1];
        set_removable(&mut tiny);
        assert_eq!(tiny, [0]);
    }
}
