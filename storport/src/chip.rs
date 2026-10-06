// Copyright (c) SoraCard
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Realtek RTS5129-family controller access over USB ("RTCR" protocol).
//!
//! These readers have no SCSI firmware: the host programs controller
//! registers, either one at a time over the control pipe or in batches over
//! the bulk pipes. Packet framing lives in `sora_core::rtsx`; this module
//! only moves the bytes and runs the controller init.
//!
//! Everything here runs at PASSIVE_LEVEL in worker context (bring-up or the
//! I/O worker thread), because it uses the shared worker URB.

use sora_core::rtsx::{self, reg, Batch, CardStatus};

use crate::diag::{self, w};
use crate::{io, usb, AdapterExtension};

const TIMEOUT_MS: u32 = 2_000;

/// A transfer failed; carries the USB result for diagnostics.
#[derive(Clone, Copy)]
pub struct ChipError(pub usb::Xfer);

/// ep0 masked register write.
///
/// # Safety
/// Worker context, PASSIVE_LEVEL, after USB bring-up.
pub unsafe fn write_reg(
    ext: *mut AdapterExtension,
    addr: u16,
    mask: u8,
    value: u8,
) -> Result<(), ChipError> {
    let x = unsafe {
        usb::control(
            ext,
            rtsx::ep0_write_register(addr, mask, value),
            core::ptr::null_mut(),
            TIMEOUT_MS,
        )
    };
    if x.ok() {
        Ok(())
    } else {
        Err(ChipError(x))
    }
}

/// ep0 single register read.
///
/// # Safety
/// As [`write_reg`].
pub unsafe fn read_reg(ext: *mut AdapterExtension, addr: u16) -> Result<u8, ChipError> {
    let buf = unsafe { io::scratch(ext) };
    let x = unsafe { usb::control(ext, rtsx::ep0_read_register(addr), buf, TIMEOUT_MS) };
    if x.ok() && x.len >= 1 {
        Ok(unsafe { *buf })
    } else {
        Err(ChipError(x))
    }
}

/// Card presence via the ep0 status poll (cheap; safe to call per command).
///
/// # Safety
/// As [`write_reg`].
pub unsafe fn poll_status(ext: *mut AdapterExtension) -> Result<CardStatus, ChipError> {
    let (x, raw) = unsafe { poll_raw(ext) };
    match rtsx::parse_status(&raw[..x.len.min(2) as usize]) {
        Some(s) if x.ok() => Ok(s),
        _ => Err(ChipError(x)),
    }
}

/// The raw ep0 status poll: transfer result and the (up to) two bytes read.
///
/// # Safety
/// As [`write_reg`].
pub unsafe fn poll_raw(ext: *mut AdapterExtension) -> (usb::Xfer, [u8; 2]) {
    let buf = unsafe { io::scratch(ext) };
    let x = unsafe { usb::control(ext, rtsx::ep0_poll_status(), buf, TIMEOUT_MS) };
    let mut raw = [0u8; 2];
    let n = x.len.min(2) as usize;
    // SAFETY: `n` bytes arrived in scratch.
    unsafe { core::ptr::copy_nonoverlapping(buf, raw.as_mut_ptr(), n) };
    (x, raw)
}

/// Send a batch on bulk OUT and, if it has reads, collect the response.
/// Returns the number of response bytes (copied into `rsp`).
///
/// On any failure the controller's command FSM is cleared, as the controller
/// otherwise stays wedged after a bad packet.
///
/// # Safety
/// As [`write_reg`].
pub unsafe fn send(
    ext: *mut AdapterExtension,
    batch: &mut Batch,
    rsp: &mut [u8],
) -> Result<usize, ChipError> {
    let want = batch.response_len();
    let pkt = batch.encode(0);
    let buf = unsafe { io::scratch(ext) };
    // SAFETY: scratch holds at least MAX_PACKET bytes.
    unsafe { core::ptr::copy_nonoverlapping(pkt.as_ptr(), buf, pkt.len()) };
    #[allow(clippy::cast_possible_truncation)]
    let x = unsafe {
        usb::bulk(
            ext,
            (*ext).bulk_out,
            buf,
            pkt.len() as u32,
            false,
            TIMEOUT_MS,
        )
    };
    if !x.ok() {
        unsafe { clear_fsm(ext) };
        return Err(ChipError(x));
    }
    if want == 0 {
        return Ok(0);
    }
    #[allow(clippy::cast_possible_truncation)]
    let x = unsafe { usb::bulk(ext, (*ext).bulk_in, buf, want as u32, true, TIMEOUT_MS) };
    if !x.ok() {
        unsafe { clear_fsm(ext) };
        return Err(ChipError(x));
    }
    let n = (x.len as usize).min(rsp.len());
    // SAFETY: `x.len` bytes arrived in scratch.
    unsafe { core::ptr::copy_nonoverlapping(buf, rsp.as_mut_ptr(), n) };
    Ok(n)
}

/// Send a batch on bulk OUT with the given stage flags, without collecting a
/// response (for batches followed by a data phase).
///
/// # Safety
/// As [`write_reg`].
pub unsafe fn send_packet(
    ext: *mut AdapterExtension,
    batch: &mut Batch,
    stage: u8,
) -> Result<(), ChipError> {
    let pkt = batch.encode(stage);
    let buf = unsafe { io::scratch(ext) };
    // SAFETY: scratch holds at least MAX_PACKET bytes.
    unsafe { core::ptr::copy_nonoverlapping(pkt.as_ptr(), buf, pkt.len()) };
    #[allow(clippy::cast_possible_truncation)]
    let x = unsafe {
        usb::bulk(
            ext,
            (*ext).bulk_out,
            buf,
            pkt.len() as u32,
            false,
            TIMEOUT_MS,
        )
    };
    if x.ok() {
        Ok(())
    } else {
        unsafe { clear_fsm(ext) };
        Err(ChipError(x))
    }
}

/// Read a batch response of `n` bytes (rounded up to 4) from bulk IN.
///
/// # Safety
/// As [`write_reg`].
pub unsafe fn read_response(
    ext: *mut AdapterExtension,
    rsp: &mut [u8],
    timeout_ms: u32,
) -> Result<usize, ChipError> {
    let want = (rsp.len() + 3) & !3;
    let buf = unsafe { io::scratch(ext) };
    #[allow(clippy::cast_possible_truncation)]
    let x = unsafe { usb::bulk(ext, (*ext).bulk_in, buf, want as u32, true, timeout_ms) };
    if !x.ok() {
        unsafe { clear_fsm(ext) };
        return Err(ChipError(x));
    }
    let n = (x.len as usize).min(rsp.len());
    // SAFETY: `x.len` bytes arrived in scratch.
    unsafe { core::ptr::copy_nonoverlapping(buf, rsp.as_mut_ptr(), n) };
    Ok(n)
}

/// Clear the controller's command FSM error state (best effort).
///
/// # Safety
/// As [`write_reg`].
pub unsafe fn clear_fsm(ext: *mut AdapterExtension) {
    let _ = unsafe {
        usb::control(
            ext,
            rtsx::ep0_write_register(reg::SFSM_ED, 0xF8, 0xF8),
            core::ptr::null_mut(),
            TIMEOUT_MS,
        )
    };
}

/// Identify and initialise the controller. Returns the hardware version when
/// this is an RTCR controller, `None` when it does not answer (not this
/// family — the caller then uses Bulk-Only Transport).
///
/// # Safety
/// Bring-up context (PASSIVE_LEVEL, USB pipes ready, worker not running).
pub unsafe fn init(ext: *mut AdapterExtension) -> Option<u8> {
    unsafe { clear_fsm(ext) };

    // Power the internal clock (SSC) and set the clock divider.
    let mut b = Batch::new();
    let _ = b.write(reg::FPDCTL, 0x01, 0x00);
    if unsafe { send(ext, &mut b, &mut []) }.is_err() {
        diag::set_u32(w!("ChipInit"), 0xE1);
        return None;
    }
    unsafe { delay_us(100) };
    let mut b = Batch::new();
    let _ = b.write(reg::CLK_DIV, 0x80, 0x00);
    let _ = unsafe { send(ext, &mut b, &mut []) };

    // Identify.
    let mut b = Batch::new();
    let _ = b.read(reg::HW_VERSION);
    let _ = b.read(reg::CARD_SHARE_MODE);
    let _ = b.read(reg::CFG_MODE_1);
    let _ = b.read(reg::CFG_MODE);
    let mut id = [0u8; 4];
    match unsafe { send(ext, &mut b, &mut id) } {
        Ok(n) if n >= 4 => {}
        _ => {
            diag::set_u32(w!("ChipInit"), 0xE2);
            return None;
        }
    }
    diag::set_bin(w!("ChipId"), &id); // HW_VERSION, CARD_SHARE_MODE, CFG_MODE_1, CFG_MODE

    // Package: CARD_SHARE_MODE bit 2 selects LQFP48 (else QFN24).
    let lqfp48 = id[1] & 0x04 != 0;
    // SAFETY: bring-up context; nothing else reads it yet.
    unsafe { (*ext).lqfp48 = lqfp48 };

    // Base configuration: card-detect deglitch, drive strength, LDO, DMA,
    // clear pending card interrupts.
    let mut b = Batch::new();
    if lqfp48 {
        // LQFP48: card LDO to suspend, force the LDO power-good signal, and
        // its pull defaults.
        let _ = b
            .write(reg::CARD_PWR_CTL, 0x0C, 0x08)
            .and_then(|b| b.write(reg::CARD_PWR_CTL, 0x60, 0x60))
            .and_then(|b| b.write(reg::CARD_PULL_CTL1, 0x30, 0x10))
            .and_then(|b| b.write(reg::CARD_PULL_CTL5, 0x03, 0x01))
            .and_then(|b| b.write(reg::CARD_PULL_CTL6, 0x0C, 0x04));
    }
    let _ = b
        .write(reg::SYS_DUMMY0, 0x01, 0x01)
        .and_then(|b| b.write(reg::CD_DEGLITCH_WIDTH, 0xFF, 0x08))
        .and_then(|b| b.write(reg::CD_DEGLITCH_EN, 0x04, 0x00))
        .and_then(|b| b.write(reg::SD30_DRIVE_SEL, 0x07, 0x01))
        .and_then(|b| b.write(reg::CARD_DRIVE_SEL, 0x03, 0x00))
        .and_then(|b| b.write(reg::LDO_POWER_CFG, 0xE0, 0x00))
        .and_then(|b| b.write(reg::CARD_DMA1_CTL, 0x02, 0x02))
        .and_then(|b| b.write(reg::CARD_INT_PEND, 0x1C, 0x1C))
        // Over-current protection.
        .and_then(|b| b.write(reg::OCPCTL, 0x08, 0x08))
        .and_then(|b| b.write(reg::OCPPARA1, 0xF0, 0x50))
        .and_then(|b| b.write(reg::OCPPARA2, 0x07, 0x03));
    // RTS5179-class parts (CFG_MODE_1 bit 1) need this pull-control setting.
    if id[2] & 0x02 != 0 {
        let _ = b.write(reg::CARD_PULL_CTL5, 0x03, 0x01);
    }
    if unsafe { send(ext, &mut b, &mut []) }.is_err() {
        diag::set_u32(w!("ChipInit"), 0xE3);
        return None;
    }
    // Crystal-free parts (CFG_MODE bit 7, or mode field 1) need PHY reg
    // 0xC2 = 0x7C for a stable USB clock.
    if id[3] & 0x80 != 0 || id[3] & 0x03 == 0x01 {
        if let Ok(mut b) = sora_core::rtsx_sd::phy_write_batch(0xC2, 0x7C) {
            let _ = unsafe { send(ext, &mut b, &mut []) };
        }
    }

    let status = unsafe { poll_status(ext) }.map_or(0xFF, |s| {
        u32::from(s.sd) | u32::from(s.ms) << 1 | u32::from(s.xd) << 2
    });
    diag::set_u32(w!("CardStatusAtInit"), status);
    diag::set_u32(w!("ChipInit"), 1);
    Some(id[0])
}

/// Sleep for at least `us` microseconds (rounded up to the 1 ms timer).
///
/// # Safety
/// PASSIVE_LEVEL.
pub unsafe fn delay_us(us: i64) {
    let mut t = wdk_sys::LARGE_INTEGER {
        QuadPart: -(us * 10).max(10_000),
    };
    let _ = unsafe {
        wdk_sys::ntddk::KeDelayExecutionThread(
            wdk_sys::_MODE::KernelMode as wdk_sys::KPROCESSOR_MODE,
            0,
            &raw mut t,
        )
    };
}
