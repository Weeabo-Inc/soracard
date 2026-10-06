// Copyright (c) SoraCard
// SPDX-License-Identifier: MIT OR Apache-2.0

//! The data path: intercept `IRP_MJ_SCSI` on each child PDO and translate the
//! SRB into a Bulk-Only Transport transaction on the parent's bulk pipes.
//!
//! The reader's firmware implements the SCSI command set itself, so this is a
//! *passthrough*: whatever Cdb the class driver sends is framed into a CBW,
//! executed, and the CSW status is mapped back to the SRB. `sora-core` provides
//! the CBW/CSW framing so the wire format is unit-tested off-target.

use crate::srb::{
    srb_from_irp, ScsiRequestBlock, SCSISTAT_GOOD, SRB_FLAGS_DATA_IN, SRB_FLAGS_DATA_OUT,
    SRB_FUNCTION_CLAIM_DEVICE, SRB_FUNCTION_EXECUTE_SCSI, SRB_FUNCTION_RELEASE_DEVICE,
    SRB_STATUS_ERROR, SRB_STATUS_INVALID_REQUEST, SRB_STATUS_SUCCESS,
};
use crate::{diag, DeviceContext, DRIVER_TAG};
use core::sync::atomic::{AtomicU32, Ordering};
use sora_core::bot::{parse_csw, Cbw, CswStatus, Direction};
use wdk::{nt_success, println};
use wdk_sys::{
    call_unsafe_wdf_function_binding, NTSTATUS, PIRP, PVOID, ULONG, WDFDEVICE, WDFIOTARGET,
    WDFOBJECT, WDFREQUEST, WDFUSBPIPE, WDF_MEMORY_DESCRIPTOR, WDF_NO_HANDLE,
    WDF_NO_OBJECT_ATTRIBUTES, WDF_REQUEST_SEND_OPTIONS,
};

const WDF_REQUEST_SEND_OPTION_SYNCHRONOUS: u32 = 0x2;
const WDF_REQUEST_SEND_OPTION_TIMEOUT: u32 = 0x1;
const WDF_MEMORY_DESCRIPTOR_TYPE_BUFFER: i32 = 1;

static CBW_TAG: AtomicU32 = AtomicU32::new(1);

/// Register our `IRP_MJ_SCSI` handler on the child PDO. Returns the NTSTATUS.
pub fn assign_irp_preprocess(child_init: *mut wdk_sys::WDFDEVICE_INIT) -> i32 {
    unsafe {
        call_unsafe_wdf_function_binding!(
            WdfDeviceInitAssignWdmIrpPreprocessCallback,
            child_init,
            Some(evt_irp_scsi as unsafe extern "C" fn(WDFDEVICE, PIRP) -> NTSTATUS),
            wdk_sys::IRP_MJ_SCSI as u8,
            core::ptr::null_mut(),
            0u32,
        )
    }
}

/// Get the parent device's bulk pipes (the child PDO has no pipes of its own).
unsafe fn parent_pipes(child: WDFDEVICE) -> Option<(WDFUSBPIPE, WDFUSBPIPE)> {
    let parent = unsafe { call_unsafe_wdf_function_binding!(WdfPdoGetParent, child) };
    let ctx: *mut DeviceContext =
        unsafe { crate::wdf_object_get_device_context(parent as WDFOBJECT) };
    if ctx.is_null() {
        return None;
    }
    let c = unsafe { &*ctx };
    if c.bulk_in.is_null() || c.bulk_out.is_null() {
        return None;
    }
    Some((c.bulk_in, c.bulk_out))
}

/// Register a pass-through `IRP_MJ_PNP` preprocess so we can log the minor
/// function (start/caps/query-id/…) the storage stack sends the child PDO.
pub fn assign_pnp_preprocess(child_init: *mut wdk_sys::WDFDEVICE_INIT) -> i32 {
    unsafe {
        call_unsafe_wdf_function_binding!(
            WdfDeviceInitAssignWdmIrpPreprocessCallback,
            child_init,
            Some(evt_irp_pnp as unsafe extern "C" fn(WDFDEVICE, PIRP) -> NTSTATUS),
            wdk_sys::IRP_MJ_PNP as u8,
            core::ptr::null_mut(),
            0u32,
        )
    }
}

/// Log the PnP minor function, then let KMDF handle it.
unsafe extern "C" fn evt_irp_pnp(device: WDFDEVICE, irp: PIRP) -> NTSTATUS {
    let (_, minor) = unsafe { crate::srb::major_minor_function(irp) };
    let driver = unsafe { call_unsafe_wdf_function_binding!(WdfDeviceGetDriver, device) };
    diag::set(driver, &diag::N_PNP_MINOR, u32::from(minor));
    unsafe { call_unsafe_wdf_function_binding!(WdfDeviceWdmDispatchPreprocessedIrp, device, irp) }
}

/// One synchronous bulk transfer. `write = false` reads into `buf`.
unsafe fn bulk(pipe: WDFUSBPIPE, buf: PVOID, len: u32, write: bool) -> (u32, i32) {
    let mut request = WDF_NO_HANDLE as WDFREQUEST;
    let status: i32 = unsafe {
        call_unsafe_wdf_function_binding!(
            WdfRequestCreate,
            WDF_NO_OBJECT_ATTRIBUTES,
            WDF_NO_HANDLE as WDFIOTARGET,
            &raw mut request,
        )
    };
    if !nt_success(status) {
        return (0, status);
    }

    #[allow(clippy::cast_possible_truncation)]
    let mut md: WDF_MEMORY_DESCRIPTOR = unsafe { core::mem::zeroed() };
    md.Type = WDF_MEMORY_DESCRIPTOR_TYPE_BUFFER as _;
    md.u.BufferType.Length = len;
    md.u.BufferType.Buffer = buf;

    #[allow(clippy::cast_possible_truncation)]
    let mut options: WDF_REQUEST_SEND_OPTIONS = unsafe { core::mem::zeroed() };
    options.Size = core::mem::size_of::<WDF_REQUEST_SEND_OPTIONS>() as u32;
    options.Flags = (WDF_REQUEST_SEND_OPTION_SYNCHRONOUS | WDF_REQUEST_SEND_OPTION_TIMEOUT) as _;
    options.Timeout = -50_000_000; // 5 s in 100-ns units, relative

    let mut bytes: ULONG = 0;
    let send_status: i32 = unsafe {
        if write {
            call_unsafe_wdf_function_binding!(
                WdfUsbTargetPipeWriteSynchronously,
                pipe,
                request,
                &raw mut options,
                &raw mut md,
                &raw mut bytes,
            )
        } else {
            call_unsafe_wdf_function_binding!(
                WdfUsbTargetPipeReadSynchronously,
                pipe,
                request,
                &raw mut options,
                &raw mut md,
                &raw mut bytes,
            )
        }
    };

    unsafe {
        call_unsafe_wdf_function_binding!(WdfObjectDelete, request as WDFOBJECT);
    }
    (bytes, send_status)
}

/// Execute one CDB over BOT. Returns `(csw_status, bytes_transferred)`.
unsafe fn bot_execute(
    bulk_in: WDFUSBPIPE,
    bulk_out: WDFUSBPIPE,
    cdb: &[u8],
    data: PVOID,
    len: u32,
    data_in: bool,
) -> (CswStatus, u32) {
    let tag = CBW_TAG.fetch_add(1, Ordering::Relaxed);
    let dir = if data_in {
        Direction::In
    } else {
        Direction::Out
    };
    let cbw = Cbw::new(tag, dir, 0, len, cdb).encode();

    // 1. CBW
    let (_, st) = unsafe { bulk(bulk_out, cbw.as_ptr().cast_mut().cast(), 31, true) };
    if st < 0 {
        return (CswStatus::Failed, 0);
    }

    // 2. data phase
    let mut moved = 0u32;
    if len > 0 && !data.is_null() {
        let (bytes, st) = unsafe {
            bulk(
                if data_in { bulk_in } else { bulk_out },
                data,
                len,
                !data_in,
            )
        };
        if st < 0 {
            return (CswStatus::Failed, 0);
        }
        moved = bytes;
    }

    // 3. CSW
    let mut csw_buf = [0u8; 13];
    let (_, st) = unsafe { bulk(bulk_in, csw_buf.as_mut_ptr().cast(), 13, false) };
    if st < 0 {
        return (CswStatus::Failed, moved);
    }
    match parse_csw(&csw_buf) {
        Ok(csw) => (csw.status, moved),
        Err(_) => (CswStatus::Failed, moved),
    }
}

/// `IRP_MJ_SCSI` handler for the child PDO.
unsafe extern "C" fn evt_irp_scsi(device: WDFDEVICE, irp: PIRP) -> NTSTATUS {
    let driver = unsafe { call_unsafe_wdf_function_binding!(WdfDeviceGetDriver, device) };
    diag::set(driver, &diag::N_IRP_ENTER, 1);

    let srb: *mut ScsiRequestBlock = unsafe { srb_from_irp(irp) };
    if srb.is_null() {
        diag::set(driver, &diag::N_IRP_NULL_SRB, 1);
        return unsafe { complete(irp, wdk_sys::STATUS_INVALID_DEVICE_REQUEST, 0) };
    }
    let srb = unsafe { &mut *srb };
    diag::set(driver, &diag::N_SRB_ENTER, 1);
    diag::set(driver, &diag::N_SRB_FUNC, u32::from(srb.function));

    // `classpnp` claims the device before issuing any SCSI I/O. A bus driver
    // must succeed this AND return the PDO's WDM device object in DataBuffer —
    // classpnp dereferences it during initialization.
    if srb.function == SRB_FUNCTION_CLAIM_DEVICE {
        let pdo = unsafe { call_unsafe_wdf_function_binding!(WdfDeviceWdmGetDeviceObject, device) };
        srb.data_buffer = pdo.cast();
        srb.srb_status = SRB_STATUS_SUCCESS;
        srb.scsi_status = SCSISTAT_GOOD;
        #[allow(clippy::cast_possible_truncation)]
        diag::set(driver, &diag::N_CLAIM_DB, srb.data_buffer as usize as u32);
        diag::set(driver, &diag::N_CLAIM_STATUS, 0);
        return unsafe { complete(irp, wdk_sys::STATUS_SUCCESS, 0) };
    }

    // Symmetric release; clear the claim.
    if srb.function == SRB_FUNCTION_RELEASE_DEVICE {
        srb.data_buffer = core::ptr::null_mut();
        srb.srb_status = SRB_STATUS_SUCCESS;
        srb.scsi_status = SCSISTAT_GOOD;
        return unsafe { complete(irp, wdk_sys::STATUS_SUCCESS, 0) };
    }

    if srb.function != SRB_FUNCTION_EXECUTE_SCSI {
        // Be permissive with other control functions (reset, flush, …) so the
        // storage stack can bring the device up.
        diag::set(driver, &diag::N_SRB_OTHER_FUNC, u32::from(srb.function));
        srb.srb_status = SRB_STATUS_SUCCESS;
        srb.scsi_status = SCSISTAT_GOOD;
        return unsafe { complete(irp, wdk_sys::STATUS_SUCCESS, 0) };
    }

    let Some((bulk_in, bulk_out)) = (unsafe { parent_pipes(device) }) else {
        diag::set(driver, &diag::N_HAS_PIPES, 0);
        srb.srb_status = SRB_STATUS_ERROR;
        return unsafe { complete(irp, wdk_sys::STATUS_DEVICE_NOT_READY, 0) };
    };
    diag::set(driver, &diag::N_HAS_PIPES, 1);

    let cdb_len = usize::from(srb.cdb_length & 0x1F).min(16);
    if cdb_len == 0 {
        srb.srb_status = SRB_STATUS_INVALID_REQUEST;
        return unsafe { complete(irp, wdk_sys::STATUS_SUCCESS, 0) };
    }
    let cdb = srb.cdb[..cdb_len].to_vec();

    // Where does the data live? Buffered I/O gives DataBuffer directly; direct
    // I/O hands us an MDL, which we must map to a system VA ourselves.
    let mdl = unsafe { (*irp).MdlAddress };
    let mut buffer: PVOID = srb.data_buffer;
    if buffer.is_null() && !mdl.is_null() {
        buffer = unsafe { MmMapLockedPagesSpecifyCache(mdl, 0, 1, core::ptr::null_mut(), 0, 0x10) };
    }

    let driver = unsafe { call_unsafe_wdf_function_binding!(WdfDeviceGetDriver, device) };
    diag::set(driver, &diag::N_SRB_ENTER, 1);
    if let Some(&op) = cdb.first() {
        diag::set(driver, &diag::N_SRB_OPCODE, u32::from(op));
    }

    // The device implements the command set; pass everything through.
    let flags = srb.srb_flags;
    let data_in = flags & SRB_FLAGS_DATA_IN != 0;
    let has_data = flags & (SRB_FLAGS_DATA_IN | SRB_FLAGS_DATA_OUT) != 0;
    let len = if has_data {
        srb.data_transfer_length
    } else {
        0
    };

    println!(
        "{DRIVER_TAG}: SRB cdb={:02X} len={} dir={}",
        cdb.first().copied().unwrap_or(0),
        len,
        if data_in { "in" } else { "out" },
    );

    let (status, moved) = unsafe { bot_execute(bulk_in, bulk_out, &cdb, buffer, len, data_in) };
    #[allow(clippy::cast_sign_loss)]
    diag::set(driver, &diag::N_SRB_CSW, status as u32);
    diag::set(driver, &diag::N_SRB_MOVED, moved);

    match status {
        CswStatus::Passed => {
            srb.srb_status = SRB_STATUS_SUCCESS;
            srb.scsi_status = SCSISTAT_GOOD;
            unsafe { complete(irp, wdk_sys::STATUS_SUCCESS, moved) }
        }
        _ => {
            srb.srb_status = SRB_STATUS_ERROR;
            unsafe { complete(irp, wdk_sys::STATUS_IO_DEVICE_ERROR, 0) }
        }
    }
}

/// Complete an IRP with `status` and `information`.
///
/// `IoCompleteRequest` is exported by ntoskrnl but not generated into
/// `wdk-sys`, so we declare it directly.
unsafe fn complete(irp: PIRP, status: NTSTATUS, information: u32) -> NTSTATUS {
    unsafe {
        (*irp).IoStatus.__bindgen_anon_1.Status = status;
        (*irp).IoStatus.Information = u64::from(information);
        IoCompleteRequest(irp, 0);
    }
    status
}

/// Create a default queue on the child PDO so device-control requests from
/// `disk.sys`/`classpnp` don't fail with `STATUS_INVALID_DEVICE_REQUEST`.
pub fn create_child_queue(device: WDFDEVICE) {
    #[allow(clippy::cast_possible_truncation)]
    let mut config: wdk_sys::WDF_IO_QUEUE_CONFIG = unsafe { core::mem::zeroed() };
    config.Size = core::mem::size_of::<wdk_sys::WDF_IO_QUEUE_CONFIG>() as u32;
    config.DispatchType = 1; // WdfIoQueueDispatchSequential
    config.EvtIoDefault =
        Some(evt_io_default as unsafe extern "C" fn(wdk_sys::WDFQUEUE, WDFREQUEST));
    // PowerManaged left at WdfUseDefault (0).

    let mut queue = WDF_NO_HANDLE as wdk_sys::WDFQUEUE;
    let _ = unsafe {
        call_unsafe_wdf_function_binding!(
            WdfIoQueueCreate,
            device,
            &raw mut config,
            WDF_NO_OBJECT_ATTRIBUTES,
            &raw mut queue,
        )
    };
}

/// Default handler for anything that isn't an SRB. Log the major function so we
/// can see which PnP/IOCTL requests the storage stack sends, then complete.
unsafe extern "C" fn evt_io_default(queue: wdk_sys::WDFQUEUE, request: WDFREQUEST) {
    let device = unsafe { call_unsafe_wdf_function_binding!(WdfIoQueueGetDevice, queue) };
    let driver = unsafe { call_unsafe_wdf_function_binding!(WdfDeviceGetDriver, device) };
    let irp = unsafe { call_unsafe_wdf_function_binding!(WdfRequestWdmGetIrp, request) };
    let major = unsafe { crate::srb::major_function(irp) };
    diag::set(driver, &diag::N_IO_DEFAULT, u32::from(major));
    unsafe {
        call_unsafe_wdf_function_binding!(WdfRequestComplete, request, wdk_sys::STATUS_SUCCESS);
    }
}

extern "system" {
    fn IoCompleteRequest(irp: PIRP, priority_boost: i8);
    /// Maps an MDL to a system VA. `MmGetSystemAddressForMdlSafe` is a macro,
    /// so we bind the exported function it forwards to.
    fn MmMapLockedPagesSpecifyCache(
        mdl: wdk_sys::PMDL,
        access_mode: i8,
        cache_type: i32,
        requested_address: PVOID,
        bug_check_on_failure: u32,
        priority: u32,
    ) -> PVOID;
}
