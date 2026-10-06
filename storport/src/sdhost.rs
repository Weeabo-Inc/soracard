// Copyright (c) SoraCard
// SPDX-License-Identifier: MIT OR Apache-2.0

//! SD card host on the Realtek controller: slot power, bus clock, commands,
//! card initialisation and block transfers.
//!
//! The protocol decisions live in `sora_core::sd` (what to send, how to read
//! responses) and `sora_core::rtsx_sd` (how that maps onto controller
//! registers). This module sequences them on the hardware, with the delays the
//! controller needs, and records what happened for diagnostics.
//!
//! Worker context only (PASSIVE_LEVEL, shared worker URB and scratch).

use sora_core::rtsx::{reg, Batch, STAGE_DATA_IN, STAGE_DATA_OUT};
use sora_core::rtsx_sd::{self, CmdError, Token};
use sora_core::sd::{self, CardInfo, Command, InitError, Outcome, Resp, Step};

use crate::chip::{self, ChipError};
use crate::diag::{self, w};
use crate::{usb, AdapterExtension};

const CMD_TIMEOUT_MS: u32 = 2_000;
/// R1b commands may hold the busy line for a while (e.g. CMD7, CMD12).
const BUSY_TIMEOUT_MS: u32 = 3_000;
const DATA_TIMEOUT_MS: u32 = 10_000;
/// Data clock once the card is initialised (default speed).
const DATA_CLOCK_MHZ: u32 = 25;
/// Data clock in SD High Speed mode.
const HS_CLOCK_MHZ: u32 = 50;

/// Why a host operation failed.
#[derive(Clone, Copy)]
pub enum HostError {
    Chip(ChipError),
    Cmd(u8, CmdError),
    Init(InitError),
    /// Card status reported an error bit, or the data phase status was bad.
    Status(u32),
}

/// Send one batch and collect its response (`rsp.len()` bytes).
unsafe fn exchange(
    ext: *mut AdapterExtension,
    b: &mut Batch,
    rsp: &mut [u8],
    timeout_ms: u32,
) -> Result<usize, ChipError> {
    unsafe { chip::send_packet(ext, b, 0)? };
    if rsp.is_empty() {
        return Ok(0);
    }
    unsafe { chip::read_response(ext, rsp, timeout_ms) }
}

/// Send a batch with no response.
unsafe fn apply(
    ext: *mut AdapterExtension,
    b: Result<Batch, sora_core::rtsx::BatchError>,
) -> Result<(), HostError> {
    let Ok(mut b) = b else { return Ok(()) };
    let n = b.reads();
    let mut rsp = [0u8; 8];
    unsafe { exchange(ext, &mut b, &mut rsp[..n.min(8)], CMD_TIMEOUT_MS) }
        .map(|_| ())
        .map_err(HostError::Chip)
}

/// One write as a batch.
unsafe fn write(
    ext: *mut AdapterExtension,
    addr: u16,
    mask: u8,
    value: u8,
) -> Result<(), HostError> {
    let mut b = Batch::new();
    let _ = b.write(addr, mask, value);
    unsafe { apply(ext, Ok(b)) }
}

/// Clear SD transfer errors in the controller (after any failed command or
/// data phase).
///
/// # Safety
/// Worker context.
pub unsafe fn clear_error(ext: *mut AdapterExtension) {
    unsafe {
        let _ = chip::write_reg(ext, reg::CARD_STOP, 0x44, 0x44);
        let _ = chip::write_reg(ext, reg::MC_FIFO_CTL, 0x01, 0x01);
        let _ = chip::write_reg(ext, reg::MC_DMA_RST, 0x01, 0x01);
        chip::clear_fsm(ext);
    }
}

/// Power the SD slot up: select SD, pulls on, staged power ramp.
unsafe fn power_on(ext: *mut AdapterExtension) -> Result<(), HostError> {
    unsafe {
        // Always start at 3.3 V signaling (a previous card may have left the
        // pads at 1.8 V).
        apply(ext, rtsx_sd::signal_voltage_batch(false))?;
        let mut b = Batch::new();
        let _ = b
            .write(reg::CARD_SELECT, 0x07, 0x02)
            .and_then(|b| b.write(reg::CARD_SHARE_MODE, 0x03, 0x01))
            .and_then(|b| b.write(reg::CARD_CLK_EN, 0x04, 0x04));
        apply(ext, Ok(b))?;
        apply(
            ext,
            rtsx_sd::pull_batch(rtsx_sd::sd_pulls((*ext).lqfp48, true)),
        )?;
        // Partial power first to limit inrush, then full power.
        write(ext, reg::CARD_PWR_CTL, 0x03, 0x02)?;
        chip::delay_us(1_000);
        let mut b = Batch::new();
        let _ = b
            .write(reg::CARD_PWR_CTL, 0x03, 0x00)
            .and_then(|b| b.write(reg::FPDCTL, 0x01, 0x01));
        apply(ext, Ok(b))?;
        chip::delay_us(20_000);
        write(ext, reg::FPDCTL, 0x01, 0x00)?;
        chip::delay_us(1_000);
        let mut b = Batch::new();
        let _ = b
            .write(reg::CARD_PWR_CTL, 0x0C, 0x00) // LDO on
            .and_then(|b| b.write(reg::CARD_OE, 0x04, 0x04))
            .and_then(|b| b.write(reg::SD_CFG3, 0x01, 0x01)); // response timeout detection
        apply(ext, Ok(b))
    }
}

/// Power the SD slot down (card removed, or init failed).
///
/// # Safety
/// Worker context.
pub unsafe fn power_off(ext: *mut AdapterExtension) {
    unsafe {
        let mut b = Batch::new();
        let _ = b
            .write(reg::CARD_CLK_EN, 0x04, 0x00)
            .and_then(|b| b.write(reg::CARD_OE, 0x04, 0x00))
            .and_then(|b| b.write(reg::CARD_PWR_CTL, 0x03, 0x03))
            .and_then(|b| b.write(reg::CARD_PWR_CTL, 0x0F, 0x0B));
        let _ = apply(ext, Ok(b));
        let _ = apply(
            ext,
            rtsx_sd::pull_batch(rtsx_sd::sd_pulls((*ext).lqfp48, false)),
        );
    }
}

/// Switch the SD bus clock (identification clock when `init`).
unsafe fn set_clock(ext: *mut AdapterExtension, init: bool, mhz: u32) -> Result<(), HostError> {
    let s = rtsx_sd::ssc_for(init, mhz);
    unsafe {
        apply(ext, rtsx_sd::clock_batch(&s))?;
        chip::delay_us(1_000);
        apply(ext, rtsx_sd::clock_finish_batch())
    }
}

/// Issue one SD command (with CMD55 first for application commands).
///
/// # Safety
/// Worker context, slot powered.
pub unsafe fn command(
    ext: *mut AdapterExtension,
    c: &Command,
    rca: u16,
) -> Result<Token, HostError> {
    if c.app {
        let t = unsafe { command(ext, &sd::app_cmd(rca), rca)? };
        match sd::payload48(t.as_slice()) {
            Some(s) if s & sd::R1_ERRORS == 0 => {}
            Some(s) => return Err(HostError::Status(s)),
            None => return Err(HostError::Cmd(55, CmdError::Short)),
        }
        let plain = Command { app: false, ..*c };
        return unsafe { command(ext, &plain, rca) };
    }
    let Ok(mut b) = rtsx_sd::command_batch(c) else {
        return Err(HostError::Cmd(c.index, CmdError::Short));
    };
    let mut rsp = [0u8; 20];
    let n = b.response_len().min(rsp.len());
    let timeout = if c.resp == Resp::R1b {
        BUSY_TIMEOUT_MS
    } else {
        CMD_TIMEOUT_MS
    };
    let got = match unsafe { exchange(ext, &mut b, &mut rsp[..n], timeout) } {
        Ok(got) => got,
        Err(e) => {
            unsafe { clear_error(ext) };
            return Err(HostError::Chip(e));
        }
    };
    match rtsx_sd::parse_command_response(c.resp, &rsp[..got]) {
        Ok(t) => Ok(t),
        Err(e) => {
            unsafe { clear_error(ext) };
            Err(HostError::Cmd(c.index, e))
        }
    }
}

/// Initialisation log, flushed to `SdLog` after every entry so a stuck init
/// is visible. 8-byte entries `[cmd, result, b0, b1, b2, b3, 0, 0]`:
/// * result 0: response; b0..b3 = response payload
/// * result 1: USB/bulk failure; b0..b3 = USBD status (LE)
/// * result 3: controller error flag; b0 = SD_TRANSFER
/// * result 4: CRC error; 5: short response; 6: framing (b0 = first byte)
/// * result 2: wait (cmd 0xFF)
struct InitLog {
    bytes: [u8; 8 * 64],
    n: usize,
}

impl InitLog {
    fn push(&mut self, cmd: u8, result: u8, info: [u8; 4]) {
        if self.n == 64 {
            return;
        }
        let e = &mut self.bytes[self.n * 8..self.n * 8 + 8];
        e[0] = cmd;
        e[1] = result;
        e[2..6].copy_from_slice(&info);
        self.n += 1;
        // Flushed per entry only with the detailed trace (shows a stuck
        // init live); otherwise once at the end of `init_card`.
        if diag::verbose() {
            diag::set_bin(w!("SdLog"), &self.bytes[..self.n * 8]);
        }
    }

    fn ok(&mut self, cmd: u8, t: &Token) {
        let mut info = [0u8; 4];
        // Payload bytes after the index byte; empty for no-response commands
        // (CMD0), whose token has length 0.
        let payload = t.as_slice().get(1..).unwrap_or(&[]);
        let k = payload.len().min(4);
        info[..k].copy_from_slice(&payload[..k]);
        self.push(cmd, 0, info);
    }

    fn err(&mut self, cmd: u8, e: HostError) {
        let (code, info) = match e {
            HostError::Chip(ChipError(x)) => (1, x.usbd.to_le_bytes()),
            HostError::Cmd(_, CmdError::Transfer(t)) => (3, [t, 0, 0, 0]),
            HostError::Cmd(_, CmdError::Crc) => (4, [0; 4]),
            HostError::Cmd(_, CmdError::Short) => (5, [0; 4]),
            HostError::Cmd(_, CmdError::Framing(b)) => (6, [b, 0, 0, 0]),
            HostError::Status(s) => (7, s.to_le_bytes()),
            HostError::Init(_) => (8, [0; 4]),
        };
        self.push(cmd, code, info);
    }
}

/// Coarse progress marker for diagnostics (`SdStage`).
fn stage(n: u32) {
    if diag::verbose() {
        diag::set_u32(w!("SdStage"), n);
    }
}

/// Result code of the previous `init_card`, so repeated identical failures
/// (an empty slot is probed every second) are not re-recorded.
static LAST_INIT: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

/// Last thing a command produced, fed to the state machine next.
enum Last {
    Waited,
    NoResponse,
    Response(Token),
}

/// Power the slot and bring the card from idle to transfer state with a
/// 4-bit bus at the data clock.
///
/// # Safety
/// Worker context; USB and controller initialised.
pub unsafe fn init_card(ext: *mut AdapterExtension, want_uhs: bool) -> Result<CardInfo, HostError> {
    let mut log = InitLog {
        bytes: [0; 8 * 64],
        n: 0,
    };
    let result = unsafe { init_card_logged(ext, &mut log, want_uhs) };
    stage(9);
    let code = match &result {
        Ok(_) => 1,
        Err(e) => error_code(*e),
    };
    let changed = LAST_INIT.swap(code, core::sync::atomic::Ordering::Relaxed) != code;
    if changed || code == 1 || diag::verbose() {
        diag::set_bin(w!("SdLog"), &log.bytes[..log.n * 8]);
        diag::set_u32(w!("SdInit"), code);
    }
    if let Ok(info) = &result {
        diag::set_bin(w!("SdCid"), &info.cid);
        #[allow(clippy::cast_possible_truncation)]
        diag::set_u32(w!("SdBlocks"), info.blocks.min(u64::from(u32::MAX)) as u32);
        diag::set_u32(
            w!("SdFlags"),
            u32::from(info.high_capacity)
                | u32::from(info.write_protected) << 1
                | u32::from(info.uhs) << 2,
        );
    }
    result
}

/// Milliseconds of interrupt time (monotonic).
fn now_ms() -> u64 {
    let mut qpc = 0u64;
    // SAFETY: valid out-pointer; any IRQL.
    unsafe { wdk_sys::ntddk::KeQueryInterruptTimePrecise(&raw mut qpc) / 10_000 }
}

fn error_code(e: HostError) -> u32 {
    match e {
        HostError::Chip(_) => 0xE100,
        HostError::Cmd(i, _) => 0xE200 | u32::from(i),
        HostError::Init(InitError::NoCard) => 0xE300,
        HostError::Init(InitError::BadInterfaceCondition) => 0xE301,
        HostError::Init(InitError::PowerUpTimeout) => 0xE302,
        HostError::Init(InitError::Protocol(i)) => 0xE400 | u32::from(i),
        HostError::Init(InitError::VoltageSwitch) => 0xE303,
        HostError::Status(_) => 0xE500,
    }
}

/// Power the slot to the identification state (power ramp, slow clock, 74+
/// free-running clocks), ready for CMD0.
unsafe fn power_up_for_identification(ext: *mut AdapterExtension) -> Result<(), HostError> {
    unsafe {
        stage(1);
        power_on(ext)?;
        stage(2);
        set_clock(ext, true, 0)?;
        // At least 74 clocks before the first command: let the clock run free.
        stage(3);
        write(ext, reg::SD_BUS_STAT, 0xC0, 0x80)?;
        chip::delay_us(2_000);
        write(ext, reg::SD_BUS_STAT, 0xC0, 0x00)
    }
}

/// Cheap presence probe (tens of ms, vs ~750 ms for a full init): does any
/// card answer CMD8 (SD v2) or CMD55 (SD v1) after CMD0? Leaves the slot
/// powered off. Used while a card is ejected, to notice it being pulled.
///
/// # Safety
/// Worker context.
pub unsafe fn probe(ext: *mut AdapterExtension) -> bool {
    let present = unsafe {
        power_up_for_identification(ext).is_ok()
            && command(ext, &sd::go_idle(), 0).is_ok()
            && (command(ext, &sd::send_if_cond(), 0).is_ok()
                || command(ext, &sd::app_cmd(0), 0).is_ok())
    };
    unsafe { power_off(ext) };
    present
}

unsafe fn init_card_logged(
    ext: *mut AdapterExtension,
    log: &mut InitLog,
    want_uhs: bool,
) -> Result<CardInfo, HostError> {
    unsafe { power_up_for_identification(ext)? };
    stage(4);
    // Bound the whole sequence so a misbehaving card cannot stall the I/O
    // worker (the storage stack waits on it).
    let deadline = now_ms() + 5_000;

    let mut machine = sd::Init::with_uhs(want_uhs);
    let mut last = Last::Waited;
    for _ in 0..400 {
        let outcome = match &last {
            Last::Waited => Outcome::Waited,
            Last::NoResponse => Outcome::NoResponse,
            Last::Response(t) => Outcome::Response(t.as_slice()),
        };
        match machine.next(outcome) {
            Step::Send(c) => {
                if now_ms() > deadline {
                    return Err(HostError::Init(InitError::PowerUpTimeout));
                }
                if c.index == 11 {
                    // The clock must run freely around CMD11.
                    unsafe { apply(ext, rtsx_sd::clock_toggle_batch(true))? };
                }
                last = match unsafe { command(ext, &c, machine.rca()) } {
                    Ok(t) => {
                        log.ok(c.index, &t);
                        Last::Response(t)
                    }
                    Err(e) => {
                        log.err(c.index, e);
                        Last::NoResponse
                    }
                };
            }
            Step::Wait(ms) => {
                unsafe { chip::delay_us(i64::from(ms) * 1_000) };
                last = Last::Waited;
            }
            Step::SwitchVoltage => {
                let r = unsafe { host_voltage_switch(ext) };
                log.push(0xFE, u8::from(r.is_err()), [0; 4]);
                r?;
                last = Last::Waited;
            }
            Step::Ready(info) => {
                unsafe {
                    set_clock(ext, false, DATA_CLOCK_MHZ)?;
                    write(
                        ext,
                        reg::SD_CFG1,
                        rtsx_sd::CFG1_WIDTH_MASK,
                        rtsx_sd::CFG1_WIDTH_4,
                    )?;
                }
                return Ok(info);
            }
            Step::Fail(e) => return Err(HostError::Init(e)),
        }
    }
    Err(HostError::Init(InitError::PowerUpTimeout))
}

/// Whether the initialised card still answers: CMD13 (SEND_STATUS) at its
/// RCA, one retry. This is the removal detector — the controller's card-detect
/// bit is stuck at "present" on some boards, so silence is the only reliable
/// sign the card is gone. A re-inserted card is back in idle state without an
/// RCA, so it does not answer either and gets re-initialised.
///
/// # Safety
/// Worker context, slot powered.
pub unsafe fn card_alive(ext: *mut AdapterExtension, info: &CardInfo) -> bool {
    for _ in 0..2 {
        if let Ok(t) = unsafe { command(ext, &sd::send_status(info.rca), info.rca) } {
            if sd::payload48(t.as_slice()).is_some() {
                return true;
            }
        }
    }
    false
}

/// Milliseconds since boot (monotonic), for the card-detect schedule.
pub fn uptime_ms() -> u64 {
    now_ms()
}

/// Accumulated sector-transfer timing (µs), for `Perf*` diagnostics:
/// `[transfers, blocks, command, setup, data, status, stop]`.
static PERF: [core::sync::atomic::AtomicU64; 7] =
    [const { core::sync::atomic::AtomicU64::new(0) }; 7];

fn now_us() -> u64 {
    let mut qpc = 0u64;
    // SAFETY: valid out-pointer; any IRQL.
    unsafe { wdk_sys::ntddk::KeQueryInterruptTimePrecise(&raw mut qpc) / 10 }
}

fn perf_add(i: usize, v: u64) {
    PERF[i].fetch_add(v, core::sync::atomic::Ordering::Relaxed);
}

/// Write the accumulated transfer timing to the registry (`PerfXfers`,
/// `PerfBlocks`, `PerfCmdMs`, `PerfSetupMs`, `PerfDataMs`, `PerfStatusMs`,
/// `PerfStopMs`). PASSIVE_LEVEL.
pub fn flush_perf() {
    let v = |i: usize| PERF[i].load(core::sync::atomic::Ordering::Relaxed);
    #[allow(clippy::cast_possible_truncation)]
    {
        diag::set_u32(w!("PerfXfers"), v(0) as u32);
        diag::set_u32(w!("PerfBlocks"), v(1) as u32);
        diag::set_u32(w!("PerfCmdMs"), (v(2) / 1000) as u32);
        diag::set_u32(w!("PerfSetupMs"), (v(3) / 1000) as u32);
        diag::set_u32(w!("PerfDataMs"), (v(4) / 1000) as u32);
        diag::set_u32(w!("PerfStatusMs"), (v(5) / 1000) as u32);
        diag::set_u32(w!("PerfStopMs"), (v(6) / 1000) as u32);
    }
}

/// Read or write `blocks` 512-byte blocks at `lba` to/from `buf`.
///
/// # Safety
/// Worker context; card initialised; `buf` nonpaged, `blocks * 512` bytes.
pub unsafe fn transfer(
    ext: *mut AdapterExtension,
    info: &CardInfo,
    write: bool,
    lba: u64,
    blocks: u16,
    buf: *mut u8,
) -> Result<(), HostError> {
    let Some(addr) = sd::block_address(lba, info.high_capacity) else {
        return Err(HostError::Status(0x8000_0000)); // out of range
    };
    let multi = blocks > 1;
    let c = match (write, multi) {
        (false, false) => sd::read_single(addr),
        (false, true) => sd::read_multiple(addr),
        (true, false) => sd::write_single(addr),
        (true, true) => sd::write_multiple(addr),
    };
    let t0 = now_us();
    let t = unsafe { command(ext, &c, info.rca)? };
    match sd::payload48(t.as_slice()) {
        Some(s) if s & sd::R1_ERRORS == 0 => {}
        Some(s) => return Err(HostError::Status(s)),
        None => return Err(HostError::Cmd(c.index, CmdError::Short)),
    }
    perf_add(2, now_us() - t0);

    let result = unsafe { data_phase(ext, write, blocks, buf) };
    if multi {
        let t = now_us();
        let _ = unsafe { command(ext, &sd::stop_transmission(), info.rca) };
        perf_add(6, now_us() - t);
    }
    perf_add(0, 1);
    perf_add(1, u64::from(blocks));
    if result.is_err() {
        unsafe {
            clear_error(ext);
            let _ = chip::write_reg(ext, reg::MC_FIFO_CTL, 0x01, 0x01);
        }
    }
    result
}

/// Bus speed of an initialised card.
#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Speed {
    /// Default speed, 25 MHz (SDR12-class timing at 1.8 V).
    Default = 0,
    /// SD High Speed, 50 MHz, 3.3 V (CMD6 group 1, function 1).
    High = 1,
    /// UHS-I SDR50, 100 MHz, 1.8 V, tuned sample phase (group 1, function 2).
    Sdr50 = 2,
}

/// SDR50 data clock.
const SDR50_CLOCK_MHZ: u32 = 100;

/// Set the data clock and bus timing for `speed`. The card must already be
/// in that mode (see [`try_high_speed`], [`try_sdr50`]).
///
/// # Safety
/// Worker context, slot powered.
pub unsafe fn set_speed(ext: *mut AdapterExtension, speed: Speed) -> Result<(), HostError> {
    unsafe {
        // The clock batch resets the sample/push points, so timing goes last.
        match speed {
            Speed::Default => {
                set_clock(ext, false, DATA_CLOCK_MHZ)?;
                apply(ext, rtsx_sd::timing_batch(rtsx_sd::Timing::Default))
            }
            Speed::High => {
                set_clock(ext, false, HS_CLOCK_MHZ)?;
                apply(ext, rtsx_sd::timing_batch(rtsx_sd::Timing::HighSpeed))
            }
            Speed::Sdr50 => {
                let s = rtsx_sd::ssc_for_uhs(SDR50_CLOCK_MHZ);
                apply(ext, rtsx_sd::clock_batch(&s))?;
                chip::delay_us(1_000);
                apply(ext, rtsx_sd::clock_finish_batch())?;
                apply(ext, rtsx_sd::timing_batch(rtsx_sd::Timing::Sdr50))
            }
        }
    }
}

/// CMD6 (switch function) `c` with its 64-byte status block read into
/// `status`.
unsafe fn switch_function(
    ext: *mut AdapterExtension,
    c: &Command,
    status: &mut [u8; sd::SWITCH_STATUS_LEN],
) -> Result<(), HostError> {
    #[allow(clippy::cast_possible_truncation)]
    let Ok(mut b) = rtsx_sd::read_data_batch(c, sd::SWITCH_STATUS_LEN as u16) else {
        return Err(HostError::Cmd(6, CmdError::Short));
    };
    let mut rsp = [0u8; 8];
    let n = b.response_len();
    let got = match unsafe { exchange(ext, &mut b, &mut rsp[..n], CMD_TIMEOUT_MS) } {
        Ok(got) => got,
        Err(e) => {
            unsafe { clear_error(ext) };
            return Err(HostError::Chip(e));
        }
    };
    // rsp: SD_TRANSFER, then the R1 card status (big-endian).
    if got < 5 || rsp[0] & rtsx_sd::TRANSFER_ERR != 0 {
        unsafe { clear_error(ext) };
        return Err(HostError::Cmd(6, CmdError::Transfer(rsp[0])));
    }
    let card_status = u32::from_be_bytes([rsp[1], rsp[2], rsp[3], rsp[4]]);
    if card_status & sd::R1_ERRORS != 0 {
        return Err(HostError::Status(card_status));
    }
    #[allow(clippy::cast_possible_truncation)]
    let Ok(mut b) = rtsx_sd::read_ppbuf_batch(sd::SWITCH_STATUS_LEN as u16) else {
        return Err(HostError::Cmd(6, CmdError::Short));
    };
    match unsafe { exchange(ext, &mut b, status, CMD_TIMEOUT_MS) } {
        Ok(n) if n == sd::SWITCH_STATUS_LEN => Ok(()),
        Ok(_) => Err(HostError::Cmd(6, CmdError::Short)),
        Err(e) => Err(HostError::Chip(e)),
    }
}

/// Switch an initialised 3.3 V card (transfer state, 4-bit bus) to SD High
/// Speed if it supports it. On any failure the card and host stay at
/// default speed. Result recorded as `SdSpeed` (0 default, 1 high,
/// `0xE0xx` = failed at step `xx`).
///
/// # Safety
/// Worker context, card initialised.
pub unsafe fn try_high_speed(ext: *mut AdapterExtension) -> Speed {
    let mut status = [0u8; sd::SWITCH_STATUS_LEN];
    // CMD6 needs SD spec 1.10+; older cards reject it as an illegal command.
    if unsafe { switch_function(ext, &sd::switch_high_speed(false), &mut status) }.is_err() {
        diag::set_u32(w!("SdSpeed"), 0xE001);
        return Speed::Default;
    }
    if !sd::switch_supports_high_speed(&status) {
        diag::set_u32(w!("SdSpeed"), 0);
        return Speed::Default;
    }
    let switched = unsafe { switch_function(ext, &sd::switch_high_speed(true), &mut status) }
        .is_ok()
        && sd::switch_selected_high_speed(&status);
    if !switched {
        diag::set_u32(w!("SdSpeed"), 0xE002);
        return Speed::Default;
    }
    // The card switches within 8 clocks after the status block; the host
    // follows.
    if unsafe { set_speed(ext, Speed::High) }.is_err() {
        // The card is in High Speed now; the host must still be able to talk
        // to it. Default timing at 25 MHz is within High Speed limits.
        let _ = unsafe { set_speed(ext, Speed::Default) };
        diag::set_u32(w!("SdSpeed"), 0xE003);
        return Speed::Default;
    }
    diag::set_u32(w!("SdSpeed"), 1);
    Speed::High
}

/// Whether the card holds any of DAT0..DAT3 low (busy). Lets the clock run
/// freely for 1 ms first, as the UHS-I voltage switch requires.
unsafe fn dat_lines_busy(ext: *mut AdapterExtension) -> Result<bool, HostError> {
    unsafe {
        apply(ext, rtsx_sd::clock_toggle_batch(true))?;
        chip::delay_us(1_000);
        let mut b = Batch::new();
        let _ = b.read(reg::SD_BUS_STAT);
        let mut st = [0u8; 4];
        let n = exchange(ext, &mut b, &mut st[..4], CMD_TIMEOUT_MS).map_err(HostError::Chip)?;
        apply(ext, rtsx_sd::clock_toggle_batch(false))?;
        if n == 0 {
            return Err(HostError::Cmd(11, CmdError::Short));
        }
        Ok(st[0] & rtsx_sd::BUS_DAT_MASK != rtsx_sd::BUS_DAT_MASK)
    }
}

/// Host side of the UHS-I voltage switch, after the card accepted CMD11: the
/// card drives DAT low; gate the clock, move pads and LDO to 1.8 V, wait,
/// restart the clock, and check the card released DAT (it switched).
unsafe fn host_voltage_switch(ext: *mut AdapterExtension) -> Result<(), HostError> {
    let fail = HostError::Init(InitError::VoltageSwitch);
    unsafe {
        chip::delay_us(1_000);
        if !dat_lines_busy(ext)? {
            return Err(fail);
        }
        apply(ext, rtsx_sd::signal_voltage_batch(true))?;
        // Spec: clock gated for at least 5 ms; 10 ms like Linux.
        chip::delay_us(10_000);
        if dat_lines_busy(ext)? {
            return Err(fail);
        }
    }
    Ok(())
}

/// One CMD19 tuning read at the current RX phase.
unsafe fn tuning_read_ok(ext: *mut AdapterExtension) -> bool {
    let Ok(mut b) = rtsx_sd::tuning_batch() else {
        return false;
    };
    let mut rsp = [0u8; 8];
    let n = b.response_len();
    let ok = matches!(
        unsafe { exchange(ext, &mut b, &mut rsp[..n], CMD_TIMEOUT_MS) },
        Ok(got) if got >= 5 && rsp[0] & rtsx_sd::TRANSFER_ERR == 0
    );
    if !ok {
        unsafe {
            // Let the data lines go idle before the next attempt.
            for _ in 0..100 {
                match chip::read_reg(ext, reg::SD_DATA_STATE) {
                    Ok(v) if v & 0x80 != 0 => break,
                    Ok(_) => chip::delay_us(1_000),
                    Err(_) => break,
                }
            }
            clear_error(ext);
        }
    }
    ok
}

/// Tune the RX sample phase for SDR50: fixed TX phase, then sweep the 16 RX
/// phases with CMD19 three times and take the middle of the longest window
/// that passed every time. Records `SdTuneMap` and `SdPhase`.
unsafe fn tune_rx(ext: *mut AdapterExtension) -> Result<(), HostError> {
    unsafe { apply(ext, rtsx_sd::phase_batch(1, true))? };
    let mut map = 0xFFFFu16;
    for _ in 0..3 {
        let mut round = 0u16;
        for phase in (0..=rtsx_sd::MAX_PHASE).rev() {
            unsafe { apply(ext, rtsx_sd::phase_batch(phase, false))? };
            if unsafe { tuning_read_ok(ext) } {
                round |= 1 << phase;
            }
        }
        map &= round;
        if round == 0 {
            break;
        }
    }
    diag::set_u32(w!("SdTuneMap"), u32::from(map));
    let Some(phase) = rtsx_sd::final_phase(map) else {
        return Err(HostError::Cmd(19, CmdError::Transfer(0)));
    };
    diag::set_u32(w!("SdPhase"), u32::from(phase));
    unsafe { apply(ext, rtsx_sd::phase_batch(phase, false)) }
}

/// Bring a 1.8 V (UHS-I) card in transfer state to SDR50: raise the current
/// limit as far as the card allows (up to 800 mA), select SDR50, switch the
/// host to 100 MHz SD 3.0 timing and tune. On error the card must be power
/// cycled (it may be left in SDR50).
///
/// # Safety
/// Worker context, card initialised with `CardInfo::uhs`.
pub unsafe fn try_sdr50(ext: *mut AdapterExtension) -> Result<(), HostError> {
    let mut status = [0u8; sd::SWITCH_STATUS_LEN];
    let query = sd::switch_function(false, sd::GROUP_BUS_SPEED, sd::FN_SDR50);
    unsafe { switch_function(ext, &query, &mut status)? };
    if !sd::switch_supports(&status, sd::GROUP_BUS_SPEED, sd::FN_SDR50) {
        return Err(HostError::Cmd(6, CmdError::Transfer(0)));
    }
    // Current limit: 3 = 800 mA, 2 = 600, 1 = 400, 0 = 200 (the default).
    if let Some(limit) = (1..=3u8)
        .rev()
        .find(|&f| sd::switch_supports(&status, sd::GROUP_CURRENT_LIMIT, f))
    {
        let c = sd::switch_function(true, sd::GROUP_CURRENT_LIMIT, limit);
        if unsafe { switch_function(ext, &c, &mut status) }.is_ok() {
            diag::set_u32(
                w!("SdCurrentLimit"),
                u32::from(sd::switch_selected(&status, sd::GROUP_CURRENT_LIMIT)),
            );
        }
    }
    let set = sd::switch_function(true, sd::GROUP_BUS_SPEED, sd::FN_SDR50);
    unsafe { switch_function(ext, &set, &mut status)? };
    if sd::switch_selected(&status, sd::GROUP_BUS_SPEED) != sd::FN_SDR50 {
        return Err(HostError::Cmd(6, CmdError::Transfer(1)));
    }
    unsafe {
        set_speed(ext, Speed::Sdr50)?;
        tune_rx(ext)
    }
}

/// What to try when bringing up a card.
#[derive(Clone, Copy)]
pub struct Modes {
    /// Ask UHS-I cards for 1.8 V and SDR50 (`Parameters\Uhs`, default on).
    pub uhs: bool,
    /// SD High Speed for 3.3 V cards (`Parameters\HighSpeed`, default on).
    pub high_speed: bool,
}

/// Power-cycle the slot (card back to idle, pads back to 3.3 V).
unsafe fn power_cycle(ext: *mut AdapterExtension) {
    unsafe {
        power_off(ext);
        chip::delay_us(20_000);
    }
}

/// Initialise the card in the slot and bring it to the fastest mode that
/// works: SDR50 (1.8 V) if allowed and supported, else High Speed, else
/// default speed. A failure in the 1.8 V path power-cycles the card and
/// retries at 3.3 V. Records `SdSpeed` (2 = SDR50).
///
/// # Safety
/// Worker context; USB and controller initialised.
pub unsafe fn bring_up_card(
    ext: *mut AdapterExtension,
    modes: Modes,
) -> Result<(CardInfo, Speed), HostError> {
    // Always from a real power cycle: a card left powered (e.g. across a
    // driver restart) may still be at 1.8 V signaling, and would then not
    // offer 1.8 V again but answer at the wrong levels.
    unsafe { power_cycle(ext) };
    if modes.uhs {
        match unsafe { init_card(ext, true) } {
            Ok(info) if info.uhs => match unsafe { try_sdr50(ext) } {
                Ok(()) => {
                    diag::set_u32(w!("SdSpeed"), 2);
                    return Ok((info, Speed::Sdr50));
                }
                Err(e) => {
                    diag::set_u32(w!("SdUhsError"), error_code(e));
                    unsafe { power_cycle(ext) };
                }
            },
            Ok(info) => {
                // Not a UHS card: it is initialised at 3.3 V already.
                let speed = if modes.high_speed {
                    unsafe { try_high_speed(ext) }
                } else {
                    Speed::Default
                };
                return Ok((info, speed));
            }
            Err(HostError::Init(InitError::VoltageSwitch)) => {
                diag::set_u32(w!("SdUhsError"), 0xE303);
                unsafe { power_cycle(ext) };
            }
            Err(e) => return Err(e),
        }
    }
    let info = unsafe { init_card(ext, false)? };
    let speed = if modes.high_speed {
        unsafe { try_high_speed(ext) }
    } else {
        Speed::Default
    };
    Ok((info, speed))
}

unsafe fn data_phase(
    ext: *mut AdapterExtension,
    write: bool,
    blocks: u16,
    buf: *mut u8,
) -> Result<(), HostError> {
    let batch = if write {
        rtsx_sd::write_batch(blocks)
    } else {
        rtsx_sd::read_batch(blocks)
    };
    let Ok(mut b) = batch else {
        return Err(HostError::Status(0));
    };
    let stage = if write { STAGE_DATA_OUT } else { STAGE_DATA_IN };
    let t0 = now_us();
    unsafe { chip::send_packet(ext, &mut b, stage).map_err(HostError::Chip)? };
    let t1 = now_us();
    perf_add(3, t1 - t0);
    let total = u32::from(blocks) * 512;
    let pipe = unsafe {
        if write {
            (*ext).bulk_out
        } else {
            (*ext).bulk_in
        }
    };
    let x = unsafe { usb::bulk(ext, pipe, buf, total, !write, DATA_TIMEOUT_MS) };
    let t2 = now_us();
    perf_add(4, t2 - t1);
    if !x.ok() || x.len != total {
        unsafe { chip::clear_fsm(ext) };
        return Err(HostError::Chip(ChipError(x)));
    }
    let mut status = [0u8; 4];
    let n =
        unsafe { chip::read_response(ext, &mut status, CMD_TIMEOUT_MS).map_err(HostError::Chip)? };
    perf_add(5, now_us() - t2);
    if rtsx_sd::data_status_ok(&status[..n]) {
        Ok(())
    } else {
        Err(HostError::Status(u32::from(status[0])))
    }
}
