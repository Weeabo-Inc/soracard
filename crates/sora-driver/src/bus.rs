// Copyright (c) SoraCard
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Bus enumeration: expose the reader's logical unit(s) as child PDOs so the
//! standard storage stack (`disk.sys`) binds to them.
//!
//! We use KMDF's **default** child list (configured on the FDO's device-init),
//! because that is the list PnP enumerates. `WdfFdoInitSetDefaultChildListConfig`
//! runs before `WdfDeviceCreate`; children are published afterwards with
//! `WdfChildListAddOrUpdateChildDescriptionAsPresent`.

use crate::{diag, DRIVER_TAG};
use wdk::{nt_success, println};
use wdk_sys::{
    call_unsafe_wdf_function_binding, NTSTATUS, PWDFDEVICE_INIT,
    PWDF_CHILD_IDENTIFICATION_DESCRIPTION_HEADER, UNICODE_STRING, WDFCHILDLIST, WDFDEVICE,
    WDFDEVICE_INIT, WDFDRIVER, WDF_CHILD_IDENTIFICATION_DESCRIPTION_HEADER, WDF_CHILD_LIST_CONFIG,
    WDF_NO_HANDLE, WDF_NO_OBJECT_ATTRIBUTES, WDF_OBJECT_ATTRIBUTES,
};

/// Identification description for one logical unit. The header must come first.
#[repr(C)]
pub struct LunId {
    pub header: WDF_CHILD_IDENTIFICATION_DESCRIPTION_HEADER,
    pub lun: u8,
}

const fn utf16<const N: usize>(s: &[u8; N]) -> [u16; N] {
    let mut out = [0u16; N];
    let mut i = 0;
    while i < N {
        out[i] = s[i] as u16;
        i += 1;
    }
    out
}

fn ustr(bytes: &'static [u16]) -> UNICODE_STRING {
    #[allow(clippy::cast_possible_truncation)]
    let byte_len = (bytes.len() * core::mem::size_of::<u16>()) as u16;
    UNICODE_STRING {
        Length: byte_len,
        MaximumLength: byte_len,
        Buffer: bytes.as_ptr().cast_mut(),
    }
}

const INSTANCE_ID: [u16; 1] = utf16(b"0");
/// Hardware ID in the USBSTOR device-ID form (`Disk&Ven_..&Prod_..&Rev_..`).
const HWID: [u16; 50] = utf16(b"USBSTOR\\Disk&Ven_SoraCard&Prod_RTSUERLUN0&Rev_1.00");
/// Compatible ID that `disk.sys` matches.
const COMPAT: [u16; 7] = utf16(b"GenDisk");
/// Friendly name shown in Device Manager.
const DEVICE_TEXT: [u16; 23] = utf16(b"SoraCard SD Card Reader");

/// Configure the FDO's default child list. Must run before `WdfDeviceCreate`.
pub fn configure_child_list(device_init: &mut WDFDEVICE_INIT) {
    // SAFETY: plain struct; zero is a valid initial state, then we fill it.
    let mut config: WDF_CHILD_LIST_CONFIG = unsafe { core::mem::zeroed() };
    #[allow(clippy::cast_possible_truncation)]
    {
        config.Size = core::mem::size_of::<WDF_CHILD_LIST_CONFIG>() as u32;
        config.IdentificationDescriptionSize = core::mem::size_of::<LunId>() as u32;
    }
    config.AddressDescriptionSize = 0;
    config.EvtChildListCreateDevice = Some(evt_child_list_create_device);
    config.EvtChildListScanForChildren = Some(evt_child_list_scan_for_children);

    unsafe {
        call_unsafe_wdf_function_binding!(
            WdfFdoInitSetDefaultChildListConfig,
            device_init,
            &raw mut config,
            WDF_NO_OBJECT_ATTRIBUTES,
        );
    }
}

/// KMDF calls this when the parent enters D0. We report every present child
/// inside a balanced BeginScan/EndScan session; `EndScan` is what actually
/// hands the updated list to the PnP manager.
extern "C" fn evt_child_list_scan_for_children(child_list: WDFCHILDLIST) {
    let parent = unsafe { call_unsafe_wdf_function_binding!(WdfChildListGetDevice, child_list) };
    let driver = unsafe { call_unsafe_wdf_function_binding!(WdfDeviceGetDriver, parent) };

    unsafe {
        call_unsafe_wdf_function_binding!(WdfChildListBeginScan, child_list);
    }

    // SAFETY: plain struct; zeroed then filled.
    let mut id: LunId = unsafe { core::mem::zeroed() };
    #[allow(clippy::cast_possible_truncation)]
    {
        id.header.IdentificationDescriptionSize = core::mem::size_of::<LunId>() as u32;
    }
    id.lun = 0;

    let status = unsafe {
        call_unsafe_wdf_function_binding!(
            WdfChildListAddOrUpdateChildDescriptionAsPresent,
            child_list,
            &raw mut id.header,
            core::ptr::null_mut(),
        )
    };
    #[allow(clippy::cast_sign_loss)]
    diag::set(driver, &diag::N_CHILD_PRESENT, status as u32);

    unsafe {
        call_unsafe_wdf_function_binding!(WdfChildListEndScan, child_list);
    }
    diag::set(driver, &diag::N_SCAN_END, 1);
    println!("{DRIVER_TAG}: child scan complete (status {status:#010X})");
}

/// KMDF calls this to build the PDO for a child. `child_init` is provided by
/// the framework (already allocated).
extern "C" fn evt_child_list_create_device(
    child_list: WDFCHILDLIST,
    _identification: PWDF_CHILD_IDENTIFICATION_DESCRIPTION_HEADER,
    mut child_init: PWDFDEVICE_INIT,
) -> NTSTATUS {
    println!("{DRIVER_TAG}: creating child PDO");

    let parent = unsafe { call_unsafe_wdf_function_binding!(WdfChildListGetDevice, child_list) };
    let driver = unsafe { call_unsafe_wdf_function_binding!(WdfDeviceGetDriver, parent) };
    diag::set(driver, &diag::N_CHILD_ENTERED, 1);

    // Make the PDO look like a removable disk so disk.sys will bind and start.
    const FILE_DEVICE_DISK: u32 = 0x0000_0007;
    const FILE_REMOVABLE_MEDIA: u32 = 0x0001;
    unsafe {
        let _ = call_unsafe_wdf_function_binding!(
            WdfDeviceInitSetDeviceType,
            child_init,
            FILE_DEVICE_DISK
        );
        let _ = call_unsafe_wdf_function_binding!(
            WdfDeviceInitSetCharacteristics,
            child_init,
            FILE_REMOVABLE_MEDIA,
            1u8, // OrIn = TRUE
        );
    }

    let mut init_status: i32 = 0;
    unsafe {
        let mut instance = ustr(&INSTANCE_ID);
        init_status |= call_unsafe_wdf_function_binding!(
            WdfPdoInitAssignInstanceID,
            child_init,
            &raw mut instance
        );

        let mut hwid = ustr(&HWID);
        init_status |=
            call_unsafe_wdf_function_binding!(WdfPdoInitAssignDeviceID, child_init, &raw mut hwid);
        init_status |=
            call_unsafe_wdf_function_binding!(WdfPdoInitAddHardwareID, child_init, &raw mut hwid);

        let mut compat = ustr(&COMPAT);
        init_status |= call_unsafe_wdf_function_binding!(
            WdfPdoInitAddCompatibleID,
            child_init,
            &raw mut compat
        );

        let mut text = ustr(&DEVICE_TEXT);
        let mut location = ustr(&INSTANCE_ID);
        init_status |= call_unsafe_wdf_function_binding!(
            WdfPdoInitAddDeviceText,
            child_init,
            &raw mut text,
            &raw mut location,
            0x0409u32, // en-US
        );
    }
    #[allow(clippy::cast_sign_loss)]
    diag::set(driver, &diag::N_CHILD_INIT_STATUS, init_status as u32);

    // Intercept IRP_MJ_SCSI so we can service SRBs over BOT.
    let irp_status = crate::storage::assign_irp_preprocess(child_init);
    #[allow(clippy::cast_sign_loss)]
    diag::set(driver, &diag::N_IRP_ASSIGN, irp_status as u32);

    // Pass-through PnP preprocess, purely for logging the minor function.
    let _ = crate::storage::assign_pnp_preprocess(child_init);

    #[allow(clippy::cast_possible_truncation)]
    let mut attributes = WDF_OBJECT_ATTRIBUTES {
        Size: core::mem::size_of::<WDF_OBJECT_ATTRIBUTES>() as u32,
        ExecutionLevel: wdk_sys::_WDF_EXECUTION_LEVEL::WdfExecutionLevelInheritFromParent,
        SynchronizationScope:
            wdk_sys::_WDF_SYNCHRONIZATION_SCOPE::WdfSynchronizationScopeInheritFromParent,
        ..WDF_OBJECT_ATTRIBUTES::default()
    };

    let mut child = WDF_NO_HANDLE as WDFDEVICE;
    let status = unsafe {
        call_unsafe_wdf_function_binding!(
            WdfDeviceCreate,
            &raw mut child_init,
            &raw mut attributes,
            &raw mut child,
        )
    };
    if !nt_success(status) {
        println!("{DRIVER_TAG}: child WdfDeviceCreate failed {status:#010X}");
    } else {
        // Give the child a default queue so storage-stack probes are answered.
        crate::storage::create_child_queue(child);
    }
    #[allow(clippy::cast_sign_loss)]
    diag::set(driver, &diag::N_CHILD_CREATED, status as u32);
    status
}
