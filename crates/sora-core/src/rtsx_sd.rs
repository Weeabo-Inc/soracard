//! SD card access through a Realtek `RTS51xx` controller: the register batches
//! for SD commands and block transfers, response parsing, and clock maths.
//!
//! [`crate::sd`] knows the SD protocol; [`crate::rtsx`] knows the packet
//! format. This module maps one onto the other, so the driver only has to
//! send the batches, move data, and wait where told.

use crate::rtsx::{reg, Batch, BatchError};
use crate::sd::{Command, Resp};

/// `SD_TRANSFER` bits.
pub const TRANSFER_START: u8 = 0x80;
pub const TRANSFER_END: u8 = 0x40;
pub const TRANSFER_IDLE: u8 = 0x20;
pub const TRANSFER_ERR: u8 = 0x10;
/// Transfer modes (`SD_TRANSFER[3:0]`).
pub const TM_CMD_RSP: u8 = 0x08;
pub const TM_AUTO_READ_3: u8 = 0x05;
pub const TM_AUTO_WRITE_3: u8 = 0x01;
/// Command, response, then one data block into the ping-pong buffer.
pub const TM_NORMAL_READ: u8 = 0x0C;
/// Like `TM_NORMAL_READ` for the CMD19 tuning block (CRC-checked, discarded).
pub const TM_AUTO_TUNING: u8 = 0x0F;

/// `SD_STAT1` CRC7 error.
pub const STAT1_CRC7_ERR: u8 = 0x80;

/// `SD_CFG1` bus-width field.
pub const CFG1_WIDTH_MASK: u8 = 0x03;
pub const CFG1_WIDTH_1: u8 = 0x00;
pub const CFG1_WIDTH_4: u8 = 0x01;

/// `SD_CFG2` value for each response type: response length, CRC7 checking,
/// busy wait.
#[must_use]
pub const fn cfg2_for(resp: Resp) -> u8 {
    match resp {
        Resp::None => 0x04,
        Resp::R1 | Resp::R6 | Resp::R7 => 0x01,
        Resp::R1b => 0x09,
        Resp::R2 => 0x02,
        Resp::R3 => 0x05,
    }
}

/// Batch that issues `cmd` and reads back its response.
///
/// Response layout (see [`parse_command_response`]): `SD_TRANSFER` (from the
/// completion check), then the response bytes, then `SD_STAT1`.
///
/// # Errors
/// Never in practice (fixed op count well under the packet limit).
pub fn command_batch(cmd: &Command) -> Result<Batch, BatchError> {
    let f = cmd.frame();
    let mut b = Batch::new();
    for (i, byte) in f[..5].iter().enumerate() {
        #[allow(clippy::cast_possible_truncation)]
        b.write(reg::SD_CMD0 + i as u16, 0xFF, *byte)?;
    }
    b.write(reg::SD_CFG2, 0xFF, cfg2_for(cmd.resp))?;
    b.write(reg::CARD_DATA_SOURCE, 0x01, 0x01)?; // ping-pong buffer
    b.write(reg::SD_TRANSFER, 0xFF, TRANSFER_START | TM_CMD_RSP)?;
    b.check(
        reg::SD_TRANSFER,
        TRANSFER_END | TRANSFER_IDLE,
        TRANSFER_END | TRANSFER_IDLE,
    )?;
    match cmd.resp {
        Resp::None => {}
        Resp::R2 => {
            for i in 0..16 {
                b.read(reg::PPBUF_2 + i)?;
            }
        }
        _ => {
            for i in 0..5 {
                b.read(reg::SD_CMD0 + i)?;
            }
        }
    }
    b.read(reg::SD_STAT1)?;
    Ok(b)
}

/// Batch that issues a command whose single data block (up to 512 bytes,
/// e.g. the 64-byte CMD6 status) is read into the controller's ping-pong
/// buffer. Response: `SD_TRANSFER`, then the 4 payload bytes of the R1
/// response. Fetch the data afterwards with [`read_ppbuf_batch`].
///
/// # Errors
/// Never in practice.
pub fn read_data_batch(cmd: &Command, byte_cnt: u16) -> Result<Batch, BatchError> {
    let f = cmd.frame();
    let [cnt_lo, cnt_hi] = byte_cnt.to_le_bytes();
    let mut b = Batch::new();
    for (i, byte) in f[..5].iter().enumerate() {
        #[allow(clippy::cast_possible_truncation)]
        b.write(reg::SD_CMD0 + i as u16, 0xFF, *byte)?;
    }
    b.write(reg::SD_BYTE_CNT_L, 0xFF, cnt_lo)?
        .write(reg::SD_BYTE_CNT_H, 0xFF, cnt_hi)?
        .write(reg::SD_BLOCK_CNT_L, 0xFF, 1)?
        .write(reg::SD_BLOCK_CNT_H, 0xFF, 0)?
        .write(reg::SD_CFG2, 0xFF, cfg2_for(Resp::R1))?
        .write(reg::CARD_DATA_SOURCE, 0x01, 0x01)? // ping-pong buffer
        .write(reg::SD_TRANSFER, 0xFF, TRANSFER_START | TM_NORMAL_READ)?
        .check(reg::SD_TRANSFER, TRANSFER_END, TRANSFER_END)?;
    for i in 1..5 {
        b.read(reg::SD_CMD0 + i)?;
    }
    Ok(b)
}

/// Batch that reads `len` bytes of the ping-pong buffer (data of a
/// [`read_data_batch`]).
///
/// # Errors
/// `len` exceeds what one packet can carry.
pub fn read_ppbuf_batch(len: u16) -> Result<Batch, BatchError> {
    let mut b = Batch::new();
    for i in 0..len {
        b.read(reg::PPBUF_2 + i)?;
    }
    Ok(b)
}

/// Bus timing modes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Timing {
    /// Default speed (and SDR12/SDR25 at 1.8 V): SD 2.0 mode, data sampled
    /// on the rising edge.
    Default,
    /// SD High Speed: SD 2.0 mode, output 1/4 cycle ahead, input sampled 1/4
    /// cycle late.
    HighSpeed,
    /// UHS-I SDR50: SD 3.0 mode with variable-phase sampling (tuned).
    Sdr50,
}

/// `SD_VPCLK0_CTL` bit: phase generator out of reset.
pub const PHASE_NOT_RESET: u8 = 0x40;
/// `SD_CFG1` bit: asynchronous FIFO reset.
pub const ASYNC_FIFO_RST: u8 = 0x10;
/// Highest sample phase index (16 phases).
pub const MAX_PHASE: u8 = 15;

/// Bus timing for `t`. Apply after the clock switch (the clock batch resets
/// the SD 2.0 push/sample points).
///
/// # Errors
/// Never in practice.
pub fn timing_batch(t: Timing) -> Result<Batch, BatchError> {
    let mut b = Batch::new();
    match t {
        Timing::Default | Timing::HighSpeed => {
            b.write(reg::SD_CFG1, 0x0C, 0x00)? // SD 2.0 mode
                .write(reg::CARD_CLK_SOURCE, 0xFF, 0x24)?; // CRC fixed, SD30 var, sample var
            if t == Timing::HighSpeed {
                b.write(reg::SD_PUSH_POINT_CTL, 0x10, 0x10)?.write(
                    reg::SD_SAMPLE_POINT_CTL,
                    0x08,
                    0x08,
                )?;
            } else {
                b.write(reg::SD_PUSH_POINT_CTL, 0xFF, 0x00)?.write(
                    reg::SD_SAMPLE_POINT_CTL,
                    0x08,
                    0x00,
                )?;
            }
        }
        Timing::Sdr50 => {
            // SD 3.0 mode + FIFO reset; CRC var, SD30 fixed, sample var.
            b.write(reg::SD_CFG1, 0x0C | ASYNC_FIFO_RST, 0x08 | ASYNC_FIFO_RST)?
                .write(reg::CARD_CLK_SOURCE, 0xFF, 0x21)?;
        }
    }
    Ok(b)
}

/// Signal voltage of the SD pads. For 1.8 V the SD clock is also stopped
/// (UHS-I voltage switch: the clock stays gated while the levels change).
///
/// # Errors
/// Never in practice.
pub fn signal_voltage_batch(v1_8: bool) -> Result<Batch, BatchError> {
    let mut b = Batch::new();
    if v1_8 {
        b.write(reg::SD_BUS_STAT, 0xC0, 0x40)? // clock force-stop
            .write(reg::SD_PAD_CTL, 0x80, 0x80)? // I/O at 1.8 V
            .write(reg::LDO_POWER_CFG, 0x1C, 0x04)?; // SD18 LDO 1.8 V
    } else {
        b.write(reg::SD_PAD_CTL, 0x80, 0x00)? // I/O at 3.3 V
            .write(reg::LDO_POWER_CFG, 0x1C, 0x1C)?; // SD18 LDO 3.3 V
    }
    Ok(b)
}

/// Let the SD clock toggle freely (`on`) or return it to command-driven
/// operation; also releases a force-stop.
///
/// # Errors
/// Never in practice.
pub fn clock_toggle_batch(on: bool) -> Result<Batch, BatchError> {
    let mut b = Batch::new();
    b.write(reg::SD_BUS_STAT, 0xC0, if on { 0x80 } else { 0x00 })?;
    Ok(b)
}

/// `SD_BUS_STAT` DAT3..DAT0 line levels.
pub const BUS_DAT_MASK: u8 = 0x1E;

/// CMD19 tuning read: like [`read_data_batch`] (64-byte block) in the
/// controller's auto-tuning mode, which checks the data CRC without keeping
/// the block. Response: `SD_TRANSFER` + 4 R1 bytes.
///
/// # Errors
/// Never in practice.
pub fn tuning_batch() -> Result<Batch, BatchError> {
    let c = crate::sd::send_tuning_block();
    let f = c.frame();
    let [lo, hi] = crate::sd::TUNING_BLOCK_LEN.to_le_bytes();
    let mut b = Batch::new();
    for (i, byte) in f[..5].iter().enumerate() {
        #[allow(clippy::cast_possible_truncation)]
        b.write(reg::SD_CMD0 + i as u16, 0xFF, *byte)?;
    }
    b.write(reg::SD_BYTE_CNT_L, 0xFF, lo)?
        .write(reg::SD_BYTE_CNT_H, 0xFF, hi)?
        .write(reg::SD_BLOCK_CNT_L, 0xFF, 1)?
        .write(reg::SD_BLOCK_CNT_H, 0xFF, 0)?
        .write(reg::SD_CFG2, 0xFF, cfg2_for(Resp::R1))?
        .write(reg::SD_TRANSFER, 0xFF, TRANSFER_START | TM_AUTO_TUNING)?
        .check(reg::SD_TRANSFER, TRANSFER_END, TRANSFER_END)?;
    for i in 1..5 {
        b.read(reg::SD_CMD0 + i)?;
    }
    Ok(b)
}

/// Set the TX (`tx`) or RX sample phase (0..=15) of the variable-phase clocks.
///
/// # Errors
/// Never in practice.
pub fn phase_batch(point: u8, tx: bool) -> Result<Batch, BatchError> {
    let mut b = Batch::new();
    b.write(reg::CLK_DIV, 0x80, 0x80)? // clock change in progress
        .write(
            if tx {
                reg::SD_VPCLK0_CTL
            } else {
                reg::SD_VPCLK1_CTL
            },
            0x0F,
            point & MAX_PHASE,
        )?
        .write(reg::SD_VPCLK0_CTL, PHASE_NOT_RESET, 0)?
        .write(reg::SD_VPCLK0_CTL, PHASE_NOT_RESET, PHASE_NOT_RESET)?
        .write(reg::CLK_DIV, 0x80, 0x00)?
        .write(reg::SD_CFG1, ASYNC_FIFO_RST, 0)?;
    Ok(b)
}

/// Pick the RX phase from a map of passing phases (bit i = phase i passed):
/// the middle of the longest run of passing phases, treating the 16 phases
/// as a ring. `None` if nothing passed.
#[must_use]
pub fn final_phase(map: u16) -> Option<u8> {
    if map == 0 {
        return None;
    }
    let n = u32::from(MAX_PHASE) + 1;
    let passes = |i: u32| map >> (i % n) & 1 != 0;
    let run = |start: u32| (0..n).take_while(|k| passes(start + k)).count();
    let (mut start, mut best_start, mut best_len) = (0u32, 0u32, 0usize);
    while start < n {
        let len = run(start);
        if len > best_len {
            best_start = start;
            best_len = len;
        }
        #[allow(clippy::cast_possible_truncation)]
        let step = len.max(1) as u32;
        start += step;
    }
    #[allow(clippy::cast_possible_truncation)]
    let phase = ((best_start + best_len as u32 / 2) % n) as u8;
    Some(phase)
}

/// Why a command failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CmdError {
    /// The controller flagged the transfer (no response / timeout).
    Transfer(u8),
    /// CRC7 error on the response.
    Crc,
    /// The response was shorter than expected.
    Short,
    /// The response's start/transmission bits were not 00.
    Framing(u8),
}

/// A response token in the layout [`crate::sd`] expects: 6 bytes for short
/// responses (`[index, arg×4, crc]`), 17 for R2 (`[0x3F, CID/CSD×16]`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Token {
    pub bytes: [u8; 17],
    pub len: usize,
}

impl Token {
    #[must_use]
    pub fn as_slice(&self) -> &[u8] {
        &self.bytes[..self.len]
    }
}

/// Parse the response of a [`command_batch`].
///
/// The controller drops the trailing CRC byte; it is replaced by `0x01` (CRC
/// already checked by hardware, end bit set).
///
/// # Errors
/// See [`CmdError`].
pub fn parse_command_response(kind: Resp, rsp: &[u8]) -> Result<Token, CmdError> {
    let transfer = *rsp.first().ok_or(CmdError::Short)?;
    if transfer & TRANSFER_ERR != 0 {
        return Err(CmdError::Transfer(transfer));
    }
    let mut t = Token {
        bytes: [0; 17],
        len: 0,
    };
    let stat1 = match kind {
        Resp::None => *rsp.get(1).ok_or(CmdError::Short)?,
        Resp::R2 => {
            let body = rsp.get(1..17).ok_or(CmdError::Short)?;
            // body[0] is the start byte; body[1..16] are bits 127..8.
            t.bytes[..16].copy_from_slice(body);
            t.bytes[16] = 0x01;
            t.len = 17;
            *rsp.get(17).ok_or(CmdError::Short)?
        }
        _ => {
            let body = rsp.get(1..6).ok_or(CmdError::Short)?;
            if body[0] & 0xC0 != 0 {
                return Err(CmdError::Framing(body[0]));
            }
            t.bytes[..5].copy_from_slice(body);
            t.bytes[5] = 0x01;
            t.len = 6;
            *rsp.get(6).ok_or(CmdError::Short)?
        }
    };
    if kind.has_crc() && stat1 & STAT1_CRC7_ERR != 0 {
        return Err(CmdError::Crc);
    }
    Ok(t)
}

fn transfer_batch(blocks: u16, write: bool) -> Result<Batch, BatchError> {
    let total = u32::from(blocks) * 512;
    let tc = total.to_le_bytes();
    let [bl, bh] = blocks.to_le_bytes();
    let mut b = Batch::new();
    b.write(reg::SD_BYTE_CNT_L, 0xFF, 0x00)?
        .write(reg::SD_BYTE_CNT_H, 0xFF, 0x02)?
        .write(reg::SD_BLOCK_CNT_L, 0xFF, bl)?
        .write(reg::SD_BLOCK_CNT_H, 0xFF, bh)?
        .write(reg::CARD_DATA_SOURCE, 0x01, 0x00)?; // ring buffer
    for (i, byte) in tc.iter().enumerate() {
        // MC_DMA_TC0 (LSB) .. TC3 (MSB).
        #[allow(clippy::cast_possible_truncation)]
        b.write(reg::MC_DMA_TC0 + i as u16, 0xFF, *byte)?;
    }
    if write {
        // DMA enable, to card, 512-byte packets.
        b.write(reg::MC_DMA_CTL, 0x0F, 0x09)?
            .write(reg::SD_CFG2, 0xFF, 0x84)?
            .write(reg::SD_TRANSFER, 0xFF, TRANSFER_START | TM_AUTO_WRITE_3)?;
    } else {
        // DMA enable, from card, 512-byte packets.
        b.write(reg::MC_DMA_CTL, 0x0F, 0x0B)?
            .write(reg::SD_CFG2, 0xFF, 0x00)?
            .write(reg::SD_TRANSFER, 0xFF, TRANSFER_START | TM_AUTO_READ_3)?;
    }
    b.check(reg::SD_TRANSFER, TRANSFER_END, TRANSFER_END)?;
    Ok(b)
}

/// Data-phase batch for reading `blocks` × 512 bytes after CMD17/CMD18 was
/// issued with [`command_batch`]. Send with stage `STAGE_DATA_IN`, then bulk
/// IN `blocks*512` bytes, then bulk IN the 4-byte status.
///
/// # Errors
/// Never in practice.
pub fn read_batch(blocks: u16) -> Result<Batch, BatchError> {
    transfer_batch(blocks, false)
}

/// Data-phase batch for writing `blocks` × 512 bytes after CMD24/CMD25. Send
/// with stage `STAGE_DATA_OUT`, bulk OUT the data, then bulk IN the status.
///
/// # Errors
/// Never in practice.
pub fn write_batch(blocks: u16) -> Result<Batch, BatchError> {
    transfer_batch(blocks, true)
}

/// Whether the 4-byte status after a data phase reports success.
#[must_use]
pub fn data_status_ok(status: &[u8]) -> bool {
    status
        .first()
        .is_some_and(|&t| t & TRANSFER_ERR == 0 && t & TRANSFER_END != 0)
}

/// Clock-generator settings for an SD clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ssc {
    /// `SSC_DIV_N_0`.
    pub n: u8,
    /// `CLK_DIV[5:0]`: post-divider in [5:4], MCU count in [3:0].
    pub clk_div: u8,
    /// `SSC_CTL2[1:0]` spread depth.
    pub depth: u8,
    /// `SD_CFG1[7:6]`: 0x80 (/128) for the identification clock, else 0.
    pub sd_div: u8,
    /// Reset the variable-phase clocks (SD 3.0 / UHS timings).
    pub vpclk: bool,
}

/// Settings for the identification clock (`init`) or a data clock of
/// `mhz` MHz. The controller runs its internal clock at twice the card clock
/// and keeps the PLL N above 60 by adding post-divider steps.
#[must_use]
pub fn ssc_for(init: bool, mhz: u32) -> Ssc {
    let f = if init { 30 } else { (mhz * 2).max(2) };
    let mut n = f.saturating_sub(2);
    let mut div = 0u32;
    while n < 60 && div < 3 {
        n = (n + 2) * 2 - 2;
        div += 1;
    }
    let mcu = (60 / f + 3).min(15);
    // Spread depth: 512K (3), one step less for the doubled clock, and less
    // again for each post-divider step beyond the first.
    let mut depth: u32 = 3 - 1;
    if div > 0 {
        depth = depth.saturating_sub(div - 1).max(1);
    }
    #[allow(clippy::cast_possible_truncation)]
    Ssc {
        n: n.min(255) as u8,
        clk_div: ((div << 4) | mcu) as u8,
        depth: depth as u8,
        sd_div: if init { 0x80 } else { 0x00 },
        vpclk: false,
    }
}

/// Clock-generator settings for a UHS-I SDR clock (SDR50: 100 MHz): the SSC
/// runs at the card clock itself (no doubling), 2M spread, variable-phase
/// clocks reset.
#[must_use]
pub fn ssc_for_uhs(mhz: u32) -> Ssc {
    let f = mhz.max(3);
    let mut n = f - 2;
    let mut div = 0u32;
    while n < 60 && div < 2 {
        n = (n + 2) * 2 - 2;
        div += 1;
    }
    let mcu = (60 / f + 3).min(15);
    // 2M depth (1), reduced for post-divider steps beyond the first.
    let depth = if div > 1 {
        1u32.saturating_sub(div - 1).max(1)
    } else {
        1
    };
    #[allow(clippy::cast_possible_truncation)]
    Ssc {
        n: n.min(255) as u8,
        clk_div: ((div << 4) | mcu) as u8,
        depth: depth as u8,
        sd_div: 0,
        vpclk: true,
    }
}

/// First batch of a clock switch (default-speed timing + PLL programming).
/// Then: `SSC_CTL1 = 0xD0`, wait ≥100 µs, `CLK_DIV[7] = 0`
/// ([`clock_finish_batch`]).
///
/// # Errors
/// Never in practice.
pub fn clock_batch(s: &Ssc) -> Result<Batch, BatchError> {
    let mut b = Batch::new();
    b.write(reg::SD_CFG1, 0xC0, s.sd_div)?
        .write(reg::SD_CFG1, 0x0C, 0x00)? // SD 2.0 mode
        .write(reg::CARD_CLK_SOURCE, 0xFF, 0x24)?
        .write(reg::SD_PUSH_POINT_CTL, 0x10, 0x00)?
        .write(reg::SD_SAMPLE_POINT_CTL, 0x08, 0x00)?
        .write(reg::CLK_DIV, 0x80, 0x80)? // clock change in progress
        .write(reg::CLK_DIV, 0x3F, s.clk_div)?
        .write(reg::SSC_CTL1, 0x80, 0x00)? // hold SSC in reset
        .write(reg::SSC_CTL2, 0x03, s.depth)?
        .write(reg::SSC_DIV_N_0, 0xFF, s.n)?
        .write(reg::SSC_CTL1, 0x80, 0x80)?;
    if s.vpclk {
        b.write(reg::SD_VPCLK0_CTL, PHASE_NOT_RESET, 0)?.write(
            reg::SD_VPCLK0_CTL,
            PHASE_NOT_RESET,
            PHASE_NOT_RESET,
        )?;
    }
    b.write(reg::SSC_CTL1, 0xFF, 0xD0)?;
    Ok(b)
}

/// Second batch of a clock switch (after the settle delay).
///
/// # Errors
/// Never in practice.
pub fn clock_finish_batch() -> Result<Batch, BatchError> {
    let mut b = Batch::new();
    b.write(reg::CLK_DIV, 0x80, 0x00)?;
    Ok(b)
}

/// Pull-up/down values for the SD pins, QFN24 package (`CARD_PULL_CTL1..6`).
pub const PULL_SD_ON_QFN24: [u8; 6] = [0xA5, 0x9A, 0xA5, 0x9A, 0x65, 0x5A];
pub const PULL_SD_OFF_QFN24: [u8; 6] = [0x65, 0x55, 0x95, 0x55, 0x56, 0x59];
/// The same for the LQFP48 package (RTS5139-class boards).
pub const PULL_SD_ON_LQFP48: [u8; 6] = [0xAA, 0xAA, 0xA9, 0x55, 0x55, 0xA5];
pub const PULL_SD_OFF_LQFP48: [u8; 6] = [0x55, 0x55, 0x95, 0x55, 0x55, 0xA5];

/// SD pin pulls for the package (`lqfp48`) with the slot on or off.
#[must_use]
pub const fn sd_pulls(lqfp48: bool, on: bool) -> &'static [u8; 6] {
    match (lqfp48, on) {
        (false, true) => &PULL_SD_ON_QFN24,
        (false, false) => &PULL_SD_OFF_QFN24,
        (true, true) => &PULL_SD_ON_LQFP48,
        (true, false) => &PULL_SD_OFF_LQFP48,
    }
}

/// Batch writing all six pull-control registers.
///
/// # Errors
/// Never in practice.
pub fn pull_batch(values: &[u8; 6]) -> Result<Batch, BatchError> {
    let mut b = Batch::new();
    for (i, v) in values.iter().enumerate() {
        #[allow(clippy::cast_possible_truncation)]
        b.write(reg::CARD_PULL_CTL1 + i as u16, 0xFF, *v)?;
    }
    Ok(b)
}

/// Batch writing an 8-bit value to an internal PHY register.
///
/// # Errors
/// Never in practice.
pub fn phy_write_batch(addr: u8, value: u8) -> Result<Batch, BatchError> {
    let mut b = Batch::new();
    b.write(reg::HS_VSTAIN, 0xFF, value)?;
    for nibble in [addr & 0x0F, addr >> 4] {
        b.write(reg::HS_VCONTROL, 0xFF, nibble)?
            .write(reg::HS_VLOADM, 0xFF, 0x00)?
            .write(reg::HS_VLOADM, 0xFF, 0x00)?
            .write(reg::HS_VLOADM, 0xFF, 0x01)?;
    }
    Ok(b)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rtsx::{STAGE_DATA_IN, STAGE_DATA_OUT, STAGE_RESPONSE};
    use crate::sd;

    #[test]
    fn clock_values_match_reference_arithmetic() {
        assert_eq!(
            ssc_for(true, 0),
            Ssc {
                n: 118,
                clk_div: 0x25,
                depth: 1,
                sd_div: 0x80,
                vpclk: false
            }
        );
        assert_eq!(
            ssc_for(false, 25),
            Ssc {
                n: 98,
                clk_div: 0x14,
                depth: 2,
                sd_div: 0,
                vpclk: false
            }
        );
        assert_eq!(
            ssc_for(false, 50),
            Ssc {
                n: 98,
                clk_div: 0x03,
                depth: 2,
                sd_div: 0,
                vpclk: false
            }
        );
    }

    #[test]
    fn cmd8_batch_layout() {
        let mut b = command_batch(&sd::send_if_cond()).unwrap();
        // 5 cmd regs + cfg2 + data source + transfer + check + 5 reads + stat1.
        assert_eq!(b.ops(), 15);
        assert_eq!(b.reads(), 7); // check + 5 + stat1
        assert_eq!(b.response_len(), 8);
        let p = b.encode(0);
        assert_eq!(p[7], STAGE_RESPONSE);
        // First op: SD_CMD0 (0xFDA9) <- 0x48.
        assert_eq!(&p[8..12], &[0x7D, 0xA9, 0xFF, 0x48]);
        // SD_CFG2 <- R7 setting.
        assert_eq!(&p[8 + 5 * 4..8 + 6 * 4], &[0x7D, 0xA1, 0xFF, 0x01]);
        // Transfer start, cmd+rsp mode.
        assert_eq!(&p[8 + 7 * 4..8 + 8 * 4], &[0x7D, 0xB3, 0xFF, 0x88]);
        // Check op on SD_TRANSFER (type 2).
        assert_eq!(p[8 + 8 * 4] >> 6, 2);
    }

    #[test]
    fn r2_and_none_batches() {
        assert_eq!(command_batch(&sd::send_csd(1)).unwrap().reads(), 1 + 16 + 1);
        assert_eq!(command_batch(&sd::go_idle()).unwrap().reads(), 2);
    }

    #[test]
    fn parse_short_response() {
        let rsp = [0x60, 0x08, 0x00, 0x00, 0x01, 0xAA, 0x00, 0x00];
        let t = parse_command_response(Resp::R7, &rsp).unwrap();
        assert_eq!(t.as_slice(), &[0x08, 0, 0, 1, 0xAA, 0x01]);
        assert_eq!(sd::payload48(t.as_slice()), Some(0x1AA));
    }

    #[test]
    fn parse_errors() {
        assert_eq!(
            parse_command_response(Resp::R7, &[0x70, 0, 0, 0, 0, 0, 0, 0]),
            Err(CmdError::Transfer(0x70))
        );
        assert_eq!(
            parse_command_response(Resp::R1, &[0x60, 8, 0, 0, 0, 0, 0x80, 0]),
            Err(CmdError::Crc)
        );
        // R3 has no CRC: a CRC7 flag is ignored.
        assert!(parse_command_response(
            Resp::R3,
            &[0x60, 0x3F & 0x3F, 0x80, 0xFF, 0x80, 0, 0x80, 0]
        )
        .is_ok());
        assert_eq!(
            parse_command_response(Resp::R1, &[0x60, 8]),
            Err(CmdError::Short)
        );
        assert_eq!(
            parse_command_response(Resp::R1, &[0x60, 0xFF, 0, 0, 0, 0, 0, 0]),
            Err(CmdError::Framing(0xFF))
        );
    }

    #[test]
    fn parse_r2_feeds_csd_parser() {
        let mut rsp = [0u8; 20];
        rsp[0] = 0x60;
        rsp[1] = 0x3F; // start byte
        rsp[2] = 0x40; // CSD_STRUCTURE = 1
        let t = parse_command_response(Resp::R2, &rsp).unwrap();
        assert_eq!(t.len, 17);
        assert_eq!(t.bytes[16], 0x01);
        assert_eq!(sd::Csd::parse(&t.bytes[1..17]).unwrap().version, 1);
    }

    #[test]
    fn transfer_batches() {
        let mut r = read_batch(128).unwrap();
        assert_eq!(r.response_len(), 4); // the completion check
        let p = r.encode(STAGE_DATA_IN);
        assert_eq!(p[7], STAGE_DATA_IN | STAGE_RESPONSE);
        // Block count 128 → SD_BLOCK_CNT_L = 0x80.
        assert_eq!(&p[8 + 2 * 4..8 + 3 * 4], &[0x7D, 0xB1, 0xFF, 0x80]);
        // DMA byte count 65536 = 0x00010000 → TC2 = 0x01.
        assert_eq!(&p[8 + 7 * 4..8 + 8 * 4], &[0x7F, 0x13, 0xFF, 0x01]);
        let mut w = write_batch(1).unwrap();
        assert_eq!(w.encode(STAGE_DATA_OUT)[7], STAGE_DATA_OUT | STAGE_RESPONSE);
    }

    #[test]
    fn data_status() {
        assert!(data_status_ok(&[0x40 | 0x20, 0, 0, 0]));
        assert!(!data_status_ok(&[0x50, 0, 0, 0]));
        assert!(!data_status_ok(&[0x00, 0, 0, 0]));
        assert!(!data_status_ok(&[]));
    }

    #[test]
    fn phy_write() {
        let mut b = phy_write_batch(0xC2, 0x7C).unwrap();
        assert_eq!(b.ops(), 9);
        let p = b.encode(0);
        assert_eq!(&p[8..12], &[0x7E, 0x27, 0xFF, 0x7C]); // HS_VSTAIN
        assert_eq!(&p[12..16], &[0x7E, 0x26, 0xFF, 0x02]); // low nibble
        assert_eq!(&p[28..32], &[0x7E, 0x26, 0xFF, 0x0C]); // high nibble
    }
    #[test]
    fn read_data_batch_shape() {
        let mut b = read_data_batch(&crate::sd::switch_high_speed(false), 64).unwrap();
        // 5 command bytes + 6 setup writes + start + check + 4 response reads.
        assert_eq!(b.ops(), 17);
        // check + 4 reads = 5 response bytes, rounded up to 8.
        assert_eq!(b.reads(), 5);
        assert_eq!(b.response_len(), 8);
        let pkt = b.encode(0);
        assert_eq!(&pkt[..4], b"RTCR");
    }

    #[test]
    fn ppbuf_and_timing_batches() {
        assert_eq!(read_ppbuf_batch(64).unwrap().reads(), 64);
        assert_eq!(timing_batch(Timing::HighSpeed).unwrap().ops(), 4);
        assert_eq!(timing_batch(Timing::Default).unwrap().ops(), 4);
        assert_eq!(timing_batch(Timing::Sdr50).unwrap().ops(), 2);
    }
    #[test]
    fn uhs_clock() {
        assert_eq!(
            ssc_for_uhs(100),
            Ssc {
                n: 98,
                clk_div: 0x03,
                depth: 1,
                sd_div: 0,
                vpclk: true
            }
        );
        // The vpclk reset adds two writes to the clock batch.
        let plain = clock_batch(&ssc_for(false, 50)).unwrap().ops();
        assert_eq!(clock_batch(&ssc_for_uhs(100)).unwrap().ops(), plain + 2);
    }

    #[test]
    fn final_phase_picks_middle_of_longest_window() {
        assert_eq!(final_phase(0), None);
        assert_eq!(final_phase(0xFFFF), Some(8));
        // Phases 4..=9 pass: middle is 7.
        assert_eq!(final_phase(0b0000_0011_1111_0000), Some(7));
        // Two windows: 1..=2 and 10..=14; the longer wins (middle 12).
        assert_eq!(final_phase(0b0111_1100_0000_0110), Some(12));
        // A window wrapping around 15 -> 0: phases 14, 15, 0, 1, 2.
        assert_eq!(final_phase(0b1100_0000_0000_0111), Some(0));
        assert_eq!(final_phase(1 << 5), Some(5));
    }

    #[test]
    fn uhs_batches() {
        assert_eq!(signal_voltage_batch(true).unwrap().ops(), 3);
        assert_eq!(signal_voltage_batch(false).unwrap().ops(), 2);
        let t = tuning_batch().unwrap();
        assert_eq!(t.ops(), 5 + 5 + 2 + 4);
        assert_eq!(t.reads(), 5);
        assert_eq!(phase_batch(3, false).unwrap().ops(), 6);
    }
}
