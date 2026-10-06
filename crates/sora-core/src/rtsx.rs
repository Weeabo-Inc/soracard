//! Realtek USB card-reader register protocol ("RTCR" packets).
//!
//! The RTS5129/5139 family is *not* a mass-storage device: it has no SCSI
//! firmware. The host reads and writes the controller's registers and runs the
//! SD protocol itself. Register access goes two ways:
//!
//! * **Control pipe (ep0)**: single-register read/write and a card-status poll
//!   ([`ep0_write_register`], [`ep0_read_register`], [`ep0_poll_status`]).
//! * **Bulk pipes**: a command packet on bulk OUT carrying a batch of
//!   register operations, optionally followed by a response on bulk IN
//!   ([`Batch`]).
//!
//! Packet layout (observed protocol, all multi-byte fields big-endian):
//!
//! ```text
//! [0..4]  'R' 'T' 'C' 'R'
//! [4]     packet type (0 = batch of register ops)
//! [5..7]  op count
//! [7]     stage flags: 0x01 response follows, 0x02 data in, 0x04 data out
//! [8..]   ops, 4 bytes each: [type<<6 | addr[13:8], addr[7:0], mask, value]
//!         type 0 = read, 1 = write, 2 = check (wait until reg & mask == value)
//! ```
//!
//! The response to a batch is one byte per *read or check* op, in order,
//! padded to a multiple of four bytes (a check op reports the register's
//! final value).
//!
//! Pure and allocation-free, like the rest of `sora-core`.

/// Packet signature.
pub const SIGNATURE: [u8; 4] = *b"RTCR";
/// Header length in bytes.
pub const HEADER_LEN: usize = 8;
/// Largest packet the driver sends (the controller's command buffer).
pub const MAX_PACKET: usize = 1024;
/// Most ops that fit in one packet.
pub const MAX_OPS: usize = (MAX_PACKET - HEADER_LEN) / 4;

/// Stage flag: a response (one byte per read op) follows on bulk IN.
pub const STAGE_RESPONSE: u8 = 0x01;
/// Stage flag: a data-in phase follows.
pub const STAGE_DATA_IN: u8 = 0x02;
/// Stage flag: a data-out phase follows.
pub const STAGE_DATA_OUT: u8 = 0x04;

const PKT_BATCH: u8 = 0;

/// Register operation kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum OpKind {
    Read = 0,
    Write = 1,
    /// Wait (in the controller) until `reg & mask == value`.
    Check = 2,
}

/// Why a batch could not take another op.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BatchError {
    /// The packet is full.
    Full,
    /// Register address outside the 14-bit register space.
    BadAddress,
}

/// Registers are 14-bit addresses in the 0xC000..=0xFFFF window.
const fn valid_addr(addr: u16) -> bool {
    addr >= 0xC000
}

/// A batch command packet under construction.
#[derive(Clone, Copy)]
pub struct Batch {
    buf: [u8; MAX_PACKET],
    ops: usize,
    reads: usize,
}

impl Default for Batch {
    fn default() -> Self {
        Self::new()
    }
}

impl Batch {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            buf: [0; MAX_PACKET],
            ops: 0,
            reads: 0,
        }
    }

    /// Append a register op.
    ///
    /// # Errors
    /// [`BatchError::Full`] when the packet is full, [`BatchError::BadAddress`]
    /// for an address outside the register window.
    pub fn op(
        &mut self,
        kind: OpKind,
        addr: u16,
        mask: u8,
        value: u8,
    ) -> Result<&mut Self, BatchError> {
        if self.ops == MAX_OPS {
            return Err(BatchError::Full);
        }
        if !valid_addr(addr) {
            return Err(BatchError::BadAddress);
        }
        let at = HEADER_LEN + self.ops * 4;
        let [hi, lo] = addr.to_be_bytes();
        self.buf[at] = ((kind as u8) << 6) | (hi & 0x3F);
        self.buf[at + 1] = lo;
        self.buf[at + 2] = mask;
        self.buf[at + 3] = value;
        self.ops += 1;
        if matches!(kind, OpKind::Read | OpKind::Check) {
            self.reads += 1;
        }
        Ok(self)
    }

    /// Convenience: masked register write.
    ///
    /// # Errors
    /// As [`Batch::op`].
    pub fn write(&mut self, addr: u16, mask: u8, value: u8) -> Result<&mut Self, BatchError> {
        self.op(OpKind::Write, addr, mask, value)
    }

    /// Convenience: register read (one response byte).
    ///
    /// Check ops also produce a response byte (see module docs).
    ///
    /// # Errors
    /// As [`Batch::op`].
    pub fn read(&mut self, addr: u16) -> Result<&mut Self, BatchError> {
        self.op(OpKind::Read, addr, 0, 0)
    }

    /// Convenience: controller-side wait until `reg & mask == value`.
    ///
    /// # Errors
    /// As [`Batch::op`].
    pub fn check(&mut self, addr: u16, mask: u8, value: u8) -> Result<&mut Self, BatchError> {
        self.op(OpKind::Check, addr, mask, value)
    }

    #[must_use]
    pub const fn ops(&self) -> usize {
        self.ops
    }

    /// Number of response bytes the read and check ops produce.
    #[must_use]
    pub const fn reads(&self) -> usize {
        self.reads
    }

    /// Bulk IN length to request for the response (reads rounded up to 4).
    #[must_use]
    pub const fn response_len(&self) -> usize {
        (self.reads + 3) & !3
    }

    /// Finish the packet. `stage` is a combination of the `STAGE_*` flags;
    /// [`STAGE_RESPONSE`] is added automatically when the batch has reads.
    /// Returns the bytes to send on bulk OUT.
    pub fn encode(&mut self, stage: u8) -> &[u8] {
        self.buf[0..4].copy_from_slice(&SIGNATURE);
        self.buf[4] = PKT_BATCH;
        #[allow(clippy::cast_possible_truncation)] // ops <= MAX_OPS < 65536
        let count = (self.ops as u16).to_be_bytes();
        self.buf[5] = count[0];
        self.buf[6] = count[1];
        self.buf[7] = stage | if self.reads > 0 { STAGE_RESPONSE } else { 0 };
        &self.buf[..HEADER_LEN + self.ops * 4]
    }
}

/// Setup packet for an ep0 register write (`reg = (reg & !mask) | (value & mask)`).
#[must_use]
pub const fn ep0_write_register(addr: u16, mask: u8, value: u8) -> [u8; 8] {
    let [hi, lo] = addr.to_be_bytes();
    // bmRequestType vendor/device/OUT, bRequest 0; wValue carries the address
    // (write flag 0xC0 in the high byte) and wIndex the mask and value, in the
    // byte order the controller expects on the wire.
    [0x40, 0x00, 0xC0 | (hi & 0x3F), lo, mask, value, 0, 0]
}

/// Setup packet for an ep0 register read (one data byte returned).
#[must_use]
pub const fn ep0_read_register(addr: u16) -> [u8; 8] {
    let [hi, lo] = addr.to_be_bytes();
    [0xC0, 0x00, 0x80 | (hi & 0x3F), lo, 0, 0, 1, 0]
}

/// Setup packet for the card-status poll (two data bytes returned).
#[must_use]
pub const fn ep0_poll_status() -> [u8; 8] {
    [0xC0, 0x02, 0, 0, 0, 0, 2, 0]
}

/// Cards reported by the status poll (one flag per hardware status bit).
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CardStatus {
    pub sd: bool,
    pub ms: bool,
    pub xd: bool,
    /// The card's write-protect switch is set.
    pub write_protect: bool,
}

/// Parse the two-byte status poll response.
#[must_use]
pub fn parse_status(b: &[u8]) -> Option<CardStatus> {
    let s = u16::from_le_bytes([*b.first()?, *b.get(1)?]);
    Some(CardStatus {
        sd: s & 0x01 != 0,
        ms: s & 0x02 != 0,
        xd: s & 0x04 != 0,
        write_protect: s & 0x08 != 0,
    })
}

/// Controller registers used by the driver.
pub mod reg {
    /// Clear FSM error (write 0xF8/0xF8 after any failed bulk transfer).
    pub const SFSM_ED: u16 = 0xFC04;
    pub const FPDCTL: u16 = 0xFC00;
    pub const HW_VERSION: u16 = 0xFC01;
    pub const CLK_DIV: u16 = 0xFC03;
    pub const CFG_MODE: u16 = 0xFC0E;
    pub const CFG_MODE_1: u16 = 0xFC0F;
    pub const SYS_DUMMY0: u16 = 0xFC30;
    pub const CD_DEGLITCH_WIDTH: u16 = 0xFC20;
    pub const CD_DEGLITCH_EN: u16 = 0xFC21;
    pub const CARD_SHARE_MODE: u16 = 0xFD51;
    pub const CARD_DRIVE_SEL: u16 = 0xFD52;
    pub const SD30_DRIVE_SEL: u16 = 0xFD57;
    pub const CARD_DMA1_CTL: u16 = 0xFD5C;
    pub const CARD_EXIST: u16 = 0xFD6F;
    pub const CARD_INT_PEND: u16 = 0xFD71;
    pub const LDO_POWER_CFG: u16 = 0xFD7B;
    pub const OCPCTL: u16 = 0xFD80;
    pub const OCPPARA1: u16 = 0xFD81;
    pub const OCPPARA2: u16 = 0xFD82;
    pub const OCPSTAT: u16 = 0xFD83;

    // Clock generation.
    pub const SSC_DIV_N_0: u16 = 0xFC07;
    pub const SSC_CTL1: u16 = 0xFC09;
    pub const SSC_CTL2: u16 = 0xFC0A;
    pub const CARD_CLK_SOURCE: u16 = 0xFC2E;
    /// Variable-phase clocks: TX phase (and phase-generator reset) / RX phase.
    pub const SD_VPCLK0_CTL: u16 = 0xFC2A;
    pub const SD_VPCLK1_CTL: u16 = 0xFC2B;

    // Card slot control.
    pub const CARD_STOP: u16 = 0xFD53;
    pub const CARD_OE: u16 = 0xFD54;
    pub const CARD_DATA_SOURCE: u16 = 0xFD5D;
    pub const CARD_SELECT: u16 = 0xFD5E;
    pub const CARD_PULL_CTL1: u16 = 0xFD60;
    pub const CARD_PULL_CTL5: u16 = 0xFD64;
    pub const CARD_PULL_CTL6: u16 = 0xFD65;
    pub const CARD_CLK_EN: u16 = 0xFD79;
    pub const CARD_PWR_CTL: u16 = 0xFD7A;

    // SD host.
    pub const SD_CFG1: u16 = 0xFDA0;
    pub const SD_CFG2: u16 = 0xFDA1;
    pub const SD_CFG3: u16 = 0xFDA2;
    pub const SD_STAT1: u16 = 0xFDA3;
    pub const SD_STAT2: u16 = 0xFDA4;
    pub const SD_BUS_STAT: u16 = 0xFDA5;
    pub const SD_PAD_CTL: u16 = 0xFDA6;
    pub const SD_SAMPLE_POINT_CTL: u16 = 0xFDA7;
    pub const SD_PUSH_POINT_CTL: u16 = 0xFDA8;
    pub const SD_CMD0: u16 = 0xFDA9;
    pub const SD_BYTE_CNT_L: u16 = 0xFDAF;
    pub const SD_BYTE_CNT_H: u16 = 0xFDB0;
    pub const SD_BLOCK_CNT_L: u16 = 0xFDB1;
    pub const SD_BLOCK_CNT_H: u16 = 0xFDB2;
    pub const SD_TRANSFER: u16 = 0xFDB3;
    /// Bit 7: SD data lines idle.
    pub const SD_DATA_STATE: u16 = 0xFDB6;

    // PHY access and DMA.
    pub const HS_VCONTROL: u16 = 0xFE26;
    pub const HS_VSTAIN: u16 = 0xFE27;
    pub const HS_VLOADM: u16 = 0xFE28;
    pub const MC_FIFO_CTL: u16 = 0xFF02;
    pub const MC_DMA_CTL: u16 = 0xFF10;
    pub const MC_DMA_TC0: u16 = 0xFF11;
    pub const MC_DMA_RST: u16 = 0xFF15;
    /// Ping-pong buffer 2: where 136-bit (R2) responses land.
    pub const PPBUF_2: u16 = 0xFA00;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_write_matches_observed_packet() {
        // FPDCTL power-on: verified on hardware (accepted, 12 bytes).
        let mut b = Batch::new();
        b.write(reg::FPDCTL, 0x01, 0x00).unwrap();
        assert_eq!(
            b.encode(0),
            &[0x52, 0x54, 0x43, 0x52, 0x00, 0x00, 0x01, 0x00, 0x7C, 0x00, 0x01, 0x00]
        );
    }

    #[test]
    fn read_sets_response_stage_and_length() {
        // HW_VERSION read: verified on hardware (4-byte response, value 03).
        let mut b = Batch::new();
        b.read(reg::HW_VERSION).unwrap();
        assert_eq!(b.response_len(), 4);
        assert_eq!(
            b.encode(0),
            &[0x52, 0x54, 0x43, 0x52, 0x00, 0x00, 0x01, 0x01, 0x3C, 0x01, 0x00, 0x00]
        );
    }

    #[test]
    fn mixed_batch_counts_and_padding() {
        let mut b = Batch::new();
        b.write(reg::CARD_INT_PEND, 0x1C, 0x1C)
            .unwrap()
            .read(reg::CARD_EXIST)
            .unwrap()
            .read(reg::OCPSTAT)
            .unwrap()
            .check(reg::CARD_EXIST, 0x01, 0x01)
            .unwrap();
        assert_eq!(b.ops(), 4);
        assert_eq!(b.reads(), 3); // two reads + one check
        assert_eq!(b.response_len(), 4);
        let p = b.encode(0);
        assert_eq!(p.len(), 8 + 16);
        assert_eq!(&p[5..8], &[0x00, 0x04, STAGE_RESPONSE]);
        assert_eq!(&p[8..12], &[0x7D, 0x71, 0x1C, 0x1C]); // write
        assert_eq!(&p[12..14], &[0x3D, 0x6F]); // read
        assert_eq!(p[20] >> 6, 2); // check
    }

    #[test]
    fn rejects_bad_address_and_overflow() {
        let mut b = Batch::new();
        assert_eq!(b.write(0x1234, 0xFF, 0).err(), Some(BatchError::BadAddress));
        for _ in 0..MAX_OPS {
            b.write(reg::FPDCTL, 1, 0).unwrap();
        }
        assert_eq!(b.write(reg::FPDCTL, 1, 0).err(), Some(BatchError::Full));
        assert_eq!(b.encode(0).len(), MAX_PACKET);
    }

    #[test]
    fn ep0_setups_match_observed() {
        // All three verified on hardware.
        assert_eq!(
            ep0_write_register(reg::SFSM_ED, 0xF8, 0xF8),
            [0x40, 0x00, 0xFC, 0x04, 0xF8, 0xF8, 0, 0]
        );
        assert_eq!(
            ep0_read_register(reg::HW_VERSION),
            [0xC0, 0x00, 0xBC, 0x01, 0, 0, 1, 0]
        );
        assert_eq!(ep0_poll_status(), [0xC0, 0x02, 0, 0, 0, 0, 2, 0]);
    }

    #[test]
    fn status_parse() {
        // Observed with a card inserted: 01 00.
        let s = parse_status(&[0x01, 0x00]).unwrap();
        assert!(s.sd && !s.ms && !s.xd && !s.write_protect);
        assert!(parse_status(&[0x09, 0x00]).unwrap().write_protect);
        assert_eq!(parse_status(&[0x01]), None);
    }
}
