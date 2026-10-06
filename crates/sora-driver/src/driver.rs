// Copyright (c) SoraCard
// SPDX-License-Identifier: MIT OR Apache-2.0

//! `DriverEntry` and the KMDF driver object.

use crate::{device, DRIVER_TAG, DRIVER_VERSION, WDF_DRIVER_CONFIG_SIZE};
use wdk::{nt_success, println};
use wdk_sys::{
    call_unsafe_wdf_function_binding, DRIVER_OBJECT, NTSTATUS, PCUNICODE_STRING, PDRIVER_OBJECT,
    PWDFDEVICE_INIT, WDFDRIVER, WDF_DRIVER_CONFIG, WDF_NO_HANDLE, WDF_NO_OBJECT_ATTRIBUTES,
};

/// First routine the system calls after loading the driver.
///
/// # Safety
/// Called by the I/O manager with valid pointers; the signature is fixed by the
/// kernel and must not change.
#[link_section = "INIT"]
#[export_name = "DriverEntry"]
unsafe extern "system" fn driver_entry(
    driver: &mut DRIVER_OBJECT,
    registry_path: PCUNICODE_STRING,
) -> NTSTATUS {
    let mut driver_config = WDF_DRIVER_CONFIG {
        Size: WDF_DRIVER_CONFIG_SIZE,
        EvtDriverDeviceAdd: Some(evt_driver_device_add),
        ..WDF_DRIVER_CONFIG::default()
    };
    let driver_handle_out = WDF_NO_HANDLE.cast::<WDFDRIVER>();

    let nt_status = call_unsafe_wdf_function_binding!(
        WdfDriverCreate,
        driver as PDRIVER_OBJECT,
        registry_path,
        WDF_NO_OBJECT_ATTRIBUTES,
        &raw mut driver_config,
        driver_handle_out,
    );

    if nt_success(nt_status) {
        println!("{DRIVER_TAG} v{DRIVER_VERSION}: DriverEntry ok");
    } else {
        println!("{DRIVER_TAG}: WdfDriverCreate failed {nt_status:#010X}");
    }
    nt_status
}

/// Called by KMDF in response to an `AddDevice` from the PnP manager.
#[link_section = "PAGE"]
extern "C" fn evt_driver_device_add(driver: WDFDRIVER, device_init: PWDFDEVICE_INIT) -> NTSTATUS {
    println!("{DRIVER_TAG}: EvtDriverDeviceAdd");
    crate::diag::set(driver, &crate::diag::N_ADD_ENTERED, 1);
    // SAFETY: KMDF guarantees a valid, non-null PWDFDEVICE_INIT here.
    let nt_status = match unsafe { device_init.as_mut() } {
        Some(init) => device::create(init),
        None => wdk_sys::STATUS_INVALID_PARAMETER,
    };
    #[allow(clippy::cast_sign_loss)]
    crate::diag::set(driver, &crate::diag::N_CREATE_STATUS, nt_status as u32);
    nt_status
}
