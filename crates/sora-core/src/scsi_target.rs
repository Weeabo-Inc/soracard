//! The SCSI *target* side: answering the storage stack's commands ourselves.
//!
//! Readers like the RTS5129 have no SCSI firmware — the host drives the SD
//! card directly — so the driver must present a SCSI direct-access device and
//! translate. This module decides every command that does not touch sectors
//! and hands sector I/O to the backend as [`Reply::Io`].
//!
//! It also owns the media-change semantics that make plug and play work:
//! `disk.sys`/`classpnp` poll TEST UNIT READY about once a second on
//! removable media; we answer NOT READY / MEDIUM NOT PRESENT while the slot is
//! empty, and a one-shot UNIT ATTENTION / MEDIUM MAY HAVE CHANGED after a card
//! appears, which makes Windows re-read capacity and remount.
//!
//! Pure, allocation-free, host-tested.

use crate::be::{read_u16, read_u32, read_u64, write_u32, write_u64};
use crate::scsi;
use crate::scsi_emu;

pub const OP_READ_12: u8 = 0xA8;
pub const OP_WRITE_12: u8 = 0xAA;
pub const OP_VERIFY_10: u8 = 0x2F;
pub const OP_SYNCHRONIZE_CACHE_16: u8 = 0x91;
pub const OP_SERVICE_ACTION_IN_16: u8 = 0x9E;

const SENSE_NOT_READY: (u8, u8, u8) = (0x02, 0x3A, 0x00); // medium not present
const SENSE_BECOMING_READY: (u8, u8, u8) = (0x02, 0x04, 0x01);
const SENSE_MEDIUM_CHANGED: (u8, u8, u8) = (0x06, 0x28, 0x00);
const SENSE_INVALID_OPCODE: (u8, u8, u8) = (0x05, 0x20, 0x00);
const SENSE_INVALID_FIELD: (u8, u8, u8) = (0x05, 0x24, 0x00);
const SENSE_LBA_OUT_OF_RANGE: (u8, u8, u8) = (0x05, 0x21, 0x00);
const SENSE_WRITE_PROTECTED: (u8, u8, u8) = (0x07, 0x27, 0x00);

/// What the backend knows about the slot right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Media {
    /// A card is physically present.
    pub present: bool,
    /// The card is initialised and `blocks`/`block_len` are valid.
    pub ready: bool,
    pub blocks: u64,
    pub block_len: u32,
    pub write_protected: bool,
}

/// Outcome of one command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reply {
    /// Success with `n` bytes of data-in written to the buffer.
    Data(usize),
    /// Success, no data.
    Good,
    /// CHECK CONDITION with this fixed-format sense (key, asc, ascq).
    Check(u8, u8, u8),
    /// Sector I/O for the backend (already validated against capacity).
    Io { write: bool, lba: u64, blocks: u32 },
}

/// Identity strings reported by INQUIRY (space padded).
pub const VENDOR: &[u8; 8] = b"SoraCard";
pub const PRODUCT: &[u8; 16] = b"SD Card Reader  ";
pub const REVISION: &[u8; 4] = b"0.1 ";

/// Per-LUN target state: media-change tracking and the last sense data.
#[derive(Debug, Clone, Copy, Default)]
pub struct Target {
    was_present: bool,
    unit_attention: bool,
    sense: (u8, u8, u8),
}

impl Target {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            was_present: false,
            unit_attention: false,
            sense: (0, 0, 0),
        }
    }

    /// Feed the latest presence sample; arms UNIT ATTENTION on insertion.
    /// Returns true when presence changed.
    pub fn observe(&mut self, present: bool) -> bool {
        let changed = present != self.was_present;
        if changed && present {
            self.unit_attention = true;
        }
        if changed && !present {
            // A removal is reported as NOT READY; drop any pending attention.
            self.unit_attention = false;
        }
        self.was_present = present;
        changed
    }

    fn check(&mut self, s: (u8, u8, u8)) -> Reply {
        self.sense = s;
        Reply::Check(s.0, s.1, s.2)
    }

    fn not_ready(&mut self, m: &Media) -> Reply {
        self.check(if m.present {
            SENSE_BECOMING_READY
        } else {
            SENSE_NOT_READY
        })
    }

    /// Decide one command. `buf` receives data-in (its length is the SRB's
    /// transfer length); `buf` is not touched for data-out commands.
    pub fn handle(&mut self, cdb: &[u8], m: &Media, buf: &mut [u8]) -> Reply {
        let Some(&op) = cdb.first() else {
            return self.check(SENSE_INVALID_OPCODE);
        };

        // Commands that neither report nor clear a pending UNIT ATTENTION.
        match op {
            scsi::OP_INQUIRY => return self.inquiry(cdb, buf),
            scsi::OP_REQUEST_SENSE => return self.request_sense(cdb, buf),
            scsi_emu::OP_REPORT_LUNS => {
                let n = scsi_emu::report_luns(buf, scsi_emu::allocation_len(cdb));
                return Reply::Data(n);
            }
            _ => {}
        }
        if self.unit_attention {
            self.unit_attention = false;
            return self.check(SENSE_MEDIUM_CHANGED);
        }
        self.sense = (0, 0, 0);

        match op {
            scsi::OP_READ_CAPACITY_10 => {
                if !m.ready {
                    return self.not_ready(m);
                }
                let last = m.blocks.saturating_sub(1);
                let mut r = [0u8; 8];
                #[allow(clippy::cast_possible_truncation)]
                write_u32(
                    &mut r[0..4],
                    if last > 0xFFFF_FFFE {
                        0xFFFF_FFFF
                    } else {
                        last as u32
                    },
                );
                write_u32(&mut r[4..8], m.block_len);
                Reply::Data(copy(buf, &r))
            }
            OP_SERVICE_ACTION_IN_16 if cdb.len() >= 16 && cdb[1] & 0x1F == 0x10 => {
                if !m.ready {
                    return self.not_ready(m);
                }
                let mut r = [0u8; 32];
                write_u64(&mut r[0..8], m.blocks.saturating_sub(1));
                write_u32(&mut r[8..12], m.block_len);
                let alloc = read_u32(&cdb[10..14]) as usize;
                Reply::Data(copy(head(buf, alloc), &r))
            }
            scsi::OP_MODE_SENSE_6 | scsi::OP_MODE_SENSE_10 => Self::mode_sense(cdb, m, buf),
            scsi::OP_PREVENT_ALLOW_MEDIUM_REMOVAL | scsi::OP_START_STOP_UNIT => Reply::Good,
            scsi::OP_TEST_UNIT_READY
            | scsi::OP_SYNCHRONIZE_CACHE_10
            | OP_SYNCHRONIZE_CACHE_16
            | OP_VERIFY_10 => {
                if m.ready {
                    Reply::Good
                } else {
                    self.not_ready(m)
                }
            }
            scsi::OP_READ_10
            | scsi::OP_WRITE_10
            | OP_READ_12
            | OP_WRITE_12
            | scsi::OP_READ_16
            | scsi::OP_WRITE_16 => self.rw(op, cdb, m),
            _ => self.check(SENSE_INVALID_OPCODE),
        }
    }

    fn inquiry(&mut self, cdb: &[u8], buf: &mut [u8]) -> Reply {
        let alloc = scsi_emu::allocation_len(cdb) as usize;
        let out = head(buf, alloc);
        match scsi_emu::route(cdb) {
            scsi_emu::Route::VpdSupportedPages => Reply::Data(scsi_emu::vpd_supported_pages(
                out,
                u32::MAX,
                scsi::PDT_DIRECT_ACCESS,
            )),
            scsi_emu::Route::PassThrough => {
                let mut r = [0u8; 36];
                r[0] = scsi::PDT_DIRECT_ACCESS;
                r[1] = 0x80; // RMB: removable medium
                r[2] = 0x02; // SCSI-2: keeps the stack on the simple command set
                r[3] = 0x02; // response data format
                r[4] = 31; // additional length
                r[8..16].copy_from_slice(VENDOR);
                r[16..32].copy_from_slice(PRODUCT);
                r[32..36].copy_from_slice(REVISION);
                Reply::Data(copy(out, &r))
            }
            _ => self.check(SENSE_INVALID_FIELD),
        }
    }

    fn request_sense(&mut self, cdb: &[u8], buf: &mut [u8]) -> Reply {
        let alloc = usize::from(*cdb.get(4).unwrap_or(&0));
        let s = scsi_emu::fixed_sense(self.sense.0, self.sense.1, self.sense.2);
        self.sense = (0, 0, 0);
        Reply::Data(copy(head(buf, alloc), &s))
    }

    fn mode_sense(cdb: &[u8], m: &Media, buf: &mut [u8]) -> Reply {
        let wp = if m.write_protected { 0x80 } else { 0x00 };
        // Header only (no block descriptors, no pages). Windows reads the WP
        // bit from here and tolerates missing pages.
        if cdb[0] == scsi::OP_MODE_SENSE_6 {
            let alloc = usize::from(*cdb.get(4).unwrap_or(&0));
            let r = [3u8, 0, wp, 0];
            Reply::Data(copy(head(buf, alloc), &r))
        } else {
            let alloc = if cdb.len() >= 9 {
                usize::from(read_u16(&cdb[7..9]))
            } else {
                0
            };
            let r = [0u8, 6, 0, wp, 0, 0, 0, 0];
            Reply::Data(copy(head(buf, alloc), &r))
        }
    }

    fn rw(&mut self, op: u8, cdb: &[u8], m: &Media) -> Reply {
        let write = matches!(op, scsi::OP_WRITE_10 | OP_WRITE_12 | scsi::OP_WRITE_16);
        let (lba, blocks) = match op {
            scsi::OP_READ_10 | scsi::OP_WRITE_10 if cdb.len() >= 10 => (
                u64::from(read_u32(&cdb[2..6])),
                u32::from(read_u16(&cdb[7..9])),
            ),
            OP_READ_12 | OP_WRITE_12 if cdb.len() >= 12 => {
                (u64::from(read_u32(&cdb[2..6])), read_u32(&cdb[6..10]))
            }
            scsi::OP_READ_16 | scsi::OP_WRITE_16 if cdb.len() >= 16 => {
                (read_u64(&cdb[2..10]), read_u32(&cdb[10..14]))
            }
            _ => return self.check(SENSE_INVALID_FIELD),
        };
        if !m.ready {
            return self.not_ready(m);
        }
        if write && m.write_protected {
            return self.check(SENSE_WRITE_PROTECTED);
        }
        if blocks == 0 {
            return Reply::Good;
        }
        if lba
            .checked_add(u64::from(blocks))
            .is_none_or(|end| end > m.blocks)
        {
            return self.check(SENSE_LBA_OUT_OF_RANGE);
        }
        Reply::Io { write, lba, blocks }
    }
}

/// The first `n` bytes of `buf` (or all of it, if shorter).
fn head(buf: &mut [u8], n: usize) -> &mut [u8] {
    let len = buf.len();
    &mut buf[..n.min(len)]
}

fn copy(dst: &mut [u8], src: &[u8]) -> usize {
    let n = dst.len().min(src.len());
    dst[..n].copy_from_slice(&src[..n]);
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ready(blocks: u64) -> Media {
        Media {
            present: true,
            ready: true,
            blocks,
            block_len: 512,
            write_protected: false,
        }
    }

    #[test]
    fn empty_slot_is_medium_not_present() {
        let mut t = Target::new();
        t.observe(false);
        let mut b = [0u8; 64];
        assert_eq!(
            t.handle(&[0, 0, 0, 0, 0, 0], &Media::default(), &mut b),
            Reply::Check(2, 0x3A, 0)
        );
        // REQUEST SENSE returns it, then clears it.
        assert_eq!(
            t.handle(&[3, 0, 0, 0, 18, 0], &Media::default(), &mut b),
            Reply::Data(18)
        );
        assert_eq!((b[2], b[12]), (2, 0x3A));
    }

    #[test]
    fn insertion_raises_one_unit_attention() {
        let mut t = Target::new();
        t.observe(false);
        assert!(t.observe(true));
        let m = ready(1000);
        let mut b = [0u8; 64];
        // INQUIRY does not consume it.
        assert!(matches!(
            t.handle(&[0x12, 0, 0, 0, 36, 0], &m, &mut b),
            Reply::Data(36)
        ));
        assert_eq!(
            t.handle(&[0, 0, 0, 0, 0, 0], &m, &mut b),
            Reply::Check(6, 0x28, 0)
        );
        assert_eq!(t.handle(&[0, 0, 0, 0, 0, 0], &m, &mut b), Reply::Good);
    }

    #[test]
    fn removal_cancels_attention_and_reports_not_ready() {
        let mut t = Target::new();
        t.observe(true);
        t.observe(false);
        let mut b = [0u8; 8];
        assert_eq!(
            t.handle(&[0, 0, 0, 0, 0, 0], &Media::default(), &mut b),
            Reply::Check(2, 0x3A, 0)
        );
    }

    #[test]
    fn present_but_not_initialised_is_becoming_ready() {
        let mut t = Target::new();
        let m = Media {
            present: true,
            ..Media::default()
        };
        let mut b = [0u8; 8];
        assert_eq!(
            t.handle(&[0x25, 0, 0, 0, 0, 0, 0, 0, 0, 0], &m, &mut b),
            Reply::Check(2, 4, 1)
        );
    }

    #[test]
    fn inquiry_is_removable_disk() {
        let mut t = Target::new();
        let mut b = [0u8; 96];
        assert_eq!(
            t.handle(&[0x12, 0, 0, 0, 96, 0], &Media::default(), &mut b),
            Reply::Data(36)
        );
        assert_eq!((b[0], b[1], b[4]), (0, 0x80, 31));
        assert_eq!(&b[8..16], VENDOR);
        assert_eq!(
            t.handle(&[0x12, 1, 0, 0, 96, 0], &Media::default(), &mut b),
            Reply::Data(5)
        );
        assert_eq!(
            t.handle(&[0x12, 1, 0x83, 0, 96, 0], &Media::default(), &mut b),
            Reply::Check(5, 0x24, 0)
        );
    }

    #[test]
    fn capacity_reports_last_lba() {
        let mut t = Target::new();
        let mut b = [0u8; 8];
        assert_eq!(
            t.handle(
                &[0x25, 0, 0, 0, 0, 0, 0, 0, 0, 0],
                &ready(244_277_248),
                &mut b
            ),
            Reply::Data(8)
        );
        assert_eq!(b, [0x0E, 0x8F, 0x5F, 0xFF, 0, 0, 2, 0]); // last LBA 244_277_247
    }

    #[test]
    fn mode_sense_wp_bit() {
        let mut t = Target::new();
        let mut m = ready(100);
        let mut b = [0u8; 8];
        assert_eq!(
            t.handle(&[0x1A, 0, 0x3F, 0, 4, 0], &m, &mut b),
            Reply::Data(4)
        );
        assert_eq!(b[2], 0);
        m.write_protected = true;
        t.handle(&[0x1A, 0, 0x3F, 0, 4, 0], &m, &mut b);
        assert_eq!(b[2], 0x80);
        assert_eq!(
            t.handle(&[0x5A, 0, 0x3F, 0, 0, 0, 0, 0, 8, 0], &m, &mut b),
            Reply::Data(8)
        );
        assert_eq!(b[3], 0x80);
    }

    #[test]
    fn rw_validation() {
        let mut t = Target::new();
        let m = ready(1000);
        let mut b = [0u8; 0];
        // READ(10) lba 10, 8 blocks.
        assert_eq!(
            t.handle(&[0x28, 0, 0, 0, 0, 10, 0, 0, 8, 0], &m, &mut b),
            Reply::Io {
                write: false,
                lba: 10,
                blocks: 8
            }
        );
        // Past the end.
        assert_eq!(
            t.handle(&[0x28, 0, 0, 0, 0x03, 0xE5, 0, 0, 8, 0], &m, &mut b),
            Reply::Check(5, 0x21, 0)
        );
        // Write-protected write.
        let wp = Media {
            write_protected: true,
            ..m
        };
        assert_eq!(
            t.handle(&[0x2A, 0, 0, 0, 0, 0, 0, 0, 1, 0], &wp, &mut b),
            Reply::Check(7, 0x27, 0)
        );
        // Zero-length is a no-op success.
        assert_eq!(
            t.handle(&[0x28, 0, 0, 0, 0, 0, 0, 0, 0, 0], &m, &mut b),
            Reply::Good
        );
        // Unknown opcode.
        assert_eq!(
            t.handle(&[0x04, 0, 0, 0, 0, 0], &m, &mut b),
            Reply::Check(5, 0x20, 0)
        );
    }
}
