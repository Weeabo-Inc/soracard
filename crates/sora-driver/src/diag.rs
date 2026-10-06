// Copyright (c) SoraCard
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Temporary bring-up diagnostics.
//!
//! DebugView's kernel capture is unavailable in this environment, so progress
//! values are written to the driver's own `Parameters` registry key
//! (`HKLM\SYSTEM\CurrentControlSet\Services\SoraCard\Parameters`), which is
//! trivially readable from user mode with `reg query`. This module will be
//! removed once the driver is proven on hardware.

use wdk::println;
use wdk_sys::{
    call_unsafe_wdf_function_binding, NTSTATUS, ULONG, UNICODE_STRING, WDFDRIVER, WDFKEY,
    WDF_NO_HANDLE, WDF_NO_OBJECT_ATTRIBUTES,
};

const KEY_SET_VALUE: u32 = 0x0002;

/// Widen an ASCII byte string to a UTF-16 array at compile time.
const fn utf16<const N: usize>(s: &[u8; N]) -> [u16; N] {
    let mut out = [0u16; N];
    let mut i = 0;
    while i < N {
        out[i] = s[i] as u16;
        i += 1;
    }
    out
}

pub const N_ADD_ENTERED: [u16; 10] = utf16(b"AddEntered");
pub const N_CREATE_STATUS: [u16; 12] = utf16(b"CreateStatus");
pub const N_PREPARE: [u16; 14] = utf16(b"PrepareEntered");
pub const N_TARGET_STATUS: [u16; 12] = utf16(b"TargetStatus");
pub const N_VID: [u16; 3] = utf16(b"Vid");
pub const N_PID: [u16; 3] = utf16(b"Pid");
pub const N_BANNER_A: [u16; 6] = utf16(b"BcdCfg");
pub const N_BANNER_B: [u16; 6] = utf16(b"ClsSub");
pub const N_SELECT_STATUS: [u16; 12] = utf16(b"SelectStatus");
pub const N_NUM_IFACES: [u16; 9] = utf16(b"NumIfaces");
pub const N_NUM_PIPES: [u16; 8] = utf16(b"NumPipes");
pub const N_EP_INFO: [u16; 6] = utf16(b"EpInfo");
pub const N_BULK_IN: [u16; 6] = utf16(b"BulkIn");
pub const N_BULK_OUT: [u16; 7] = utf16(b"BulkOut");
pub const N_CHILD_STATUS: [u16; 11] = utf16(b"ChildStatus");
pub const N_CHILD_PRESENT: [u16; 12] = utf16(b"ChildPresent");
pub const N_CHILD_ENTERED: [u16; 12] = utf16(b"ChildEntered");
pub const N_CHILD_CREATED: [u16; 12] = utf16(b"ChildCreated");
pub const N_CHILD_INIT_STATUS: [u16; 15] = utf16(b"ChildInitStatus");
pub const N_SCAN_END: [u16; 7] = utf16(b"ScanEnd");
pub const N_SRB_ENTER: [u16; 8] = utf16(b"SrbEnter");
pub const N_SRB_OPCODE: [u16; 5] = utf16(b"SrbOp");
pub const N_SRB_FUNC: [u16; 7] = utf16(b"SrbFunc");
pub const N_SRB_OTHER_FUNC: [u16; 12] = utf16(b"SrbOtherFunc");
pub const N_HAS_PIPES: [u16; 8] = utf16(b"HasPipes");
pub const N_IRP_ENTER: [u16; 8] = utf16(b"IrpEnter");
pub const N_IRP_NULL_SRB: [u16; 10] = utf16(b"IrpNullSrb");
pub const N_IO_DEFAULT: [u16; 9] = utf16(b"IoDefault");
pub const N_CLAIM_DB: [u16; 7] = utf16(b"ClaimDb");
pub const N_CLAIM_STATUS: [u16; 11] = utf16(b"ClaimStatus");
pub const N_PNP_MINOR: [u16; 8] = utf16(b"PnpMinor");
pub const N_SRB_CSW: [u16; 6] = utf16(b"SrbCsw");
pub const N_SRB_MOVED: [u16; 8] = utf16(b"SrbMoved");
pub const N_IRP_ASSIGN: [u16; 9] = utf16(b"IrpAssign");

fn ustr(bytes: &'static [u16]) -> UNICODE_STRING {
    #[allow(clippy::cast_possible_truncation)]
    let byte_len = (bytes.len() * core::mem::size_of::<u16>()) as u16;
    UNICODE_STRING {
        Length: byte_len,
        MaximumLength: byte_len,
        Buffer: bytes.as_ptr().cast_mut(),
    }
}

/// Record a `ULONG` under the driver's Parameters key. Best-effort: on any
/// failure it logs and returns.
pub fn set(driver: WDFDRIVER, name: &'static [u16], value: ULONG) {
    let mut key = WDF_NO_HANDLE as WDFKEY;
    // SAFETY: `driver` is a valid WDFDRIVER supplied by KMDF; `key` is a valid
    // out-pointer. The value name outlives the call (static).
    let nt_status: NTSTATUS = unsafe {
        call_unsafe_wdf_function_binding!(
            WdfDriverOpenParametersRegistryKey,
            driver,
            KEY_SET_VALUE,
            WDF_NO_OBJECT_ATTRIBUTES,
            &raw mut key,
        )
    };
    if nt_status < 0 {
        println!("SoraCard: diag open params key failed {nt_status:#010X}");
        return;
    }

    let mut value_name = ustr(name);
    // SAFETY: `key` is a WDFKEY from the call above; the name is static valid memory.
    let _assign: NTSTATUS = unsafe {
        call_unsafe_wdf_function_binding!(WdfRegistryAssignULong, key, &raw mut value_name, value,)
    };

    // Keys are WDF objects; close it.
    unsafe {
        call_unsafe_wdf_function_binding!(WdfObjectDelete, key as wdk_sys::WDFOBJECT);
    }
}
