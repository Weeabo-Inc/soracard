// Copyright (c) SoraCard
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Device creation and the PnP/power lifecycle.

use crate::wdf_object_context::wdf_get_context_type_info;
use crate::{
    diag, usb, DeviceContext, DRIVER_TAG, DRIVER_VERSION, WDF_OBJECT_ATTRIBUTES_SIZE,
    WDF_PNPPOWER_EVENT_CALLBACKS_SIZE,
};
use wdk::{nt_success, paged_code, println};
use wdk_sys::{
    call_unsafe_wdf_function_binding, _WDF_EXECUTION_LEVEL, _WDF_SYNCHRONIZATION_SCOPE, NTSTATUS,
    STATUS_SUCCESS, WDFCMRESLIST, WDFDEVICE, WDFDEVICE_INIT, WDFOBJECT, WDFUSBDEVICE,
    WDF_NO_HANDLE, WDF_OBJECT_ATTRIBUTES, WDF_PNPPOWER_EVENT_CALLBACKS,
};

/// Create the KMDF device object for one instance of the reader.
#[link_section = "PAGE"]
pub fn create(mut device_init: &mut WDFDEVICE_INIT) -> NTSTATUS {
    paged_code!();
    println!("{DRIVER_TAG} v{DRIVER_VERSION}: EvtDriverDeviceAdd");

    let mut pnp_power_callbacks = WDF_PNPPOWER_EVENT_CALLBACKS {
        Size: WDF_PNPPOWER_EVENT_CALLBACKS_SIZE,
        EvtDevicePrepareHardware: Some(evt_device_prepare_hardware),
        EvtDeviceReleaseHardware: Some(evt_device_release_hardware),
        ..WDF_PNPPOWER_EVENT_CALLBACKS::default()
    };
    unsafe {
        call_unsafe_wdf_function_binding!(
            WdfDeviceInitSetPnpPowerEventCallbacks,
            device_init,
            &raw mut pnp_power_callbacks
        );
    }

    // Configure the default child list (must precede WdfDeviceCreate).
    crate::bus::configure_child_list(device_init);

    // NOTE: ExecutionLevel and SynchronizationScope must be set to a *valid*
    // enum value. Their zero value is the "Invalid" discriminant, and KMDF
    // rejects the attributes with STATUS_WDF_OBJECT_ATTRIBUTES_INVALID.
    let mut attributes = WDF_OBJECT_ATTRIBUTES {
        Size: WDF_OBJECT_ATTRIBUTES_SIZE,
        ExecutionLevel: _WDF_EXECUTION_LEVEL::WdfExecutionLevelInheritFromParent,
        SynchronizationScope: _WDF_SYNCHRONIZATION_SCOPE::WdfSynchronizationScopeInheritFromParent,
        ContextTypeInfo: wdf_get_context_type_info!(DeviceContext),
        ..WDF_OBJECT_ATTRIBUTES::default()
    };

    let mut device = WDF_NO_HANDLE as WDFDEVICE;
    let nt_status = unsafe {
        call_unsafe_wdf_function_binding!(
            WdfDeviceCreate,
            (core::ptr::addr_of_mut!(device_init)).cast(),
            &raw mut attributes,
            &raw mut device,
        )
    };
    if !nt_success(nt_status) {
        println!("{DRIVER_TAG}: WdfDeviceCreate failed {nt_status:#010X}");
        return nt_status;
    }

    let context: *mut DeviceContext =
        unsafe { crate::wdf_object_get_device_context(device as WDFOBJECT) };
    if !context.is_null() {
        unsafe {
            (*context).usb_target = WDF_NO_HANDLE as WDFUSBDEVICE;
            (*context).usb_interface = WDF_NO_HANDLE as wdk_sys::WDFUSBINTERFACE;
            (*context).bulk_in = WDF_NO_HANDLE as wdk_sys::WDFUSBPIPE;
            (*context).bulk_out = WDF_NO_HANDLE as wdk_sys::WDFUSBPIPE;
            (*context).child_list = WDF_NO_HANDLE as wdk_sys::WDFCHILDLIST;
        }
    }

    println!("{DRIVER_TAG}: device created");
    STATUS_SUCCESS
}

/// Called once the device's hardware resources are available. Opens the USB
/// target and records the device descriptor in the registry trace.
#[link_section = "PAGE"]
extern "C" fn evt_device_prepare_hardware(
    device: WDFDEVICE,
    _resources_raw: WDFCMRESLIST,
    _resources_translated: WDFCMRESLIST,
) -> NTSTATUS {
    println!("{DRIVER_TAG}: EvtDevicePrepareHardware");

    // SAFETY: valid handles supplied by KMDF.
    let driver = unsafe { call_unsafe_wdf_function_binding!(WdfDeviceGetDriver, device) };
    diag::set(driver, &diag::N_PREPARE, 1);

    let context: *mut DeviceContext =
        unsafe { crate::wdf_object_get_device_context(device as WDFOBJECT) };

    match usb::create_target(device, driver) {
        Ok((target, interface, pipes)) => {
            if !context.is_null() {
                unsafe {
                    (*context).usb_target = target;
                    (*context).usb_interface = interface;
                    (*context).bulk_in = pipes.bulk_in;
                    (*context).bulk_out = pipes.bulk_out;
                }
            }
            // Publish the LUN in EvtDeviceSelfManagedIoInit (post-start), not
            // here — adding children mid-start doesn't reliably trigger PnP
            // re-enumeration.
            STATUS_SUCCESS
        }
        Err(status) => status,
    }
}

/// Called when resources are being released (stop, surprise-remove, rebalance).
#[link_section = "PAGE"]
extern "C" fn evt_device_release_hardware(
    _device: WDFDEVICE,
    _resources_translated: WDFCMRESLIST,
) -> NTSTATUS {
    println!("{DRIVER_TAG}: EvtDeviceReleaseHardware");
    STATUS_SUCCESS
}
