// Copyright (c) SoraCard
// SPDX-License-Identifier: MIT OR Apache-2.0

//! USB target bring-up: open the target, select the configuration, and claim
//! the bulk pipes the Bulk-Only Transport layer needs.

use crate::{diag, DRIVER_TAG};
use wdk::{nt_success, println};
use wdk_sys::{
    _WdfUsbTargetDeviceSelectConfigType, call_unsafe_wdf_function_binding, NTSTATUS,
    USB_DEVICE_DESCRIPTOR, WDFDEVICE, WDFDRIVER, WDFUSBDEVICE, WDFUSBINTERFACE, WDFUSBPIPE,
    WDF_NO_HANDLE, WDF_NO_OBJECT_ATTRIBUTES, WDF_USB_DEVICE_SELECT_CONFIG_PARAMS,
    WDF_USB_PIPE_INFORMATION,
};

/// The pipes a Bulk-Only Transport device needs.
#[derive(Debug, Clone, Copy)]
pub struct Pipes {
    pub bulk_in: WDFUSBPIPE,
    pub bulk_out: WDFUSBPIPE,
}

/// Create the USB target, select the configuration, and collect the pipes.
pub fn create_target(
    device: WDFDEVICE,
    driver: WDFDRIVER,
) -> Result<(WDFUSBDEVICE, WDFUSBINTERFACE, Pipes), NTSTATUS> {
    let mut target = WDF_NO_HANDLE as WDFUSBDEVICE;
    let status = unsafe {
        call_unsafe_wdf_function_binding!(
            WdfUsbTargetDeviceCreate,
            device,
            WDF_NO_OBJECT_ATTRIBUTES,
            &raw mut target,
        )
    };
    if !nt_success(status) {
        println!("{DRIVER_TAG}: WdfUsbTargetDeviceCreate failed {status:#010X}");
        #[allow(clippy::cast_sign_loss)]
        diag::set(driver, &diag::N_TARGET_STATUS, status as u32);
        return Err(status);
    }
    diag::set(driver, &diag::N_TARGET_STATUS, 0);

    let mut dd = USB_DEVICE_DESCRIPTOR::default();
    unsafe {
        call_unsafe_wdf_function_binding!(
            WdfUsbTargetDeviceGetDeviceDescriptor,
            target,
            &raw mut dd
        );
    }
    let vid = dd.idVendor;
    let pid = dd.idProduct;
    let bcd_usb = dd.bcdUSB;
    let class = dd.bDeviceClass;
    let sub = dd.bDeviceSubClass;
    let proto = dd.bDeviceProtocol;
    let configs = dd.bNumConfigurations;
    diag::set(driver, &diag::N_VID, u32::from(vid));
    diag::set(driver, &diag::N_PID, u32::from(pid));
    diag::set(
        driver,
        &diag::N_BANNER_A,
        (u32::from(bcd_usb) << 16) | (u32::from(configs) << 8),
    );
    diag::set(
        driver,
        &diag::N_BANNER_B,
        (u32::from(class) << 16) | (u32::from(sub) << 8) | u32::from(proto),
    );
    println!(
        "{DRIVER_TAG}: USB device VID={vid:04X} PID={pid:04X} bcdUSB={bcd_usb:04X} \
         class={class:02X} sub={sub:02X} proto={proto:02X} configs={configs}",
    );

    // --- select the default configuration / single interface ----------------
    #[allow(clippy::cast_possible_truncation)]
    let select_size = core::mem::size_of::<WDF_USB_DEVICE_SELECT_CONFIG_PARAMS>() as u32;
    let mut select = WDF_USB_DEVICE_SELECT_CONFIG_PARAMS {
        Size: select_size,
        Type:
            _WdfUsbTargetDeviceSelectConfigType::WdfUsbTargetDeviceSelectConfigTypeSingleInterface,
        // SAFETY: the C INIT macro zeroes the union; zeroed is valid here.
        Types: unsafe { core::mem::zeroed() },
    };
    let status = unsafe {
        call_unsafe_wdf_function_binding!(
            WdfUsbTargetDeviceSelectConfig,
            target,
            WDF_NO_OBJECT_ATTRIBUTES,
            &raw mut select,
        )
    };
    if !nt_success(status) {
        println!("{DRIVER_TAG}: SelectConfig failed {status:#010X}");
        #[allow(clippy::cast_sign_loss)]
        diag::set(driver, &diag::N_SELECT_STATUS, status as u32);
        return Err(status);
    }
    diag::set(driver, &diag::N_SELECT_STATUS, 0);

    // --- interface + configured pipes ---------------------------------------
    let num_interfaces =
        unsafe { call_unsafe_wdf_function_binding!(WdfUsbTargetDeviceGetNumInterfaces, target) };
    diag::set(driver, &diag::N_NUM_IFACES, u32::from(num_interfaces));
    if num_interfaces == 0 {
        return Err(wdk_sys::STATUS_UNSUCCESSFUL);
    }

    let interface =
        unsafe { call_unsafe_wdf_function_binding!(WdfUsbTargetDeviceGetInterface, target, 0u8) };

    let num_pipes = unsafe {
        call_unsafe_wdf_function_binding!(WdfUsbInterfaceGetNumConfiguredPipes, interface)
    };
    diag::set(driver, &diag::N_NUM_PIPES, u32::from(num_pipes));

    let mut bulk_in = WDF_NO_HANDLE as WDFUSBPIPE;
    let mut bulk_out = WDF_NO_HANDLE as WDFUSBPIPE;
    let mut i: u8 = 0;
    while i < num_pipes {
        #[allow(clippy::cast_possible_truncation)]
        let info_size = core::mem::size_of::<WDF_USB_PIPE_INFORMATION>() as u32;
        let mut info = WDF_USB_PIPE_INFORMATION {
            Size: info_size,
            ..WDF_USB_PIPE_INFORMATION::default()
        };
        let pipe = unsafe {
            call_unsafe_wdf_function_binding!(
                WdfUsbInterfaceGetConfiguredPipe,
                interface,
                i,
                &raw mut info,
            )
        };
        let addr = info.EndpointAddress;
        let ptype = info.PipeType;
        diag::set(
            driver,
            &diag::N_EP_INFO,
            (u32::from(addr) << 16) | (ptype as u32),
        );
        if ptype == wdk_sys::_WDF_USB_PIPE_TYPE::WdfUsbPipeTypeBulk {
            if addr & 0x80 != 0 {
                bulk_in = pipe;
            } else {
                bulk_out = pipe;
            }
        }
        i += 1;
    }

    if bulk_in.is_null() || bulk_out.is_null() {
        println!("{DRIVER_TAG}: missing bulk pipe(s)");
        return Err(wdk_sys::STATUS_UNSUCCESSFUL);
    }
    diag::set(driver, &diag::N_BULK_IN, 1);
    diag::set(driver, &diag::N_BULK_OUT, 1);

    Ok((target, interface, Pipes { bulk_in, bulk_out }))
}
