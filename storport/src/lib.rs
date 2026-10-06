// Copyright (c) SoraCard
// SPDX-License-Identifier: MIT OR Apache-2.0

//! # soracard — a StorPort miniport for the Realtek RTS5129
//!
//! StorPort is the port driver, so it owns the claim/attach/release/queue SRB
//! set that a KMDF bus/PDO driver must otherwise emulate. We implement only the
//! miniport callbacks; `HwStorStartIo` hands `SRB_FUNCTION_EXECUTE_SCSI` to the
//! data path in [`io`]. On the RTS5129 the driver is the SCSI target itself
//! and drives the SD card through the controller's register protocol
//! ([`chip`], [`sdhost`]); genuine mass-storage readers get Bulk-Only
//! Transport pass-through. Design: docs/ARCHITECTURE.md.
//!
//! The reader is a USB-backed, PnP, resource-less device, so we register as a
//! **virtual** miniport (`VIRTUAL_HW_INITIALIZATION_DATA`, interface type
//! `Internal`, `PORT_CONFIGURATION_INFORMATION.VirtualDevice = TRUE`). This
//! is what stops StorPort trying to allocate interrupt/DMA resources.

#![no_std]
#![allow(non_snake_case, non_camel_case_types, non_upper_case_globals)]
#![deny(clippy::all)]

use storport_sys as sp;

mod chip;
mod diag;
mod io;
mod sdhost;
mod spx;
mod usb;

/// A panic parks the current thread for good. At PASSIVE_LEVEL (the worker,
/// bring-up) it also records where it happened (`PanicLine`, `PanicFile`) and
/// sleeps instead of spinning, so a panic shows up in the registry trace
/// rather than as a silently pinned CPU core.
#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    // SAFETY: reading the IRQL is always legal.
    let passive = unsafe { wdk_sys::ntddk::KeGetCurrentIrql() } == 0;
    if passive {
        if let Some(loc) = info.location() {
            diag::set_u32(diag::w!("PanicLine"), loc.line());
            let f = loc.file().as_bytes();
            diag::set_bin(diag::w!("PanicFile"), &f[f.len().saturating_sub(64)..]);
        }
    }
    loop {
        if passive {
            // SAFETY: PASSIVE_LEVEL.
            unsafe { chip::delay_us(1_000_000) };
        }
    }
}

// NOTE: `_fltused` and `__CxxFrameHandler3` used to be declared here because
// only `storport-sys` was linked. `usb.rs` now also links `wdk-sys`, which
// already provides both (as `#[no_mangle]` definitions), so declaring them
// here again would produce duplicate symbols at link time.

/// Per-adapter (HBA) state allocated by StorPort from nonpaged pool. USB pipe
/// state is filled in during passive initialization; keep it POD.
#[repr(C)]
pub struct AdapterExtension {
    /// Adapter device object (FDO).
    pub fdo: sp::PVOID,
    /// Physical device object (the USB PDO).
    pub pdo: sp::PVOID,
    /// Lower device object (target for `IOCTL_INTERNAL_USB_SUBMIT_URB` and the
    /// `TargetDeviceObject` passed to `USBD_CreateHandle`).
    pub lower: sp::PVOID,
    /// `USBD_HANDLE` from `USBD_CreateHandle` (opaque; null until bring-up).
    pub usbd_handle: sp::PVOID,
    /// `USBD_CONFIGURATION_HANDLE` returned by the select-configuration URB.
    pub config_handle: sp::PVOID,
    /// Bulk IN pipe (device -> host): `USBD_PIPE_HANDLE`.
    pub bulk_in: sp::PVOID,
    /// Bulk OUT pipe (host -> device): `USBD_PIPE_HANDLE`.
    pub bulk_out: sp::PVOID,
    /// Nonpaged scratch buffer that holds the full configuration descriptor.
    /// Kept alive for the adapter lifetime because the select-configuration URB
    /// references its address.
    pub config_desc: [u8; usb::CONFIG_DESC_MAX],
    /// Length of the valid prefix of `config_desc` (0 before bring-up).
    pub config_desc_len: u16,
    /// `bInterfaceNumber` of the BOT interface (target of Mass Storage Reset).
    pub interface_number: u8,
    /// The controller is the LQFP48 package (RTS5139-class), not QFN24:
    /// different SD pin pulls and extra power setup.
    pub lqfp48: bool,
    /// URB reused by the I/O worker for every transfer (`USBD_UrbAllocate`).
    pub urb: sp::PVOID,
    /// Data-path state: request queue, worker thread, BOT buffers.
    pub io: io::IoState,
}

/// Driver entry point. Populates `HW_INITIALIZATION_DATA` and hands control to
/// `StorPortInitialize`, which registers our callbacks.
///
/// # Safety
/// Called by the I/O manager with valid pointers.
#[no_mangle]
pub unsafe extern "C" fn DriverEntry(
    driver: sp::PDRIVER_OBJECT,
    registry_path: sp::PUNICODE_STRING,
) -> sp::ULONG {
    // Virtual miniport initialization exactly as working virtual miniports
    // (WinSpd, Arsenal Image Mounter) do it: VIRTUAL_HW_INITIALIZATION_DATA,
    // AdapterInterfaceType = Internal, no FeatureSupport flag, and
    // VirtualDevice = TRUE in the 7-parameter FindAdapter. (The earlier
    // HW_INITIALIZATION_DATA + STOR_FEATURE_VIRTUAL_MINIPORT recipe only
    // "worked" because ConfigInfo was never actually applied.)
    // SAFETY: POD, zero is a valid initial state.
    let mut init: sp::VIRTUAL_HW_INITIALIZATION_DATA = unsafe { core::mem::zeroed() };

    #[allow(clippy::cast_possible_truncation)]
    {
        init.HwInitializationDataSize =
            core::mem::size_of::<sp::VIRTUAL_HW_INITIALIZATION_DATA>() as u32;
        init.DeviceExtensionSize = core::mem::size_of::<AdapterExtension>() as u32;
    }
    init.AdapterInterfaceType = sp::_INTERFACE_TYPE::Internal;
    // Read/write buffers are resolved with StorPortGetSystemAddress.
    init.MapBuffers = sp::STOR_MAP_NON_READ_WRITE_BUFFERS as u8;
    init.AutoRequestSense = 1;
    init.MultipleRequestPerLu = 1;

    init.HwInitialize = Some(HwStorInitialize);
    init.HwStartIo = Some(HwStorStartIo);
    init.HwResetBus = Some(HwStorResetBus);
    init.HwFreeAdapterResources = Some(HwStorFreeAdapterResources);
    init.HwAdapterControl = Some(HwStorAdapterControl);
    // Typed: the compiler checks the 7-parameter virtual signature.
    init.HwFindAdapter = Some(HwStorFindAdapter);

    unsafe {
        sp::StorPortInitialize(
            driver as sp::PVOID,
            registry_path as sp::PVOID,
            (&raw mut init).cast::<sp::HW_INITIALIZATION_DATA>(),
            core::ptr::null_mut(),
        )
    }
}

/// PnP adapters are discovered by PnP; configure as a virtual device.
unsafe extern "C" fn HwStorFindAdapter(
    device_extension: sp::PVOID,
    _hw_context: sp::PVOID,
    _bus_information: sp::PVOID,
    _lower_device: sp::PVOID,
    _argument_string: sp::PCHAR,
    config_info: sp::PPORT_CONFIGURATION_INFORMATION,
    again: sp::PBOOLEAN,
) -> sp::ULONG {
    if !again.is_null() {
        // SAFETY: StorPort-provided out-parameter. One adapter per device.
        unsafe { *again = 0 };
    }
    let ext = device_extension.cast::<AdapterExtension>();
    // SAFETY: PASSIVE_LEVEL; StorPort zeroed the extension.
    unsafe { io::init(ext) };
    diag::init();
    diag::set_u32(
        diag::w!("FindAdapterCalls"),
        diag::get_u32(diag::w!("FindAdapterCalls"), 0) + 1,
    );

    // Resolve the underlying device objects (FDO / USB PDO / lower).
    if let Some((fdo, pdo, lower)) = unsafe { spx::get_device_objects(device_extension) } {
        unsafe {
            (*ext).fdo = fdo;
            (*ext).pdo = pdo;
            (*ext).lower = lower;
        }
    }

    // USB bring-up happens here: HwStorFindAdapter is called at PASSIVE_LEVEL
    // once the USB stack below us has started, and before StorPort enumerates
    // LUNs. (On this system the HwStorPassiveInitializeRoutine callback
    // requested from HwStorInitialize is never delivered to a virtual
    // miniport, so relying on it left the data path permanently pending.)
    // SAFETY: PASSIVE_LEVEL; our extension; idempotent.
    unsafe { usb::passive_initialize(device_extension) };

    if !config_info.is_null() {
        unsafe {
            (*config_info).VirtualDevice = 1; // TRUE
            (*config_info).NumberOfBuses = 1;
            #[allow(clippy::cast_possible_truncation)]
            {
                // One SRB = one SD multi-block transfer (RTCR) or one BOT
                // transfer through the bounce buffer.
                let max = io::max_transfer(ext);
                (*config_info).MaximumTransferLength = max as u32;
                (*config_info).NumberOfPhysicalBreaks = (max / 4096) as u32 + 1;
            }
            (*config_info).AlignmentMask = 0; // FILE_BYTE_ALIGNMENT
                                              // StorPort only supports bus-master scatter/gather adapters, even
                                              // virtual ones (as WinSpd sets); FALSE fails start with
                                              // STATUS_DEVICE_CONFIGURATION_ERROR.
            (*config_info).ScatterGather = 1;
            (*config_info).Master = 1;
            (*config_info).CachesData = 0;
            (*config_info).WmiDataProvider = 0;
            (*config_info).SynchronizationModel =
                sp::_STOR_SYNCHRONIZATION_MODEL::StorSynchronizeFullDuplex;
            // The reader exposes exactly one LUN (0) on one target (0).
            (*config_info).MaximumNumberOfTargets = 1;
            (*config_info).MaximumNumberOfLogicalUnits = 1;
            diag::set_u32(diag::w!("ConfigApplied"), 1);
            (*config_info).SynchronizationModel =
                sp::_STOR_SYNCHRONIZATION_MODEL::StorSynchronizeFullDuplex;
        }
    }
    sp::SP_RETURN_FOUND as sp::ULONG
}

/// Called after `HwStorFindAdapter` (which already did USB bring-up). Only
/// records the IRQL it runs at.
unsafe extern "C" fn HwStorInitialize(device_extension: sp::PVOID) -> sp::BOOLEAN {
    // Bring-up already happened in HwStorFindAdapter. Deliberately do NOT
    // request a HwStorPassiveInitializeRoutine: StorPort never delivered it to
    // this virtual miniport, and the undelivered request left every PnP
    // stop/remove of the adapter hanging (wedging pnputil and shutdown).
    // SAFETY: reading the IRQL is always legal; recorded by the worker later.
    let irql = unsafe { wdk_sys::ntddk::KeGetCurrentIrql() };
    unsafe { io::note_init_irql(device_extension.cast::<AdapterExtension>(), irql) };
    1 // TRUE
}

/// SRB entry point (IRQL <= DISPATCH_LEVEL). SCSI commands are queued for the
/// BOT worker thread; everything else is answered here.
unsafe extern "C" fn HwStorStartIo(
    device_extension: sp::PVOID,
    srb: sp::PSCSI_REQUEST_BLOCK,
) -> sp::BOOLEAN {
    if srb.is_null() {
        return 0;
    }
    let ext = device_extension.cast::<AdapterExtension>();
    // SAFETY: StorPort passes a live SRB and our extension.
    let status = unsafe {
        match u32::from((*srb).Function) {
            sp::SRB_FUNCTION_EXECUTE_SCSI => {
                if (*srb).PathId != 0 || (*srb).TargetId != 0 || (*srb).Lun != 0 {
                    sp::SRB_STATUS_NO_DEVICE
                } else {
                    match io::submit(ext, srb) {
                        io::Accept::Queued => return 1, // completed by the worker
                        io::Accept::Busy => sp::SRB_STATUS_BUSY,
                        io::Accept::NoDevice => sp::SRB_STATUS_NO_DEVICE,
                    }
                }
            }
            sp::SRB_FUNCTION_RESET_BUS
            | sp::SRB_FUNCTION_RESET_DEVICE
            | sp::SRB_FUNCTION_RESET_LOGICAL_UNIT
            | sp::SRB_FUNCTION_ABORT_COMMAND => {
                io::abort_all(ext, sp::SRB_STATUS_BUS_RESET);
                sp::SRB_STATUS_SUCCESS
            }
            sp::SRB_FUNCTION_FLUSH | sp::SRB_FUNCTION_SHUTDOWN | sp::SRB_FUNCTION_POWER => {
                sp::SRB_STATUS_SUCCESS
            }
            _ => sp::SRB_STATUS_INVALID_REQUEST,
        }
    };
    unsafe {
        (*srb).SrbStatus = status as u8;
        sp::StorPortNotification(
            sp::_SCSI_NOTIFICATION_TYPE::RequestComplete,
            device_extension,
            srb,
        );
    }
    1 // TRUE
}

/// Timeout recovery: fail everything outstanding with `SRB_STATUS_BUS_RESET`
/// (StorPort retries) and have the worker run BOT reset recovery.
unsafe extern "C" fn HwStorResetBus(
    device_extension: sp::PVOID,
    _path_id: sp::ULONG,
) -> sp::BOOLEAN {
    // SAFETY: our extension; lock initialized in HwStorFindAdapter.
    unsafe {
        io::abort_all(
            device_extension.cast::<AdapterExtension>(),
            sp::SRB_STATUS_BUS_RESET,
        )
    };
    1 // TRUE
}

unsafe extern "C" fn HwStorAdapterControl(
    device_extension: sp::PVOID,
    control_type: sp::SCSI_ADAPTER_CONTROL_TYPE,
    parameters: sp::PVOID,
) -> sp::SCSI_ADAPTER_CONTROL_STATUS {
    use sp::_SCSI_ADAPTER_CONTROL_TYPE as T;
    // StorPort miniports must support stop/restart (PnP). The work itself
    // (joining the worker, closing USB) happens in HwStorFreeAdapterResources
    // at PASSIVE_LEVEL; these run at DIRQL and must not block.
    if control_type == T::ScsiQuerySupportedControlTypes && !parameters.is_null() {
        // SAFETY: StorPort passes a SCSI_SUPPORTED_CONTROL_TYPE_LIST with
        // `MaxControlType` BOOLEAN slots in its flexible array.
        unsafe {
            let list = parameters.cast::<sp::SCSI_SUPPORTED_CONTROL_TYPE_LIST>();
            let max = (*list).MaxControlType as usize;
            let slots = (*list).SupportedTypeList.as_mut_ptr();
            for t in [
                T::ScsiQuerySupportedControlTypes,
                T::ScsiStopAdapter,
                T::ScsiRestartAdapter,
            ] {
                #[allow(clippy::cast_sign_loss)]
                let i = t as usize;
                if i < max {
                    *slots.add(i) = 1;
                }
            }
        }
    }
    if control_type == T::ScsiRestartAdapter {
        // Back from a low-power state: the worker re-initialises the
        // controller and re-checks the card before the next command.
        // SAFETY: our extension; an atomic store is fine at DIRQL.
        unsafe { io::note_restart(device_extension.cast::<AdapterExtension>()) };
    }
    sp::_SCSI_ADAPTER_CONTROL_STATUS::ScsiAdapterControlSuccess
}

unsafe extern "C" fn HwStorFreeAdapterResources(device_extension: sp::PVOID) {
    // `HwStorFreeAdapterResources` runs at PASSIVE_LEVEL, which is what joining
    // the worker thread and `USBD_CloseHandle` require.
    let ext = device_extension.cast::<AdapterExtension>();
    diag::set_u32(diag::w!("FreeStage"), 1);
    unsafe { io::stop_worker(ext) };
    diag::set_u32(diag::w!("FreeStage"), 2);
    // SAFETY: worker stopped, USB still open, PASSIVE_LEVEL.
    unsafe { io::power_down_slot(ext) };
    unsafe { usb::close(ext) };
    diag::set_u32(diag::w!("FreeStage"), 3);
}
