// Copyright (c) SoraCard
// SPDX-License-Identifier: MIT OR Apache-2.0

//! The storage stack hands us `SCSI_REQUEST_BLOCK`s via `IRP_MJ_SCSI`.
//!
//! `wdk-sys` leaves `SCSI_REQUEST_BLOCK` opaque (the full definition lives in
//! `srb.h`, which the WDK NuGet package does not ship), so we re-declare the
//! x64 layout here. The offsets are taken from `storport.h`'s annotated
//! definition; the `_WIN64` padding (`Reserved`) aligns `Cdb` to offset 72.

use wdk_sys::{_IO_STACK_LOCATION, PIRP, PVOID};

/// `SCSI_REQUEST_BLOCK.Function`
pub const SRB_FUNCTION_EXECUTE_SCSI: u8 = 0x00;
pub const SRB_FUNCTION_CLAIM_DEVICE: u8 = 0x01;
pub const SRB_FUNCTION_RELEASE_DEVICE: u8 = 0x06;

/// `SCSI_REQUEST_BLOCK.SrbStatus`
pub const SRB_STATUS_SUCCESS: u8 = 0x01;
pub const SRB_STATUS_ERROR: u8 = 0x04;
pub const SRB_STATUS_INVALID_REQUEST: u8 = 0x06;

/// `SCSI_REQUEST_BLOCK.SrbFlags`
pub const SRB_FLAGS_DATA_IN: u32 = 0x0000_0040;
pub const SRB_FLAGS_DATA_OUT: u32 = 0x0000_0080;

/// `SCSI_REQUEST_BLOCK.ScsiStatus`
pub const SCSISTAT_GOOD: u8 = 0x00;
#[allow(dead_code)]
pub const SCSISTAT_CHECK_CONDITION: u8 = 0x02;

/// Re-declaration of the WDK `SCSI_REQUEST_BLOCK` (x64).
#[repr(C)]
pub struct ScsiRequestBlock {
    pub length: u16,
    pub function: u8,
    pub srb_status: u8,
    pub scsi_status: u8,
    pub path_id: u8,
    pub target_id: u8,
    pub lun: u8,
    pub queue_tag: u8,
    pub queue_action: u8,
    pub cdb_length: u8,
    pub sense_info_buffer_length: u8,
    pub srb_flags: u32,
    pub data_transfer_length: u32,
    pub time_out_value: u32,
    pub data_buffer: PVOID,
    pub sense_info_buffer: PVOID,
    pub next_srb: *mut ScsiRequestBlock,
    pub original_request: PVOID,
    pub srb_extension: PVOID,
    pub internal_status: u32,
    pub reserved: u32,
    pub cdb: [u8; 16],
}

/// Fetch both the major and minor function of the current IRP stack location.
///
/// # Safety
/// `irp` must be a valid IRP.
pub unsafe fn major_minor_function(irp: PIRP) -> (u8, u8) {
    if irp.is_null() {
        return (0, 0);
    }
    let stack: *mut _IO_STACK_LOCATION = unsafe {
        (*irp)
            .Tail
            .Overlay
            .__bindgen_anon_2
            .__bindgen_anon_1
            .CurrentStackLocation
    };
    if stack.is_null() {
        (0, 0)
    } else {
        unsafe { ((*stack).MajorFunction, (*stack).MinorFunction) }
    }
}

/// Fetch the major function of the current IRP stack location.
///
/// # Safety
/// `irp` must be a valid IRP.
pub unsafe fn major_function(irp: PIRP) -> u8 {
    if irp.is_null() {
        return 0;
    }
    let stack: *mut _IO_STACK_LOCATION = unsafe {
        (*irp)
            .Tail
            .Overlay
            .__bindgen_anon_2
            .__bindgen_anon_1
            .CurrentStackLocation
    };
    if stack.is_null() {
        0
    } else {
        unsafe { (*stack).MajorFunction }
    }
}

/// Fetch the `SCSI_REQUEST_BLOCK` for the current IRP stack location.
///
/// Returns null if the stack location or SRB pointer is null.
///
/// # Safety
/// `irp` must be a valid IRP for an `IRP_MJ_SCSI` request.
pub unsafe fn srb_from_irp(irp: PIRP) -> *mut ScsiRequestBlock {
    if irp.is_null() {
        return core::ptr::null_mut();
    }
    // IoGetCurrentIrpStackLocation(Irp) == Irp->Tail.Overlay.CurrentStackLocation
    let stack: *mut _IO_STACK_LOCATION = unsafe {
        (*irp)
            .Tail
            .Overlay
            .__bindgen_anon_2
            .__bindgen_anon_1
            .CurrentStackLocation
    };
    if stack.is_null() {
        return core::ptr::null_mut();
    }
    // Parameters.Scsi.Srb
    unsafe { (*stack).Parameters.Scsi.Srb }.cast::<ScsiRequestBlock>()
}
