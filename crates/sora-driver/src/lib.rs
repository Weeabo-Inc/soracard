//! # soracard — KMDF function driver for USB SD card readers (SUPERSEDED).
//!
//! **Superseded by the StorPort miniport in `storport/`, which is the working
//! driver.** This KMDF bus-driver attempt had to emulate a SCSI port driver
//! and crashed in `classpnp`'s CLAIM_DEVICE handling; it is kept for
//! reference only. See docs/ARCHITECTURE.md §2.
//!
//! Vendor drivers (Realtek and friends) mishandle media change: inserts are
//! never reported and removals take minutes. This driver replaces them with a
//! clean Bulk-Only-Transport + SCSI implementation that reports card presence
//! changes promptly through PnP.
//!
//! Layout:
//! * `driver` — `DriverEntry` and the KMDF driver object
//! * `device` — device creation and the PnP/power lifecycle
//! * `usb`    — USB target configuration and pipe management
//! * `scsi`   — SRB handling that maps onto `sora_core` (never finished)
//!
//! Protocol logic lives in the host-testable `sora-core` crate.

#![no_std]
#![deny(clippy::all)]
#![warn(clippy::pedantic)]
#![allow(clippy::missing_safety_doc)]

mod bus;
mod device;
mod diag;
mod driver;
mod srb;
mod storage;
mod usb;
mod wdf_object_context;

extern crate alloc;

extern crate wdk_panic;

use wdk_alloc::WdkAllocator;

#[global_allocator]
static GLOBAL_ALLOCATOR: WdkAllocator = WdkAllocator;

use wdf_object_context::wdf_declare_context_type;
use wdk_sys::{
    ULONG, WDFCHILDLIST, WDFUSBDEVICE, WDFUSBINTERFACE, WDFUSBPIPE, WDF_DRIVER_CONFIG,
    WDF_OBJECT_ATTRIBUTES, WDF_OBJECT_CONTEXT_TYPE_INFO, WDF_PNPPOWER_EVENT_CALLBACKS,
};

/// Compute a `WDF_*` structure's `Size` field at const time, with an assert
/// that it actually fits in a `ULONG`.
macro_rules! wdf_size {
    ($name:ident, $t:ty) => {
        #[allow(clippy::cast_possible_truncation)]
        pub const $name: ULONG = {
            const S: usize = core::mem::size_of::<$t>();
            const {
                assert!(S <= ULONG::MAX as usize, "WDF struct exceeds ULONG");
            }
            S as ULONG
        };
    };
}

wdf_size!(WDF_DRIVER_CONFIG_SIZE, WDF_DRIVER_CONFIG);
wdf_size!(WDF_OBJECT_ATTRIBUTES_SIZE, WDF_OBJECT_ATTRIBUTES);
wdf_size!(
    WDF_PNPPOWER_EVENT_CALLBACKS_SIZE,
    WDF_PNPPOWER_EVENT_CALLBACKS
);
wdf_size!(
    WDF_OBJECT_CONTEXT_TYPE_INFO_SIZE,
    WDF_OBJECT_CONTEXT_TYPE_INFO
);

/// Per-device state. Kept in the WDF device object's context so there are no
/// globals and no locking surprises across device instances.
pub struct DeviceContext {
    /// The KMDF USB target for this reader instance.
    pub usb_target: WDFUSBDEVICE,
    /// The single configured interface.
    pub usb_interface: WDFUSBINTERFACE,
    /// Bulk-IN pipe (device -> host).
    pub bulk_in: WDFUSBPIPE,
    /// Bulk-OUT pipe (host -> device).
    pub bulk_out: WDFUSBPIPE,
    /// Bus child list (our LUNs).
    pub child_list: WDFCHILDLIST,
}
wdf_declare_context_type!(DeviceContext);

/// Human-readable driver identity and version tag for logs.
pub const DRIVER_TAG: &str = "SoraCard";
pub const DRIVER_VERSION: &str = env!("CARGO_PKG_VERSION");
