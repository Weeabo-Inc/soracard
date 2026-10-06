// Copyright (c) SoraCard
// SPDX-License-Identifier: MIT OR Apache-2.0

//! The data path: SCSI requests → a worker thread → the card.
//!
//! `HwStorStartIo` runs at up to DISPATCH_LEVEL and must not block, while USB
//! transfers, pipe resets and waits need PASSIVE_LEVEL — and the reader is
//! strictly one command at a time anyway. So `HwStorStartIo` only enqueues the
//! SRB and signals one dedicated system thread, which owns all USB traffic.
//! Two backends:
//!
//! ```text
//! Realtek RTCR (RTS5129):  worker ─▶ card detect ─▶ scsi_target ─▶ [sdhost transfer] ─▶ complete
//! Bulk-Only Transport:     worker ─▶ CBW ─▶ data ─▶ CSW ─▶ [REQUEST SENSE] ─▶ complete
//! ```
//!
//! On the RTCR backend the driver is the SCSI target and detects the card by
//! whether it answers SD commands (`process_rtsx`; see docs/ARCHITECTURE.md
//! §6). Protocol decisions (SCSI answers, media-change semantics, CSW
//! interpretation, sense layout) come from `sora-core`; this module only
//! sequences and moves bytes.
//!
//! ## Resets and exactly-once completion
//!
//! A reset (`HwStorResetBus`, `SRB_FUNCTION_RESET_*`) completes every *queued*
//! SRB itself, but never touches the *in-flight* one: it cancels that SRB's
//! USB transfer and flags reset recovery, and the worker — which then fails
//! fast through the gate — completes it with `SRB_STATUS_BUS_RESET`. So the
//! worker owns its SRB until it completes it, and nothing is ever used after
//! completion or completed twice (both guaranteed bugchecks).
//!
//! Every SRB also gets one deadline, shorter than its own `TimeOutValue`,
//! that bounds all its USB transfers together, so we normally finish before
//! StorPort's timer would fire.

use core::ffi::c_void;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use sora_core::bot_plan::{self, BotTransaction, DataDirection, Disposition, TagGenerator};
use sora_core::scsi_target::{Media, Reply, Target};
use sora_core::{bot, scsi, scsi_emu};
use storport_sys as sp;
use wdk_sys as wk;

use crate::diag::{self, w};
use crate::{chip, sdhost, spx, usb, AdapterExtension};

/// SRBs we can hold queued (StorPort gets `SRB_STATUS_BUSY` beyond this).
pub const QUEUE_LEN: usize = 32;
/// Largest single transfer; advertised as `MaximumTransferLength`.
pub const BOUNCE_LEN: usize = 64 * 1024;
/// Largest sector transfer on the RTCR backend, done in place on the SRB's
/// own (locked, system-mapped) buffer: one SD multi-block command. Same limit
/// as the Linux `rtsx_usb_sdmmc` driver.
pub const MAX_XFER: usize = 512 * 1024;

const CBW_TIMEOUT_MS: u32 = 5_000;
/// Upper bound for one data or CSW transfer (the SRB budget usually binds).
const PHASE_TIMEOUT_MS: u32 = 20_000;
const CONTROL_TIMEOUT_MS: u32 = 5_000;
const HISTORY_LEN: usize = 64;
const XFER_LOG_LEN: usize = 64;
const POLL_LOG_LEN: usize = 32;
/// Card-detect: CMD13 liveness check interval while a card is ready.
const CHECK_MS: u64 = 500;
/// Card-detect: init retry interval while no card answers.
const RETRY_MS: u64 = 1_000;
/// Card-detect: after the slot has been empty this long, probe less often.
const IDLE_AFTER_MS: u64 = 60_000;
/// Card-detect: init retry interval for a long-empty slot (still well inside
/// the 5 s insert target, at half the power-cycling).
const SLOW_RETRY_MS: u64 = 2_000;
/// Record every command for this many commands, then only errors (plus one
/// periodic refresh) so steady-state I/O does not hammer the registry.
const TRACE_ALL_FIRST: u32 = 512;

const READY_PENDING: u32 = 0;
const READY_OK: u32 = 1;
const READY_FAILED: u32 = 2;

/// Lowest kernel-mode virtual address on x64.
const KERNEL_VA_START: usize = 0xFFFF_8000_0000_0000;

// ---- Diagnostic value names --------------------------------------------------

const D_CMDS: &[u16] = w!("Cmds");
const D_ERRS: &[u16] = w!("CmdErrors");
const D_RESETS: &[u16] = w!("Resets");
const D_HISTORY: &[u16] = w!("History");
const D_HIST_NEXT: &[u16] = w!("HistoryNext");
const D_LAST_NT: &[u16] = w!("LastXferNt");
const D_LAST_USBD: &[u16] = w!("LastXferUsbd");
const D_LAST_PHASE: &[u16] = w!("LastXferPhase");
const D_INQUIRY: &[u16] = w!("Inquiry");
const D_CAPACITY: &[u16] = w!("Capacity10");
const D_VA_FALLBACK: &[u16] = w!("VaFallback");
const D_THREAD: &[u16] = w!("WorkerStatus");

// ---- State -------------------------------------------------------------------

/// The part of the I/O state the USB submit path needs: lets a reset cancel
/// the in-flight IRP and keep new transfers off the wire until recovery.
#[repr(C)]
pub struct Gate {
    /// Protects this struct and the queue fields of [`IoState`].
    lock: wk::KSPIN_LOCK,
    /// Reset recovery must run before the next transfer.
    reset_pending: bool,
    /// IRP currently submitted by the worker (null when idle).
    inflight_irp: wk::PIRP,
}

/// Per-adapter I/O state, embedded in [`AdapterExtension`] (nonpaged).
#[repr(C)]
pub struct IoState {
    gate: Gate,
    // -- protected by `gate.lock` --
    ring: [sp::PSCSI_REQUEST_BLOCK; QUEUE_LEN],
    head: usize,
    count: usize,
    inflight: sp::PSCSI_REQUEST_BLOCK,
    // -- lock-free --
    wake: wk::KEVENT,
    ready: AtomicU32,
    stop: AtomicBool,
    thread: wk::PVOID,
    // -- worker thread only --
    tags: TagGenerator,
    cbw: [u8; bot::CBW_LEN],
    csw: [u8; bot::CSW_LEN],
    sense: [u8; scsi_emu::SENSE_LEN],
    cmds: u32,
    errors: u32,
    resets: u32,
    va_fallback: u32,
    have_inquiry: bool,
    /// IRQL `HwStorInitialize` ran at (+1; 0 = never), reported by the worker.
    init_irql: AtomicU32,
    /// Interrupt-time deadline (100 ns) for the SRB being processed.
    deadline: u64,
    /// `Parameters\\AllowWrites`: when false the medium is presented
    /// write-protected and write-class commands are refused.
    allow_writes: bool,
    history: [[u8; 16]; HISTORY_LEN],
    /// Every USB transfer outcome: `[phase, cdb0, 0, 0, nt:u32, usbd:u32, len:u32]`.
    xfer_log: [[u8; 16]; XFER_LOG_LEN],
    xfer_next: u32,
    /// CDB opcode of the command in flight (for the transfer log).
    cur_op: u8,
    /// Controller backend: true = Realtek RTCR (host-side SCSI target),
    /// false = Bulk-Only Transport pass-through.
    rtsx: bool,
    /// SCSI target state (RTCR backend).
    target: Target,
    media: Media,
    /// The initialised card (valid while `media.ready`).
    card: sora_core::sd::CardInfo,
    /// The SD slot is powered.
    powered: bool,
    /// Polls to skip before retrying a failed card initialisation.
    /// Set by `HwStorAdapterControl(ScsiRestartAdapter)`; the worker re-inits.
    restarted: AtomicBool,
    /// The medium was ejected (START STOP UNIT with LoEj): the card stays
    /// unpowered and reported absent until it is pulled from the slot.
    ejected: bool,
    /// Bus speed the card runs at (Default = 0 when zeroed).
    speed: sdhost::Speed,
    /// Speed downgrades after transfer errors (diagnostics).
    speed_fallbacks: u32,
    /// When the slot was last found empty (ms since boot; 0 = has a card).
    empty_since_ms: u64,
    /// Card-detect schedule (ms since boot): next CMD13 liveness check while
    /// a card is ready, next init attempt while none is.
    next_check_ms: u64,
    next_init_ms: u64,
    /// Last raw status-poll word (bit 31 set once valid); a change wakes the
    /// detector immediately.
    last_raw: u32,
    /// Card-detect poll trace (see [`trace_poll`]).
    poll_count: u32,
    poll_sig: u32,
    poll_next: u32,
    poll_log: [[u8; 8]; POLL_LOG_LEN],
    /// Card's own write protection (CSD or switch).
    card_wp: bool,
    /// Nonpaged scratch for controller packets (separate from `bounce`).
    scratch: [u8; sora_core::rtsx::MAX_PACKET],
    bounce: [u8; BOUNCE_LEN],
}

/// Why a BOT command did not produce a disposition.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Fail {
    /// The reader is gone (surprise removal).
    Gone,
    /// The transport was reset; the command's outcome is unknown.
    Reset,
    /// We could not frame the command (bad CDB/length).
    Invalid,
}

/// Which BOT step failed, for `LastXferPhase`.
#[derive(Clone, Copy)]
#[repr(u32)]
enum Phase {
    Cbw = 1,
    Data = 2,
    ClearData = 3,
    Csw = 4,
    ClearCsw = 5,
    CswInvalid = 6,
    Recovery = 7,
    RecoveryClearIn = 8,
    RecoveryClearOut = 9,
}

/// # Safety
/// `ext` must be the live adapter extension.
unsafe fn state(ext: *mut AdapterExtension) -> *mut IoState {
    // SAFETY: field projection on a valid pointer; no reference is created.
    unsafe { core::ptr::addr_of_mut!((*ext).io) }
}

/// The gate the USB submit path uses.
///
/// # Safety
/// `ext` must be the live adapter extension.
pub unsafe fn gate(ext: *mut AdapterExtension) -> *mut Gate {
    // SAFETY: as for `state`.
    unsafe { core::ptr::addr_of_mut!((*ext).io.gate) }
}

struct Locked(*mut Gate, wk::KIRQL);

impl Locked {
    /// # Safety
    /// `g` must point at an initialized [`Gate`]; IRQL <= DISPATCH_LEVEL.
    unsafe fn new(g: *mut Gate) -> Self {
        // SAFETY: initialized spin lock in nonpaged memory.
        let irql =
            unsafe { wk::ntddk::KeAcquireSpinLockRaiseToDpc(core::ptr::addr_of_mut!((*g).lock)) };
        Self(g, irql)
    }
}

impl Drop for Locked {
    fn drop(&mut self) {
        // SAFETY: we hold this lock at DISPATCH_LEVEL; restore the old IRQL.
        unsafe { wk::ntddk::KeReleaseSpinLock(core::ptr::addr_of_mut!((*self.0).lock), self.1) };
    }
}

/// Publish `irp` as in flight. Returns `false` (and publishes nothing) when a
/// reset is pending, in which case the caller must not send it.
///
/// # Safety
/// `g` must be the live gate.
pub unsafe fn gate_enter(g: *mut Gate, irp: wk::PIRP) -> bool {
    // SAFETY: caller contract.
    let _l = unsafe { Locked::new(g) };
    unsafe {
        if (*g).reset_pending {
            return false;
        }
        (*g).inflight_irp = irp;
    }
    true
}

/// Withdraw the in-flight IRP before it is freed.
///
/// # Safety
/// `g` must be the live gate.
pub unsafe fn gate_leave(g: *mut Gate) {
    // SAFETY: caller contract.
    let _l = unsafe { Locked::new(g) };
    unsafe { (*g).inflight_irp = core::ptr::null_mut() };
}

// ---- Lifecycle ---------------------------------------------------------------

/// Initialize locks/events. PASSIVE_LEVEL (`HwStorFindAdapter`).
///
/// # Safety
/// `ext` must be the live, zero-initialized adapter extension.
pub unsafe fn init(ext: *mut AdapterExtension) {
    // SAFETY: caller contract.
    let st = unsafe { state(ext) };
    unsafe {
        if !(*st).thread.is_null() {
            return; // re-entered on restart while the worker still runs
        }
        wk::ntddk::KeInitializeSpinLock(core::ptr::addr_of_mut!((*st).gate.lock));
        // A restart can reuse this extension: start from a clean slate.
        (*st).gate.reset_pending = false;
        (*st).gate.inflight_irp = core::ptr::null_mut();
        (*st).head = 0;
        (*st).count = 0;
        (*st).inflight = core::ptr::null_mut();
        (*st).stop.store(false, Ordering::Release);
        (*st).ready.store(READY_PENDING, Ordering::Release);
        wk::ntddk::KeInitializeEvent(
            core::ptr::addr_of_mut!((*st).wake),
            wk::_EVENT_TYPE::SynchronizationEvent,
            0,
        );
        (*st).tags = TagGenerator::new(1);
    }
}

/// Record whether USB bring-up succeeded (gates `HwStorStartIo`).
///
/// # Safety
/// `ext` must be the live adapter extension.
pub unsafe fn set_ready(ext: *mut AdapterExtension, ok: bool) {
    // SAFETY: caller contract.
    let st = unsafe { state(ext) };
    unsafe {
        (*st)
            .ready
            .store(if ok { READY_OK } else { READY_FAILED }, Ordering::Release)
    };
}

/// Nonpaged scratch for controller packets/responses (`rtsx::MAX_PACKET`).
///
/// # Safety
/// `ext` must be the live adapter extension; worker context only.
pub unsafe fn scratch(ext: *mut AdapterExtension) -> *mut u8 {
    unsafe { core::ptr::addr_of_mut!((*state(ext)).scratch).cast::<u8>() }
}

/// Select the Realtek RTCR backend (called at bring-up when the controller
/// answers). PASSIVE_LEVEL, before the worker starts.
///
/// # Safety
/// `ext` must be the live adapter extension.
pub unsafe fn use_rtsx(ext: *mut AdapterExtension) {
    let st = unsafe { state(ext) };
    unsafe {
        (*st).rtsx = true;
        (*st).target = Target::new();
        (*st).media = Media::default();
    }
}

/// Largest transfer this adapter accepts, for `MaximumTransferLength`
/// (valid after USB bring-up chose the backend).
///
/// # Safety
/// `ext` must be the live adapter extension.
pub unsafe fn max_transfer(ext: *mut AdapterExtension) -> usize {
    if unsafe { (*state(ext)).rtsx } {
        MAX_XFER
    } else {
        BOUNCE_LEN
    }
}

/// Nonpaged scratch buffer of `BOUNCE_LEN` bytes (the worker's bounce buffer).
/// Only for use while the worker is not running (bring-up).
///
/// # Safety
/// `ext` must be the live adapter extension.
pub unsafe fn bounce_ptr(ext: *mut AdapterExtension) -> *mut u8 {
    unsafe { core::ptr::addr_of_mut!((*state(ext)).bounce).cast::<u8>() }
}

/// Remember the IRQL `HwStorInitialize` ran at. Any IRQL.
///
/// # Safety
/// `ext` must be the live adapter extension.
pub unsafe fn note_init_irql(ext: *mut AdapterExtension, irql: u8) {
    unsafe {
        (*state(ext))
            .init_irql
            .store(u32::from(irql) + 1, Ordering::Relaxed)
    };
}

/// Configure write policy (read before the worker starts). PASSIVE_LEVEL.
///
/// # Safety
/// `ext` must be the live adapter extension; worker not yet running.
pub unsafe fn set_allow_writes(ext: *mut AdapterExtension, allow: bool) {
    unsafe { (*state(ext)).allow_writes = allow };
}

/// Start the worker thread. PASSIVE_LEVEL.
///
/// # Safety
/// `ext` must be the live adapter extension with USB state complete.
pub unsafe fn start_worker(ext: *mut AdapterExtension) -> i32 {
    // SAFETY: caller contract.
    let st = unsafe { state(ext) };
    // SAFETY: plain data.
    let mut oa: wk::OBJECT_ATTRIBUTES = unsafe { core::mem::zeroed() };
    #[allow(clippy::cast_possible_truncation)]
    {
        oa.Length = core::mem::size_of::<wk::OBJECT_ATTRIBUTES>() as u32;
    }
    oa.Attributes = wk::OBJ_KERNEL_HANDLE;

    let mut handle: wk::HANDLE = core::ptr::null_mut();
    // SAFETY: PASSIVE_LEVEL; `worker_main` only uses `ext`, which outlives the
    // thread (`stop_worker` joins it before the extension is freed).
    let status = unsafe {
        wk::ntddk::PsCreateSystemThread(
            &raw mut handle,
            wk::THREAD_ALL_ACCESS,
            &raw mut oa,
            core::ptr::null_mut(),
            core::ptr::null_mut(),
            Some(worker_main),
            ext.cast::<c_void>(),
        )
    };
    #[allow(clippy::cast_sign_loss)]
    diag::set_u32(D_THREAD, status as u32);
    if !wk::NT_SUCCESS(status) {
        return status;
    }

    let mut thread: wk::PVOID = core::ptr::null_mut();
    // SAFETY: valid kernel handle; a NULL object type is allowed in kernel mode.
    let status = unsafe {
        wk::ntddk::ObReferenceObjectByHandle(
            handle,
            wk::THREAD_ALL_ACCESS,
            core::ptr::null_mut(),
            wk::_MODE::KernelMode as wk::KPROCESSOR_MODE,
            &raw mut thread,
            core::ptr::null_mut(),
        )
    };
    if !wk::NT_SUCCESS(status) {
        // We could not keep a reference to join later, so the thread must not
        // outlive this call: stop it and wait on the handle instead.
        unsafe {
            (*st).stop.store(true, Ordering::Release);
            let _ = wk::ntddk::KeSetEvent(core::ptr::addr_of_mut!((*st).wake), 0, 0);
            let _ = wk::ntddk::ZwWaitForSingleObject(handle, 0, core::ptr::null_mut());
            let _ = wk::ntddk::ZwClose(handle);
        }
        return status;
    }
    // SAFETY: we own `handle`; the object reference keeps the thread.
    let _ = unsafe { wk::ntddk::ZwClose(handle) };
    unsafe { (*st).thread = thread };
    0
}

/// Stop and join the worker thread, failing anything still queued.
/// PASSIVE_LEVEL (`HwStorFreeAdapterResources`).
///
/// # Safety
/// `ext` must be the live adapter extension.
pub unsafe fn stop_worker(ext: *mut AdapterExtension) {
    // SAFETY: caller contract.
    let st = unsafe { state(ext) };
    unsafe {
        if (*st).ready.load(Ordering::Acquire) != READY_OK {
            return;
        }
        (*st).stop.store(true, Ordering::Release);
        abort_all(ext, sp::SRB_STATUS_NO_DEVICE);
        let thread = (*st).thread;
        if !thread.is_null() {
            let _ = wk::ntddk::KeWaitForSingleObject(
                thread,
                wk::_KWAIT_REASON::Executive,
                wk::_MODE::KernelMode as wk::KPROCESSOR_MODE,
                0,
                core::ptr::null_mut(),
            );
            let _ = wk::ntddk::ObfDereferenceObject(thread);
            (*st).thread = core::ptr::null_mut();
        }
        (*st).ready.store(READY_FAILED, Ordering::Release);
    }
}

// ---- StartIo side (IRQL <= DISPATCH_LEVEL) -----------------------------------

/// Outcome of offering an SRB to the data path.
pub enum Accept {
    Queued,
    /// Queue full; StorPort retries shortly.
    Busy,
    /// The data path is permanently unavailable.
    NoDevice,
}

/// Queue an `EXECUTE_SCSI` SRB for the worker.
///
/// # Safety
/// `ext` must be the live adapter extension, `srb` a live SRB.
pub unsafe fn submit(ext: *mut AdapterExtension, srb: sp::PSCSI_REQUEST_BLOCK) -> Accept {
    // SAFETY: caller contract.
    let st = unsafe { state(ext) };
    // Never answer BUSY while bring-up is pending: StorPort retries BUSY
    // forever, so LUN enumeration would never finish and the adapter could
    // never be stopped (this wedged PnP). Say "no device" instead; bring-up
    // triggers a rescan (`BusChangeDetected`) once it succeeds.
    if unsafe { (*st).ready.load(Ordering::Acquire) } != READY_OK {
        return Accept::NoDevice;
    }
    {
        let _l = unsafe { Locked::new(gate(ext)) };
        unsafe {
            if (*st).count == QUEUE_LEN {
                return Accept::Busy;
            }
            let slot = ((*st).head + (*st).count) % QUEUE_LEN;
            (*st).ring[slot] = srb;
            (*st).count += 1;
        }
    }
    // SAFETY: initialized event; legal at <= DISPATCH_LEVEL.
    let _ = unsafe { wk::ntddk::KeSetEvent(core::ptr::addr_of_mut!((*st).wake), 0, 0) };
    Accept::Queued
}

/// Complete every queued SRB with `srb_status`, cancel the in-flight transfer
/// (its SRB is completed by the worker), and schedule BOT reset recovery.
/// IRQL <= DISPATCH.
///
/// # Safety
/// `ext` must be the live adapter extension with an initialized lock.
pub unsafe fn abort_all(ext: *mut AdapterExtension, srb_status: u32) {
    // SAFETY: caller contract.
    let st = unsafe { state(ext) };
    let mut stolen: [sp::PSCSI_REQUEST_BLOCK; QUEUE_LEN] = [core::ptr::null_mut(); QUEUE_LEN];
    let mut n = 0;
    {
        let _l = unsafe { Locked::new(gate(ext)) };
        unsafe {
            // The in-flight SRB stays with the worker (see module docs); its
            // transfer is cancelled below and it completes as BUS_RESET.
            while (*st).count > 0 {
                stolen[n] = (*st).ring[(*st).head];
                n += 1;
                (*st).head = ((*st).head + 1) % QUEUE_LEN;
                (*st).count -= 1;
            }
            (*st).gate.reset_pending = true;
            if !(*st).gate.inflight_irp.is_null() {
                // We still own the IRP until the worker withdraws it under
                // this lock, so it is alive here.
                let _ = wk::ntddk::IoCancelIrp((*st).gate.inflight_irp);
            }
        }
    }
    for &srb in &stolen[..n] {
        // SAFETY: each stolen SRB was queued, never seen by the worker, and is
        // now exclusively ours.
        unsafe {
            (*srb).SrbStatus = srb_status as u8;
            (*srb).DataTransferLength = 0;
            complete(ext, srb);
        }
    }
    // Wake the worker so recovery (or shutdown) happens even when idle.
    let _ = unsafe { wk::ntddk::KeSetEvent(core::ptr::addr_of_mut!((*st).wake), 0, 0) };
}

/// Hand an SRB back to StorPort.
///
/// # Safety
/// `srb` must be outstanding and owned by the caller.
unsafe fn complete(ext: *mut AdapterExtension, srb: sp::PSCSI_REQUEST_BLOCK) {
    // SAFETY: caller contract.
    unsafe {
        sp::StorPortNotification(
            sp::_SCSI_NOTIFICATION_TYPE::RequestComplete,
            ext.cast::<c_void>(),
            srb,
        );
    }
}

// ---- Worker thread (PASSIVE_LEVEL) -------------------------------------------

unsafe extern "C" fn worker_main(context: wk::PVOID) {
    let ext = context.cast::<AdapterExtension>();
    // SAFETY: `ext` outlives this thread (see `start_worker`).
    let st = unsafe { state(ext) };
    'outer: loop {
        // SAFETY: initialized auto-reset event.
        let _ = unsafe {
            wk::ntddk::KeWaitForSingleObject(
                core::ptr::addr_of_mut!((*st).wake).cast::<c_void>(),
                wk::_KWAIT_REASON::Executive,
                wk::_MODE::KernelMode as wk::KPROCESSOR_MODE,
                0,
                core::ptr::null_mut(),
            )
        };
        loop {
            if unsafe { (*st).stop.load(Ordering::Acquire) } {
                break 'outer;
            }
            // SAFETY: worker context.
            let (srb, reset) = unsafe { take_next(ext) };
            if reset {
                unsafe { reset_recovery(ext) };
            }
            if srb.is_null() {
                break;
            }
            if unsafe { (*st).cmds } == 0 {
                // First command: report how HwStorInitialize was called.
                let v = unsafe { (*st).init_irql.load(Ordering::Relaxed) };
                diag::set_u32(w!("InitIrqlPlus1"), v);
            }
            if unsafe { (*st).restarted.swap(false, Ordering::AcqRel) } {
                // SAFETY: worker context.
                unsafe { after_restart(ext) };
            }
            // SAFETY: `srb` is registered in flight and owned by us.
            unsafe { process(ext, srb) };
        }
    }
    // SAFETY: system thread terminating itself.
    let _ = unsafe { wk::ntddk::PsTerminateSystemThread(0) };
}

/// Pop the next SRB (registering it in flight) and consume a pending reset.
///
/// # Safety
/// Worker thread only.
unsafe fn take_next(ext: *mut AdapterExtension) -> (sp::PSCSI_REQUEST_BLOCK, bool) {
    let st = unsafe { state(ext) };
    let _l = unsafe { Locked::new(gate(ext)) };
    unsafe {
        let reset = core::mem::replace(&mut (*st).gate.reset_pending, false);
        if (*st).count == 0 {
            return (core::ptr::null_mut(), reset);
        }
        let srb = (*st).ring[(*st).head];
        (*st).head = ((*st).head + 1) % QUEUE_LEN;
        (*st).count -= 1;
        (*st).inflight = srb;
        (srb, reset)
    }
}

/// Complete `srb` unless a reset already stole (and completed) it.
///
/// # Safety
/// Worker thread only; `srb` was returned by `take_next`.
unsafe fn finish(ext: *mut AdapterExtension, srb: sp::PSCSI_REQUEST_BLOCK) {
    let st = unsafe { state(ext) };
    let mine = {
        let _l = unsafe { Locked::new(gate(ext)) };
        unsafe {
            let mine = (*st).inflight == srb;
            if mine {
                (*st).inflight = core::ptr::null_mut();
            }
            mine
        }
    };
    if mine {
        unsafe { complete(ext, srb) };
    }
}

/// Ask for reset recovery before the next transfer.
///
/// # Safety
/// `ext` must be the live adapter extension.
unsafe fn request_reset(ext: *mut AdapterExtension) {
    let _l = unsafe { Locked::new(gate(ext)) };
    unsafe { (*gate(ext)).reset_pending = true };
}

/// BOT reset recovery (BOT rev 1.0 §5.3.4): Mass Storage Reset, then clear
/// HALT on bulk IN and bulk OUT.
///
/// # Safety
/// Worker thread only.
unsafe fn reset_recovery(ext: *mut AdapterExtension) {
    let st = unsafe { state(ext) };
    unsafe {
        (*st).resets = (*st).resets.wrapping_add(1);
        diag::set_u32(D_RESETS, (*st).resets);
        let r = usb::mass_storage_reset(ext, CONTROL_TIMEOUT_MS);
        log_xfer(ext, r, Phase::Recovery);
        let r = usb::reset_pipe(ext, (*ext).bulk_in, CONTROL_TIMEOUT_MS);
        log_xfer(ext, r, Phase::RecoveryClearIn);
        let r = usb::reset_pipe(ext, (*ext).bulk_out, CONTROL_TIMEOUT_MS);
        log_xfer(ext, r, Phase::RecoveryClearOut);
    }
}

/// Append one transfer outcome to the transfer log (flushed with the
/// history). Keeps the full sequence, which a single "last" value cannot.
///
/// # Safety
/// Worker thread only.
unsafe fn log_xfer(ext: *mut AdapterExtension, x: usb::Xfer, phase: Phase) {
    let st = unsafe { state(ext) };
    let mut e = [0u8; 16];
    e[0] = phase as u8;
    e[1] = unsafe { (*st).cur_op };
    e[4..8].copy_from_slice(&x.nt.to_le_bytes());
    e[8..12].copy_from_slice(&x.usbd.to_le_bytes());
    e[12..16].copy_from_slice(&x.len.to_le_bytes());
    unsafe {
        let i = (*st).xfer_next as usize % XFER_LOG_LEN;
        (*st).xfer_log[i] = e;
        (*st).xfer_next = (*st).xfer_next.wrapping_add(1);
    }
}

fn note_xfer(x: usb::Xfer, phase: Phase) {
    #[allow(clippy::cast_sign_loss)]
    {
        diag::set_u32(D_LAST_NT, x.nt as u32);
        diag::set_u32(D_LAST_USBD, x.usbd as u32);
    }
    diag::set_u32(D_LAST_PHASE, phase as u32);
}

/// Classify a failed transfer; schedules reset recovery unless the device is
/// gone.
///
/// # Safety
/// Worker thread only.
unsafe fn transport_failure(ext: *mut AdapterExtension, x: usb::Xfer, phase: Phase) -> Fail {
    note_xfer(x, phase);
    if x.gone() {
        Fail::Gone
    } else {
        unsafe { request_reset(ext) };
        Fail::Reset
    }
}

/// Current interrupt time in 100 ns units.
fn now_100ns() -> u64 {
    let mut qpc = 0u64;
    // SAFETY: valid out-pointer; callable at any IRQL.
    unsafe { wk::ntddk::KeQueryInterruptTimePrecise(&raw mut qpc) }
}

/// Start the per-SRB budget: finish before StorPort's own timer
/// (`TimeOutValue` seconds, default 10) would fire, with 1.5 s to spare.
///
/// # Safety
/// Worker thread only.
unsafe fn arm_deadline(st: *mut IoState, srb_timeout_s: u32) {
    let s = if srb_timeout_s == 0 {
        10
    } else {
        srb_timeout_s
    };
    let ms = u64::from(s)
        .saturating_mul(1000)
        .saturating_sub(1500)
        .max(2_000);
    unsafe { (*st).deadline = now_100ns() + ms * 10_000 };
}

/// Timeout for the next transfer: `want` ms capped by what is left of the
/// SRB's budget, or `None` once the budget is spent.
///
/// # Safety
/// Worker thread only.
unsafe fn budget(st: *mut IoState, want: u32) -> Option<u32> {
    let left_ms = unsafe { (*st).deadline }.saturating_sub(now_100ns()) / 10_000;
    if left_ms < 100 {
        return None;
    }
    #[allow(clippy::cast_possible_truncation)]
    Some(want.min(left_ms.min(u64::from(u32::MAX)) as u32))
}

/// Run one command over BOT. `buf` is the (nonpaged) data buffer.
///
/// # Safety
/// Worker thread only; `buf` valid for `len` bytes (may be null if `len == 0`).
unsafe fn bot_execute(
    ext: *mut AdapterExtension,
    cdb: &[u8],
    dir: DataDirection,
    buf: *mut u8,
    len: u32,
) -> Result<Disposition, Fail> {
    let st = unsafe { state(ext) };
    let tag = unsafe { (*st).tags.next_tag() };
    let Ok(t) = BotTransaction::new(tag, 0, dir, len, cdb) else {
        return Err(Fail::Invalid);
    };
    let (bulk_in, bulk_out) = unsafe { ((*ext).bulk_in, (*ext).bulk_out) };
    let expected = if t.has_data_phase() { len } else { 0 };

    // Each step gets what is left of this SRB's budget; running out is a
    // timeout: recover the transport and report BUS_RESET (StorPort retries).
    macro_rules! within {
        ($want:expr, $phase:expr) => {
            match unsafe { budget(st, $want) } {
                Some(ms) => ms,
                None => {
                    diag::set_u32(D_LAST_PHASE, $phase as u32 | 0x100);
                    unsafe { request_reset(ext) };
                    return Err(Fail::Reset);
                }
            }
        };
    }

    // 1. CBW
    unsafe { (*st).cbw = t.cbw_bytes() };
    let cbw = unsafe { core::ptr::addr_of_mut!((*st).cbw).cast::<u8>() };
    #[allow(clippy::cast_possible_truncation)]
    let ms = within!(CBW_TIMEOUT_MS, Phase::Cbw);
    let x = unsafe { usb::bulk(ext, bulk_out, cbw, bot::CBW_LEN as u32, false, ms) };
    unsafe { log_xfer(ext, x, Phase::Cbw) };
    if !x.ok() || x.len as usize != bot::CBW_LEN {
        return Err(unsafe { transport_failure(ext, x, Phase::Cbw) });
    }

    // 2. Data. A stall here is normal BOT signalling: clear it, then read CSW.
    let mut moved = 0;
    if let Some((d, n)) = t.data_phase() {
        let (pipe, dir_in) = match d {
            bot::Direction::In => (bulk_in, true),
            bot::Direction::Out => (bulk_out, false),
        };
        let ms = within!(PHASE_TIMEOUT_MS, Phase::Data);
        let x = unsafe { usb::bulk(ext, pipe, buf, n, dir_in, ms) };
        unsafe { log_xfer(ext, x, Phase::Data) };
        if x.ok() {
            moved = x.len;
        } else if x.stalled() {
            moved = x.len;
            let ms = within!(CONTROL_TIMEOUT_MS, Phase::ClearData);
            let r = unsafe { usb::reset_pipe(ext, pipe, ms) };
            unsafe { log_xfer(ext, r, Phase::ClearData) };
            if !r.ok() {
                return Err(unsafe { transport_failure(ext, r, Phase::ClearData) });
            }
        } else {
            return Err(unsafe { transport_failure(ext, x, Phase::Data) });
        }

        // Some firmware answers a data-IN it cannot satisfy with the CSW
        // itself. Recognise our own CSW arriving in the data phase.
        #[allow(clippy::cast_possible_truncation)]
        if dir_in && moved as usize == bot::CSW_LEN {
            // SAFETY: `moved` bytes were just written into `buf`.
            let raw = unsafe { core::slice::from_raw_parts(buf, bot::CSW_LEN) };
            if matches!(bot::parse_csw(raw), Ok(c) if c.tag == tag) {
                return match bot_plan::disposition(raw, tag, expected, 0) {
                    Disposition::PhaseError => {
                        unsafe { request_reset(ext) };
                        Err(Fail::Reset)
                    }
                    d => Ok(d),
                };
            }
        }
    }

    // 3. CSW, with one retry after clearing a stall (BOT §6.7.2).
    let csw = unsafe { core::ptr::addr_of_mut!((*st).csw).cast::<u8>() };
    #[allow(clippy::cast_possible_truncation)]
    let csw_len = bot::CSW_LEN as u32;
    let ms = within!(PHASE_TIMEOUT_MS, Phase::Csw);
    let mut x = unsafe { usb::bulk(ext, bulk_in, csw, csw_len, true, ms) };
    unsafe { log_xfer(ext, x, Phase::Csw) };
    if x.stalled() {
        let ms = within!(CONTROL_TIMEOUT_MS, Phase::ClearCsw);
        let r = unsafe { usb::reset_pipe(ext, bulk_in, ms) };
        unsafe { log_xfer(ext, r, Phase::ClearCsw) };
        if !r.ok() {
            return Err(unsafe { transport_failure(ext, r, Phase::ClearCsw) });
        }
        let ms = within!(PHASE_TIMEOUT_MS, Phase::Csw);
        x = unsafe { usb::bulk(ext, bulk_in, csw, csw_len, true, ms) };
        unsafe { log_xfer(ext, x, Phase::Csw) };
    }
    if x.ok() && x.len == 0 {
        // Some firmware sends a stray zero-length packet before the CSW.
        let ms = within!(PHASE_TIMEOUT_MS, Phase::Csw);
        x = unsafe { usb::bulk(ext, bulk_in, csw, csw_len, true, ms) };
        unsafe { log_xfer(ext, x, Phase::Csw) };
    }
    if !x.ok() {
        return Err(unsafe { transport_failure(ext, x, Phase::Csw) });
    }
    // SAFETY: `x.len` (<= 13) bytes were written into the CSW buffer.
    let raw = unsafe { core::slice::from_raw_parts(csw, x.len as usize) };
    match bot_plan::disposition(raw, tag, expected, moved) {
        Disposition::PhaseError => {
            note_xfer(x, Phase::CswInvalid);
            unsafe { request_reset(ext) };
            Err(Fail::Reset)
        }
        d => Ok(d),
    }
}

/// Resolve the SRB data buffer to a system VA.
///
/// # Safety
/// Live extension and SRB.
unsafe fn data_va(ext: *mut AdapterExtension, srb: sp::PSCSI_REQUEST_BLOCK) -> *mut u8 {
    // `StorPortGetSystemAddress` is authoritative (a kernel-range DataBuffer
    // can still be the unmapped original VA of a pageable buffer).
    if let Some(p) = unsafe { spx::get_system_address(ext.cast::<c_void>(), srb) } {
        return p.cast::<u8>();
    }
    let st = unsafe { state(ext) };
    unsafe {
        (*st).va_fallback = (*st).va_fallback.wrapping_add(1);
        diag::set_u32(D_VA_FALLBACK, (*st).va_fallback);
    }
    let va = unsafe { (*srb).DataBuffer };
    if va as usize >= KERNEL_VA_START {
        va.cast::<u8>()
    } else {
        core::ptr::null_mut()
    }
}

/// Fill in sense data and CHECK CONDITION status.
///
/// # Safety
/// Live SRB.
unsafe fn set_check_condition(srb: sp::PSCSI_REQUEST_BLOCK, sense: &[u8]) {
    unsafe {
        (*srb).ScsiStatus = sp::SCSISTAT_CHECK_CONDITION as u8;
        let dst = (*srb).SenseInfoBuffer.cast::<u8>();
        let cap = usize::from((*srb).SenseInfoBufferLength);
        if (*srb).SrbFlags & sp::SRB_FLAGS_DISABLE_AUTOSENSE != 0
            || dst.is_null()
            || cap == 0
            || sense.len() < 8
        {
            (*srb).SrbStatus = sp::SRB_STATUS_ERROR as u8;
            return;
        }
        let n = sense.len().min(cap);
        core::ptr::copy_nonoverlapping(sense.as_ptr(), dst, n);
        #[allow(clippy::cast_possible_truncation)]
        {
            (*srb).SenseInfoBufferLength = n as u8;
        }
        (*srb).SrbStatus = (sp::SRB_STATUS_ERROR | sp::SRB_STATUS_AUTOSENSE_VALID) as u8;
    }
}

/// Execute one SRB end to end and complete it.
///
/// # Safety
/// Worker thread only; `srb` registered in flight.
#[allow(clippy::too_many_lines)]
unsafe fn process(ext: *mut AdapterExtension, srb: sp::PSCSI_REQUEST_BLOCK) {
    let st = unsafe { state(ext) };

    let mut cdb_buf = [0u8; 16];
    let cdb_len = usize::from(unsafe { (*srb).CdbLength }).min(16);
    let src = unsafe { (*srb).Cdb };
    cdb_buf[..cdb_len].copy_from_slice(&src[..cdb_len]);
    let cdb = &cdb_buf[..cdb_len];
    unsafe { (*st).cur_op = cdb_buf[0] };

    unsafe { arm_deadline(st, (*srb).TimeOutValue) };
    let flags = unsafe { (*srb).SrbFlags };
    let mut len = unsafe { (*srb).DataTransferLength };
    let dir = if len == 0 {
        DataDirection::None
    } else if flags & sp::SRB_FLAGS_DATA_IN != 0 {
        DataDirection::In
    } else if flags & sp::SRB_FLAGS_DATA_OUT != 0 {
        DataDirection::Out
    } else {
        len = 0;
        DataDirection::None
    };
    let data = if len == 0 {
        core::ptr::null_mut()
    } else {
        unsafe { data_va(ext, srb) }
    };

    unsafe {
        (*srb).ScsiStatus = sp::SCSISTAT_GOOD as u8;
        (*srb).DataTransferLength = len;
    }

    let mut rec = Record::new(cdb, len);

    let max_len = if unsafe { (*st).rtsx } {
        MAX_XFER
    } else {
        BOUNCE_LEN
    };
    if cdb.is_empty() || (len != 0 && data.is_null()) || len as usize > max_len {
        unsafe {
            (*srb).SrbStatus = sp::SRB_STATUS_INVALID_REQUEST as u8;
            (*srb).DataTransferLength = 0;
        }
        rec.fail = 0xE1;
        unsafe { trace(st, &rec, srb, true) };
        unsafe { finish(ext, srb) };
        return;
    }

    if unsafe { (*st).rtsx } {
        // Realtek controller without SCSI firmware: we are the SCSI target.
        // SAFETY: as for this function; `data` validated above.
        unsafe { process_rtsx(ext, srb, cdb, dir, len, data, &mut rec) };
        let failed = unsafe { (*srb).SrbStatus } & 0x3F != sp::SRB_STATUS_SUCCESS as u8
            && unsafe { (*srb).SrbStatus } & 0x3F != sp::SRB_STATUS_DATA_OVERRUN as u8;
        unsafe { trace(st, &rec, srb, failed) };
        unsafe { finish(ext, srb) };
        return;
    }

    let read_only_block = unsafe { !(*st).allow_writes } && scsi_emu::is_write_command(cdb[0]);
    let route = if read_only_block {
        None
    } else {
        Some(scsi_emu::route(cdb))
    };

    match route {
        None => {
            let sense = scsi_emu::fixed_sense(
                scsi_emu::SENSE_KEY_DATA_PROTECT,
                scsi_emu::ASC_WRITE_PROTECTED,
                0,
            );
            unsafe {
                (*srb).DataTransferLength = 0;
                set_check_condition(srb, &sense);
            }
            rec.sense(&sense);
            rec.fail = 0xE2; // refused: read-only mode
        }
        Some(route @ (scsi_emu::Route::ReportLuns | scsi_emu::Route::VpdSupportedPages)) => {
            // SAFETY: `data` is valid for `len` bytes (checked above); these
            // are data-in commands.
            let out = if data.is_null() {
                &mut [][..]
            } else {
                unsafe { core::slice::from_raw_parts_mut(data, len as usize) }
            };
            let alloc = scsi_emu::allocation_len(cdb);
            let n = if matches!(route, scsi_emu::Route::ReportLuns) {
                scsi_emu::report_luns(out, alloc)
            } else {
                scsi_emu::vpd_supported_pages(out, alloc, scsi::PDT_DIRECT_ACCESS)
            };
            #[allow(clippy::cast_possible_truncation)]
            let n = n as u32;
            unsafe { finish_data(srb, n, len) };
            rec.xfer = n;
            rec.fail = 0xE0; // answered locally
        }
        Some(scsi_emu::Route::Reject) => {
            let sense = scsi_emu::fixed_sense(
                scsi_emu::SENSE_KEY_ILLEGAL_REQUEST,
                scsi_emu::ASC_INVALID_FIELD_IN_CDB,
                0,
            );
            unsafe {
                (*srb).DataTransferLength = 0;
                set_check_condition(srb, &sense);
            }
            rec.sense(&sense);
            rec.fail = 0xE0;
        }
        Some(scsi_emu::Route::PassThrough) => {
            let bounce = unsafe { core::ptr::addr_of_mut!((*st).bounce).cast::<u8>() };
            if dir == DataDirection::Out {
                // SAFETY: both valid for `len` (<= BOUNCE_LEN) bytes.
                unsafe { core::ptr::copy_nonoverlapping(data, bounce, len as usize) };
            }
            match unsafe { bot_execute(ext, cdb, dir, bounce, len) } {
                Ok(Disposition::Good) => {
                    unsafe { deliver(st, cdb, dir, data, bounce, len) };
                    unsafe { (*srb).SrbStatus = sp::SRB_STATUS_SUCCESS as u8 };
                    rec.xfer = len;
                }
                Ok(Disposition::Underrun { transferred }) => {
                    unsafe { deliver(st, cdb, dir, data, bounce, transferred) };
                    unsafe { finish_data(srb, transferred, len) };
                    rec.xfer = transferred;
                }
                Ok(Disposition::CheckCondition { transferred }) => {
                    if dir == DataDirection::In && transferred > 0 {
                        // SAFETY: `transferred` <= `len` bytes arrived in bounce.
                        unsafe {
                            core::ptr::copy_nonoverlapping(bounce, data, transferred as usize)
                        };
                    }
                    unsafe { (*srb).DataTransferLength = transferred };
                    rec.xfer = transferred;
                    unsafe { autosense(ext, srb, &mut rec) };
                }
                Ok(Disposition::PhaseError) | Err(Fail::Reset) => unsafe {
                    (*srb).SrbStatus = sp::SRB_STATUS_BUS_RESET as u8;
                    (*srb).DataTransferLength = 0;
                    rec.fail = 0xF1;
                },
                Err(Fail::Gone) => unsafe {
                    (*srb).SrbStatus = sp::SRB_STATUS_NO_DEVICE as u8;
                    (*srb).DataTransferLength = 0;
                    rec.fail = 0xF2;
                },
                Err(Fail::Invalid) => unsafe {
                    (*srb).SrbStatus = sp::SRB_STATUS_INVALID_REQUEST as u8;
                    (*srb).DataTransferLength = 0;
                    rec.fail = 0xF3;
                },
            }
        }
    }

    let failed = unsafe { (*srb).SrbStatus } & 0x3F != sp::SRB_STATUS_SUCCESS as u8
        && unsafe { (*srb).SrbStatus } & 0x3F != sp::SRB_STATUS_DATA_OVERRUN as u8;
    unsafe { trace(st, &rec, srb, failed) };
    unsafe { finish(ext, srb) };
}

/// Outcome of re-initialising a card that stopped answering.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Recovery {
    /// The same card (same CID) answered again: re-attached transparently.
    Same,
    /// A different card answered: report a media change.
    Different,
    /// Nothing answers: the slot is empty.
    Gone,
}

/// Bus modes to try, from `Parameters\Uhs` and `Parameters\HighSpeed`
/// (both default on; 0 disables).
fn modes() -> sdhost::Modes {
    sdhost::Modes {
        uhs: diag::get_u32(w!("Uhs"), 1) != 0,
        high_speed: diag::get_u32(w!("HighSpeed"), 1) != 0,
    }
}

/// Record a card that `sdhost::bring_up_card` initialised.
///
/// # Safety
/// Worker thread only.
unsafe fn adopt_card(
    ext: *mut AdapterExtension,
    info: sora_core::sd::CardInfo,
    speed: sdhost::Speed,
    now: u64,
) {
    let st = unsafe { state(ext) };
    unsafe {
        (*st).card = info;
        (*st).speed = speed;
        (*st).card_wp = info.write_protected;
        (*st).media.ready = true;
        (*st).media.blocks = info.blocks;
        (*st).media.block_len = 512;
        (*st).next_check_ms = now + CHECK_MS;
    }
}

/// The ready card stopped answering CMD13: power-cycle the slot and
/// initialise again. Covers a lost card state (resume from sleep, a glitch),
/// a swapped card, and a removal.
///
/// # Safety
/// Worker thread only.
unsafe fn recover_card(ext: *mut AdapterExtension, now: u64) -> Recovery {
    let st = unsafe { state(ext) };
    let old = unsafe { (*st).card };
    // `bring_up_card` starts with a full power cycle (card reset).
    unsafe { (*st).powered = true };
    match unsafe { sdhost::bring_up_card(ext, modes()) } {
        Ok((info, speed)) => {
            let same = info.cid == old.cid;
            unsafe { adopt_card(ext, info, speed, now) };
            if same {
                Recovery::Same
            } else {
                Recovery::Different
            }
        }
        Err(_) => unsafe {
            sdhost::power_off(ext);
            (*st).powered = false;
            (*st).media.ready = false;
            (*st).card_wp = false;
            Recovery::Gone
        },
    }
}

fn bump(name: &[u16]) {
    diag::set_u32(name, diag::get_u32(name, 0).wrapping_add(1));
}

/// One card-detect step: with a ready card, a CMD13 liveness check (at most
/// every `CHECK_MS` unless `force`), with recovery if it fails; with no card,
/// an init attempt (at most every `RETRY_MS` unless `force`). Returns the
/// recovery outcome when a ready card had to be re-initialised.
///
/// # Safety
/// Worker thread only.
unsafe fn detect_step(ext: *mut AdapterExtension, force: bool) -> Option<Recovery> {
    let st = unsafe { state(ext) };
    let now = sdhost::uptime_ms();
    let mut outcome = None;
    unsafe {
        if (*st).ejected {
            // Ejected card still in the slot: wait for it to go away. A card
            // can only be noticed by talking to it, so probe (cheaply) once
            // per RETRY_MS; once nothing answers, normal detection resumes.
            if force || now >= (*st).next_init_ms {
                (*st).next_init_ms = now + RETRY_MS;
                if !sdhost::probe(ext) {
                    (*st).ejected = false;
                    (*st).next_init_ms = now; // re-arm at once for the next card
                }
                (*st).powered = false;
            }
        } else if (*st).media.ready {
            if force || now >= (*st).next_check_ms {
                (*st).next_check_ms = now + CHECK_MS;
                let card = (*st).card;
                if !sdhost::card_alive(ext, &card) {
                    let r = recover_card(ext, now);
                    match r {
                        Recovery::Same => bump(w!("Recoveries")),
                        Recovery::Different => {
                            // Report it as remove + insert: NOT READY is
                            // skipped, but the next command gets UNIT
                            // ATTENTION / MEDIUM MAY HAVE CHANGED.
                            let _ = (*st).target.observe(false);
                            bump(w!("Removals"));
                            bump(w!("Inserts"));
                        }
                        Recovery::Gone => {
                            (*st).next_init_ms = now + RETRY_MS;
                            bump(w!("Removals"));
                        }
                    }
                    outcome = Some(r);
                }
            }
        } else if force || now >= (*st).next_init_ms {
            (*st).next_init_ms = now + RETRY_MS;
            (*st).powered = true;
            let t0 = sdhost::uptime_ms();
            let r = sdhost::bring_up_card(ext, modes());
            if r.is_ok() || diag::verbose() {
                #[allow(clippy::cast_possible_truncation)]
                diag::set_u32(w!("LastInitMs"), (sdhost::uptime_ms() - t0) as u32);
            }
            match r {
                Ok((info, speed)) => {
                    adopt_card(ext, info, speed, now);
                    (*st).empty_since_ms = 0;
                    bump(w!("Inserts"));
                }
                Err(_) => {
                    sdhost::power_off(ext);
                    (*st).powered = false;
                    if (*st).empty_since_ms == 0 {
                        (*st).empty_since_ms = now.max(1);
                    } else if now - (*st).empty_since_ms > IDLE_AFTER_MS {
                        (*st).next_init_ms = now + SLOW_RETRY_MS;
                    }
                }
            }
        }
        // A card counts as present only while it answers.
        (*st).media.present = (*st).media.ready;
        if (*st).target.observe((*st).media.ready) {
            diag::set_u32(w!("CardPresent"), u32::from((*st).media.ready));
        }
    }
    outcome
}

/// START STOP UNIT with LoEj: `load = false` ejects (Windows "Eject": the
/// card is powered down and reported absent until it is pulled and a card is
/// inserted again); `load = true` cancels an eject and re-detects.
///
/// # Safety
/// Worker thread only.
unsafe fn start_stop(ext: *mut AdapterExtension, load: bool) {
    let st = unsafe { state(ext) };
    unsafe {
        if load {
            (*st).ejected = false;
            (*st).next_init_ms = 0;
            return;
        }
        if (*st).media.ready {
            sdhost::power_off(ext);
            (*st).powered = false;
            (*st).media.ready = false;
            (*st).card_wp = false;
        }
        (*st).ejected = true;
        (*st).next_init_ms = sdhost::uptime_ms() + RETRY_MS;
        (*st).media.present = false;
        if (*st).target.observe(false) {
            diag::set_u32(w!("CardPresent"), 0);
        }
    }
    bump(w!("Ejects"));
}

/// After the adapter restarts (resume from sleep), the reader may have lost
/// power: re-run controller init and re-check the card before the next
/// command. A card that kept its contents is re-attached transparently.
///
/// # Safety
/// Worker thread only.
unsafe fn after_restart(ext: *mut AdapterExtension) {
    let st = unsafe { state(ext) };
    bump(w!("Restarts"));
    if !unsafe { (*st).rtsx } {
        return;
    }
    let _ = unsafe { chip::init(ext) };
    unsafe {
        (*st).powered = (*st).media.ready;
        (*st).last_raw = 0;
        let _ = detect_step(ext, true);
    }
}

/// Power the SD slot down when the adapter stops (after the worker has
/// stopped; it uses the worker URB), so a card is never left powered,
/// possibly at 1.8 V signaling, while no driver manages it.
///
/// # Safety
/// PASSIVE_LEVEL; worker stopped; USB still open.
pub unsafe fn power_down_slot(ext: *mut AdapterExtension) {
    let st = unsafe { state(ext) };
    unsafe {
        if (*st).rtsx && !(*ext).urb.is_null() {
            sdhost::power_off(ext);
            (*st).powered = false;
            (*st).media.ready = false;
        }
    }
}

/// Note an adapter restart (`ScsiRestartAdapter`); handled by the worker
/// before the next command. Any IRQL.
///
/// # Safety
/// `ext` must be the live adapter extension.
pub unsafe fn note_restart(ext: *mut AdapterExtension) {
    unsafe { (*state(ext)).restarted.store(true, Ordering::Release) };
}

/// Execute one SRB with the Realtek backend: run one card-detect step (for
/// non-sector commands), let `scsi_target` decide, and move any data through
/// the SD host (`sdhost::transfer`).
///
/// # Safety
/// Worker thread only; `srb` in flight; `data` valid for `len` bytes.
unsafe fn process_rtsx(
    ext: *mut AdapterExtension,
    srb: sp::PSCSI_REQUEST_BLOCK,
    cdb: &[u8],
    dir: DataDirection,
    len: u32,
    data: *mut u8,
    rec: &mut Record,
) {
    let st = unsafe { state(ext) };
    let op = cdb[0];
    let sector_io = matches!(op, 0x28 | 0x2A | 0xA8 | 0xAA | 0x88 | 0x8A);

    // Card detect. The storage stack's TEST UNIT READY polling (several per
    // second) drives this; sector I/O skips it to stay fast.
    //
    // The controller's card-detect bit cannot be trusted: on the RTS5129 in
    // the IdeaPad it reads "present" with the slot empty. What does change on
    // insert/remove is the rest of the status word (the write-protect contact
    // reads "protected" with no card). So presence is decided by whether the
    // card answers SD commands, and any status change re-checks immediately:
    //   * card ready  -> CMD13 liveness check, at most every CHECK_MS
    //   * no card     -> full init attempt, at most every RETRY_MS
    if op == scsi::OP_START_STOP_UNIT && cdb.len() > 4 && cdb[4] & 0x02 != 0 {
        // LoEj set: Start = 0 ejects, Start = 1 loads.
        unsafe { start_stop(ext, cdb[4] & 0x01 != 0) };
    }
    if !sector_io {
        let (x, raw) = unsafe { chip::poll_raw(ext) };
        unsafe { trace_poll(st, op, x, raw) };
        let polled = match sora_core::rtsx::parse_status(&raw[..x.len.min(2) as usize]) {
            Some(s) if x.ok() => Ok(s),
            _ => Err(chip::ChipError(x)),
        };
        match polled {
            Ok(s) => unsafe {
                let sig = u32::from_le_bytes([raw[0], raw[1], 0, 0x80]);
                let changed = sig != (*st).last_raw;
                (*st).last_raw = sig;
                let _ = detect_step(ext, changed);
                (*st).card_wp |= s.write_protect && (*st).media.ready;
            },
            Err(chip::ChipError(x)) => note_xfer(x, Phase::Cbw),
        }
    }
    unsafe { (*st).media.write_protected = !(*st).allow_writes || (*st).card_wp };

    let bounce = unsafe { core::ptr::addr_of_mut!((*st).bounce).cast::<u8>() };
    // Non-sector replies go through the bounce buffer (they are all small);
    // sector data never touches it.
    let in_len = if dir == DataDirection::In {
        (len as usize).min(BOUNCE_LEN)
    } else {
        0
    };
    // SAFETY: bounce holds BOUNCE_LEN >= in_len bytes.
    let out = unsafe { core::slice::from_raw_parts_mut(bounce, in_len) };
    let media = unsafe { (*st).media };
    let reply = unsafe { (*st).target.handle(cdb, &media, out) };

    match reply {
        Reply::Data(n) => {
            if n > 0 {
                // SAFETY: n <= in_len <= len bytes; `data` valid for len.
                unsafe { core::ptr::copy_nonoverlapping(bounce, data, n) };
            }
            #[allow(clippy::cast_possible_truncation)]
            let n = n as u32;
            unsafe { finish_data(srb, n, len) };
            rec.xfer = n;
        }
        Reply::Good => unsafe {
            (*srb).DataTransferLength = 0;
            (*srb).SrbStatus = sp::SRB_STATUS_SUCCESS as u8;
        },
        Reply::Check(k, a, q) => {
            let sense = scsi_emu::fixed_sense(k, a, q);
            unsafe {
                (*srb).DataTransferLength = 0;
                set_check_condition(srb, &sense);
            }
            rec.sense(&sense);
        }
        Reply::Io { write, lba, blocks } => {
            let bytes = u64::from(blocks) * 512;
            // MaximumTransferLength keeps requests within MAX_XFER; the SRB
            // length must match the CDB's block count.
            if bytes > MAX_XFER as u64 || bytes != u64::from(len) {
                unsafe {
                    (*srb).SrbStatus = sp::SRB_STATUS_INVALID_REQUEST as u8;
                    (*srb).DataTransferLength = 0;
                }
                rec.fail = 0xE4;
                return;
            }
            // In place: `data` is the SRB buffer's system mapping (locked,
            // resident), so the USB stack transfers straight into it.
            #[allow(clippy::cast_possible_truncation)] // <= MAX_XFER / 512 blocks
            let blocks = blocks as u16;
            let card = unsafe { (*st).card };
            let mut r = unsafe { sdhost::transfer(ext, &card, write, lba, blocks, data) };
            if r.is_err() {
                // One retry; first drop to default speed (from High Speed or
                // SDR50), the usual culprit for CRC errors with marginal
                // cards or contacts.
                unsafe {
                    if (*st).speed != sdhost::Speed::Default
                        && sdhost::set_speed(ext, sdhost::Speed::Default).is_ok()
                    {
                        (*st).speed = sdhost::Speed::Default;
                        (*st).speed_fallbacks = (*st).speed_fallbacks.wrapping_add(1);
                        diag::set_u32(w!("SpeedFallbacks"), (*st).speed_fallbacks);
                    }
                    r = sdhost::transfer(ext, &card, write, lba, blocks, data);
                    // Still failing: the card may have lost its state (or be
                    // gone). Re-attach it if it is the same card, and retry.
                    if r.is_err() && detect_step(ext, true) == Some(Recovery::Same) {
                        let card = (*st).card;
                        r = sdhost::transfer(ext, &card, write, lba, blocks, data);
                    }
                }
            }
            match r {
                Ok(()) => {
                    unsafe {
                        (*srb).DataTransferLength = len;
                        (*srb).SrbStatus = sp::SRB_STATUS_SUCCESS as u8;
                    }
                    rec.xfer = len;
                }
                Err(_) => {
                    // Medium error: unrecovered read error / write error.
                    let sense = if write {
                        scsi_emu::fixed_sense(0x03, 0x0C, 0x00)
                    } else {
                        scsi_emu::fixed_sense(0x03, 0x11, 0x00)
                    };
                    unsafe {
                        (*srb).DataTransferLength = 0;
                        set_check_condition(srb, &sense);
                    }
                    rec.sense(&sense);
                    rec.fail = 0xE5;
                    // Maybe the card was pulled: re-check on the next poll.
                    unsafe { (*st).next_check_ms = 0 };
                }
            }
        }
    }
}

/// Success with `n` of `len` bytes: full, or an underrun.
///
/// # Safety
/// Live SRB.
unsafe fn finish_data(srb: sp::PSCSI_REQUEST_BLOCK, n: u32, len: u32) {
    unsafe {
        (*srb).DataTransferLength = n;
        (*srb).SrbStatus = if n < len {
            sp::SRB_STATUS_DATA_OVERRUN as u8
        } else {
            sp::SRB_STATUS_SUCCESS as u8
        };
    }
}

/// Copy data-in from the bounce buffer to the SRB, applying the RMB override
/// to standard INQUIRY data and recording identity data for diagnostics.
///
/// # Safety
/// Worker thread only; `data`/`bounce` valid for `n` bytes when data-in.
unsafe fn deliver(
    st: *mut IoState,
    cdb: &[u8],
    dir: DataDirection,
    data: *mut u8,
    bounce: *mut u8,
    n: u32,
) {
    if dir != DataDirection::In || n == 0 {
        return;
    }
    // SAFETY: caller contract.
    let got = unsafe { core::slice::from_raw_parts_mut(bounce, n as usize) };
    if unsafe { !(*st).allow_writes } {
        scsi_emu::set_mode_sense_wp(cdb[0], got);
    }
    match cdb[0] {
        scsi::OP_INQUIRY => {
            scsi_emu::set_removable(got);
            if unsafe { !(*st).have_inquiry } {
                unsafe { (*st).have_inquiry = true };
                diag::set_bin(D_INQUIRY, &got[..got.len().min(36)]);
            }
        }
        scsi::OP_READ_CAPACITY_10 => diag::set_bin(D_CAPACITY, &got[..got.len().min(8)]),
        _ => {}
    }
    unsafe { core::ptr::copy_nonoverlapping(bounce, data, n as usize) };
}

/// After CSW "failed": fetch sense with REQUEST SENSE and attach it.
///
/// # Safety
/// Worker thread only; `srb` in flight.
unsafe fn autosense(ext: *mut AdapterExtension, srb: sp::PSCSI_REQUEST_BLOCK, rec: &mut Record) {
    let st = unsafe { state(ext) };
    #[allow(clippy::cast_possible_truncation)]
    let (cdb, n) = scsi::request_sense(scsi_emu::SENSE_LEN as u8);
    let sense = unsafe { core::ptr::addr_of_mut!((*st).sense).cast::<u8>() };
    #[allow(clippy::cast_possible_truncation)]
    let want = scsi_emu::SENSE_LEN as u32;
    let got =
        match unsafe { bot_execute(ext, &cdb[..usize::from(n)], DataDirection::In, sense, want) } {
            Ok(Disposition::Good) => want,
            Ok(Disposition::Underrun { transferred }) => transferred,
            _ => 0,
        };
    // SAFETY: `got` <= SENSE_LEN bytes were written.
    let data = unsafe { core::slice::from_raw_parts(sense, got as usize) };
    rec.sense(data);
    unsafe { set_check_condition(srb, data) };
    if got == 0 {
        rec.fail = 0xF4; // sense unavailable
    }
}

// ---- Diagnostics -------------------------------------------------------------

/// Card-detect poll trace. `PollCount` / `PollLast` (`[raw0, raw1, op, ok]`)
/// are refreshed on every poll; `PollLog` keeps the last 32 *changes* of
/// `[raw0, raw1, op, ok]` as 8-byte entries `[that, seconds since boot:u32le]`.
///
/// # Safety
/// Worker thread only.
unsafe fn trace_poll(st: *mut IoState, op: u8, x: usb::Xfer, raw: [u8; 2]) {
    let ok = u8::from(x.ok()) | if x.len >= 2 { 0x10 } else { 0 };
    let key = [raw[0], raw[1], op, ok];
    unsafe {
        (*st).poll_count = (*st).poll_count.wrapping_add(1);
        if diag::verbose() {
            diag::set_u32(w!("PollCount"), (*st).poll_count);
            diag::set_bin(w!("PollLast"), &key);
        }
        // Ignore `op` when deciding what counts as a change.
        let sig = u32::from_le_bytes([raw[0], raw[1], 0, ok]) | 0x8000_0000;
        if sig != (*st).poll_sig {
            (*st).poll_sig = sig;
            #[allow(clippy::cast_possible_truncation)]
            let secs = (now_100ns() / 10_000_000) as u32;
            let i = ((*st).poll_next as usize) % POLL_LOG_LEN;
            let e = &mut (*st).poll_log[i];
            e[..4].copy_from_slice(&key);
            e[4..].copy_from_slice(&secs.to_le_bytes());
            (*st).poll_next = (*st).poll_next.wrapping_add(1);
            let blob = core::slice::from_raw_parts(
                core::ptr::addr_of!((*st).poll_log).cast::<u8>(),
                POLL_LOG_LEN * 8,
            );
            diag::set_bin(w!("PollLog"), blob);
            diag::set_u32(w!("PollNext"), (*st).poll_next);
        }
    }
}

/// One 16-byte `History` entry:
/// `[op, cdb1, srb_status, scsi_status, key, asc, ascq, fail, len:u32le, xfer:u32le]`.
struct Record {
    bytes: [u8; 16],
    xfer: u32,
    fail: u8,
}

impl Record {
    fn new(cdb: &[u8], len: u32) -> Self {
        let mut bytes = [0u8; 16];
        bytes[0] = cdb.first().copied().unwrap_or(0xFF);
        bytes[1] = cdb.get(1).copied().unwrap_or(0);
        bytes[8..12].copy_from_slice(&len.to_le_bytes());
        Self {
            bytes,
            xfer: 0,
            fail: 0,
        }
    }

    fn sense(&mut self, s: &[u8]) {
        if let Some(parsed) = scsi::parse_sense(s) {
            self.bytes[4] = parsed.sense_key;
            self.bytes[5] = parsed.asc;
            self.bytes[6] = parsed.ascq;
        }
    }
}

/// Append to the history ring and flush to the registry per the trace policy.
///
/// # Safety
/// Worker thread only.
unsafe fn trace(st: *mut IoState, rec: &Record, srb: sp::PSCSI_REQUEST_BLOCK, failed: bool) {
    let mut e = rec.bytes;
    unsafe {
        e[2] = (*srb).SrbStatus;
        e[3] = (*srb).ScsiStatus;
    }
    e[7] = rec.fail;
    e[12..16].copy_from_slice(&rec.xfer.to_le_bytes());

    unsafe {
        let i = ((*st).cmds as usize) % HISTORY_LEN;
        (*st).history[i] = e;
        (*st).cmds = (*st).cmds.wrapping_add(1);
        if failed {
            (*st).errors = (*st).errors.wrapping_add(1);
        }
        let cmds = (*st).cmds;
        // Errors always flush at first, but "no media" TUR polling fails once a
        // second forever, so stop flushing on errors after a while.
        let verbose = diag::verbose();
        let flush_error = failed && (*st).errors <= if verbose { 1024 } else { 64 };
        if flush_error || (verbose && cmds <= TRACE_ALL_FIRST) || cmds.is_multiple_of(256) {
            diag::set_u32(D_CMDS, cmds);
            diag::set_u32(D_ERRS, (*st).errors);
            sdhost::flush_perf();
            #[allow(clippy::cast_possible_truncation)]
            diag::set_u32(D_HIST_NEXT, (cmds as usize % HISTORY_LEN) as u32);
            // SAFETY: `history` is a contiguous POD array.
            let blob = core::slice::from_raw_parts(
                core::ptr::addr_of!((*st).history).cast::<u8>(),
                HISTORY_LEN * 16,
            );
            diag::set_bin(D_HISTORY, blob);
            let xl = core::slice::from_raw_parts(
                core::ptr::addr_of!((*st).xfer_log).cast::<u8>(),
                XFER_LOG_LEN * 16,
            );
            diag::set_bin(w!("XferLog"), xl);
            diag::set_u32(w!("XferNext"), (*st).xfer_next);
        }
    }
}
