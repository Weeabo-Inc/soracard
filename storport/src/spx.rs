// Copyright (c) SoraCard
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Thin shims over `StorPortExtendedFunction`.
//!
//! Several StorPort APIs we need (`StorPortGetDeviceObjects`,
//! `StorPortGetSystemAddress`, `StorPortInitializeWorker`,
//! `StorPortQueueWorkItem`, …) are `FORCEINLINE` wrappers in `storport.h`, so
//! bindgen emits no callable symbols for them. They all forward to the exported
//! variadic `StorPortExtendedFunction` with a function-code enum value.

use storport_sys as sp;

/// `STOR_STATUS_SUCCESS`
pub const STOR_STATUS_SUCCESS: u32 = 0;

/// `StorPortGetDeviceObjects` → (AdapterDeviceObject/FDO, PhysicalDeviceObject,
/// LowerDeviceObject). The PDO is what `USBD_CreateHandle` targets; the lower
/// device object is the `IOCTL_INTERNAL_USB_SUBMIT_URB` target.
///
/// # Safety
/// `ext` must be a valid StorPort device extension.
pub unsafe fn get_device_objects(ext: sp::PVOID) -> Option<(sp::PVOID, sp::PVOID, sp::PVOID)> {
    let mut fdo: sp::PVOID = core::ptr::null_mut();
    let mut pdo: sp::PVOID = core::ptr::null_mut();
    let mut lower: sp::PVOID = core::ptr::null_mut();

    let status = unsafe {
        sp::StorPortExtendedFunction(
            sp::_STORPORT_FUNCTION_CODE::ExtFunctionGetDeviceObjects,
            ext,
            (&raw mut fdo).cast::<sp::PVOID>(),
            (&raw mut pdo).cast::<sp::PVOID>(),
            (&raw mut lower).cast::<sp::PVOID>(),
        )
    };

    if status == STOR_STATUS_SUCCESS && !pdo.is_null() {
        Some((fdo, pdo, lower))
    } else {
        None
    }
}

/// `StorPortGetSystemAddress` → system VA of the SRB's data buffer.
///
/// # Safety
/// `ext` must be a valid StorPort device extension and `srb` an outstanding
/// SRB with a data buffer.
pub unsafe fn get_system_address(
    ext: sp::PVOID,
    srb: sp::PSCSI_REQUEST_BLOCK,
) -> Option<sp::PVOID> {
    let mut va: sp::PVOID = core::ptr::null_mut();
    // SAFETY: matches storport.h's inline body:
    // `StorPortExtendedFunction(ExtFunctionGetSystemAddress, ext, Srb, &va)`.
    let status = unsafe {
        sp::StorPortExtendedFunction(
            sp::_STORPORT_FUNCTION_CODE::ExtFunctionGetSystemAddress,
            ext,
            srb,
            (&raw mut va).cast::<sp::PVOID>(),
        )
    };
    (status == STOR_STATUS_SUCCESS && !va.is_null()).then_some(va)
}
