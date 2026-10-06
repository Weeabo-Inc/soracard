// Copyright (c) SoraCard
// SPDX-License-Identifier: MIT OR Apache-2.0

//! USB layer for the RTS5129 card reader.
//!
//! Two halves:
//!
//! * **Bring-up** (called from `HwStorFindAdapter`, PASSIVE_LEVEL): register
//!   with the USB driver stack (`USBD_CreateHandle`), read the configuration
//!   descriptor, select the configuration, capture the bulk pipe handles,
//!   probe for a Realtek RTCR controller, and start the I/O worker thread.
//! * **Transport primitives** used by the worker thread (PASSIVE_LEVEL): one
//!   synchronous bulk or ep0 control transfer, clear-halt on a pipe, and the
//!   Bulk-Only Mass Storage Reset class request.
//!
//! Every URB goes out in an IRP we allocate and own (`IoAllocateIrp` plus a
//! completion routine returning `STATUS_MORE_PROCESSING_REQUIRED`), so a
//! transfer can be timed out and cancelled safely: a wedged reader must never
//! hang the worker thread forever.

use core::ffi::c_void;

use storport_sys as sp;
use wdk_sys as wk;

use crate::diag::{self, w};
use crate::{chip, io, AdapterExtension};

/// Largest configuration descriptor we will fetch. The RTS5129 is a
/// single-interface, three-endpoint, vendor-class device, so its configuration
/// descriptor is ~39 bytes; this bound is deliberately generous.
pub const CONFIG_DESC_MAX: usize = 256;

/// `USBD_CLIENT_CONTRACT_VERSION_602` (`usbdlib.h`).
const USBD_CLIENT_CONTRACT_VERSION_602: u32 = 0x602;

/// `IOCTL_INTERNAL_USB_SUBMIT_URB`
///   = `CTL_CODE(FILE_DEVICE_USB, USB_SUBMIT_URB, METHOD_NEITHER, FILE_ANY_ACCESS)`
///   = `(0x22 << 16) | (0 << 14) | (0 << 2) | 3`.
///
/// Declared locally because it is built from the function-like `CTL_CODE`
/// macro, which bindgen does not materialize as a Rust constant.
const IOCTL_INTERNAL_USB_SUBMIT_URB: u32 = 0x0022_0003;

const STATUS_TIMEOUT: i32 = 0x0000_0102;
const STATUS_IO_TIMEOUT: i32 = 0xC000_00B5_u32 as i32;
const STATUS_MORE_PROCESSING_REQUIRED: i32 = 0xC000_0016_u32 as i32;
const STATUS_INSUFFICIENT_RESOURCES: i32 = 0xC000_009A_u32 as i32;
const STATUS_NOT_SUPPORTED: i32 = 0xC000_00BB_u32 as i32;
const STATUS_CANCELLED: i32 = 0xC000_0120_u32 as i32;
const STATUS_NO_SUCH_DEVICE: i32 = 0xC000_000E_u32 as i32;
const STATUS_DEVICE_NOT_CONNECTED: i32 = 0xC000_009D_u32 as i32;
const STATUS_DEVICE_DOES_NOT_EXIST: i32 = 0xC000_00C0_u32 as i32;

/// `USBD_STATUS_SUCCESS`.
const USBD_STATUS_SUCCESS: i32 = 0;
/// `USBD_STATUS_STALL_PID`: the endpoint answered with STALL.
const USBD_STATUS_STALL_PID: i32 = 0xC000_0004_u32 as i32;
/// `USBD_STATUS_DEVICE_GONE`.
const USBD_STATUS_DEVICE_GONE: i32 = 0xC000_7000_u32 as i32;

/// `SL_INVOKE_ON_SUCCESS | SL_INVOKE_ON_ERROR | SL_INVOKE_ON_CANCEL`.
const SL_INVOKE_ALWAYS: u8 = 0x40 | 0x80 | 0x20;

/// Pool tag `"Sora"`; `USBD_CreateHandle` rejects a zero tag.
const POOL_TAG: u32 = u32::from_le_bytes(*b"Sora");

/// USB endpoint direction mask (`USB_ENDPOINT_DIRECTION_MASK`).
const USB_ENDPOINT_DIRECTION_MASK: u8 = 0x80;

/// Number of bytes we first ask for to learn `wTotalLength`.
const CONFIG_DESC_HEADER_LEN: u32 = 9;

/// Timeout for bring-up control transfers.
const CONTROL_TIMEOUT_MS: u32 = 5_000;

/// Bulk-Only Mass Storage Reset (`bRequest` 0xFF, class, interface, OUT).
const BOT_RESET_REQUEST: u8 = 0xFF;

// ---- Diagnostic value names --------------------------------------------------

const D_STAGE: &[u16] = w!("UsbStage");
const D_STATUS: &[u16] = w!("UsbStatus");
const D_IFACE: &[u16] = w!("UsbIface");
const D_EP_IN: &[u16] = w!("UsbEpIn");
const D_EP_OUT: &[u16] = w!("UsbEpOut");
const D_CFG: &[u16] = w!("UsbConfigDesc");
const D_CFG_LEN: &[u16] = w!("UsbConfigDescRead");
const D_ARM: &[u16] = w!("Arm");
const D_ARMED_AT_START: &[u16] = w!("ArmedAtStart");
const D_ALLOW_WRITES: &[u16] = w!("AllowWrites");

/// Bring-up progress, recorded as `UsbStage` so a failure pinpoints its step.
#[derive(Clone, Copy)]
#[repr(u32)]
enum Stage {
    Entered = 1,
    Handle = 2,
    ConfigDescriptor = 3,
    SelectConfiguration = 4,
    WorkerUrb = 5,
    WorkerThread = 6,
    Ready = 7,
    /// `Parameters\\Arm` was 0: data path intentionally left off.
    Disarmed = 0xA0,
}

// ---- Transfer results --------------------------------------------------------

/// Result of one synchronous URB.
#[derive(Clone, Copy)]
pub struct Xfer {
    /// Final IRP `NTSTATUS`.
    pub nt: i32,
    /// URB header `USBD_STATUS`.
    pub usbd: i32,
    /// Bytes actually transferred.
    pub len: u32,
}

impl Xfer {
    #[must_use]
    pub fn ok(&self) -> bool {
        self.nt >= 0 && self.usbd == USBD_STATUS_SUCCESS
    }

    /// The endpoint stalled (a normal BOT event; clear it and continue).
    #[must_use]
    pub fn stalled(&self) -> bool {
        !self.ok() && self.usbd == USBD_STATUS_STALL_PID
    }

    /// The device has been unplugged / surprise-removed.
    #[must_use]
    pub fn gone(&self) -> bool {
        matches!(
            self.nt,
            STATUS_NO_SUCH_DEVICE | STATUS_DEVICE_NOT_CONNECTED | STATUS_DEVICE_DOES_NOT_EXIST
        ) || self.usbd == USBD_STATUS_DEVICE_GONE
    }

    const fn failed(nt: i32) -> Self {
        Self {
            nt,
            usbd: 0,
            len: 0,
        }
    }
}

// ---- Bring-up ----------------------------------------------------------------

/// Perform USB bring-up at PASSIVE_LEVEL (called from `HwStorFindAdapter`).
///
/// Returns `TRUE` when the data path is ready (or deliberately disarmed) and
/// `FALSE` otherwise. A `FALSE` return is not fatal to adapter load; the I/O
/// layer then fails every SCSI request with `SRB_STATUS_NO_DEVICE`.
///
/// # Safety
/// StorPort calls this with a valid device-extension pointer.
pub unsafe extern "C" fn passive_initialize(device_extension: sp::PVOID) -> sp::BOOLEAN {
    if device_extension.is_null() {
        return 0; // FALSE
    }
    let ext = device_extension.cast::<AdapterExtension>();

    // Idempotent: a restart can re-enter passive initialization.
    // SAFETY: StorPort allocated and zeroed the extension.
    if unsafe { !(*ext).usbd_handle.is_null() } {
        return 1; // TRUE
    }

    diag::init();
    diag::load_verbosity();
    diag::set_u32(
        w!("PassiveInitCalls"),
        diag::get_u32(w!("PassiveInitCalls"), 0) + 1,
    );

    // Safety interlock for driver development. Absent or `Arm=2`: armed (the
    // normal state). `Arm=0`: kill switch, the adapter loads but does no I/O.
    // `Arm=1`: armed for this start only; it is consumed here, so if anything
    // below crashes the machine the next boot comes up disarmed instead of
    // looping.
    let arm = diag::get_u32(D_ARM, 2);
    diag::set_u32(D_ARMED_AT_START, arm);
    if arm == 1 {
        diag::set_u32(D_ARM, 0);
    }
    if arm == 0 {
        diag::set_u32(D_STAGE, Stage::Disarmed as u32);
        // SAFETY: as below.
        unsafe { io::set_ready(ext, false) };
        return 1; // TRUE: the adapter loads; SCSI requests get NO_DEVICE
    }

    // Writable unless `AllowWrites=0`; a card's own lock switch still applies.
    let allow_writes = diag::get_u32(D_ALLOW_WRITES, 1) != 0;
    // SAFETY: the worker is not running yet.
    unsafe { io::set_allow_writes(ext, allow_writes) };

    // SAFETY: PASSIVE_LEVEL, valid extension, all failures handled inside.
    let ready = unsafe { bring_up(ext) };
    // SAFETY: as above; tells `HwStorStartIo` whether I/O can be accepted.
    unsafe { io::set_ready(ext, ready) };
    u8::from(ready)
}

/// Free the worker URB and close the USBD handle, if open.
///
/// Must run at PASSIVE_LEVEL (`USBD_CloseHandle`), after the worker thread has
/// stopped.
///
/// # Safety
/// `ext` must be the live adapter extension.
pub unsafe fn close(ext: *mut AdapterExtension) {
    // SAFETY: StorPort allocated and zeroed the extension; the worker thread
    // (the only other user of these fields) has been stopped by the caller.
    unsafe {
        let handle = (*ext).usbd_handle as wk::USBD_HANDLE;
        if handle.is_null() {
            return;
        }
        if !(*ext).urb.is_null() {
            wk::usb::USBD_UrbFree(handle, (*ext).urb as wk::PURB);
            (*ext).urb = core::ptr::null_mut();
        }
        wk::usb::USBD_CloseHandle(handle);
        (*ext).usbd_handle = core::ptr::null_mut();
    }
}

/// Best-effort cleanup after a partial bring-up; always returns `false`.
///
/// # Safety
/// `ext` must be the live adapter extension.
unsafe fn fail(ext: *mut AdapterExtension, stage: Stage, status: i32) -> bool {
    diag::set_u32(D_STAGE, stage as u32);
    #[allow(clippy::cast_sign_loss)]
    diag::set_u32(D_STATUS, status as u32);
    // SAFETY: `close` only touches USB state, which is valid while set.
    unsafe { close(ext) };
    false
}

/// Full PASSIVE_LEVEL bring-up. On success all USB state is stored in `ext`
/// and the worker thread is running.
///
/// # Safety
/// `ext` must be the live adapter extension, and the caller must be at
/// PASSIVE_LEVEL.
unsafe fn bring_up(ext: *mut AdapterExtension) -> bool {
    diag::set_u32(D_STAGE, Stage::Entered as u32);
    // SAFETY: StorPort allocated and zeroed the extension; nothing else runs
    // on it until `io::set_ready`.
    let (fdo, lower) = unsafe {
        (
            (*ext).fdo.cast::<wk::DEVICE_OBJECT>(),
            (*ext).lower.cast::<wk::DEVICE_OBJECT>(),
        )
    };
    if fdo.is_null() || lower.is_null() {
        return unsafe { fail(ext, Stage::Entered, STATUS_NO_SUCH_DEVICE) };
    }

    // (1) Register with the USB stack. `DeviceObject` is our FDO and
    // `TargetDeviceObject` is the next-lower device object (the same object
    // every URB IRP is sent to).
    let mut handle: wk::USBD_HANDLE = core::ptr::null_mut();
    // SAFETY: PASSIVE_LEVEL; objects from `StorPortGetDeviceObjects`.
    let status = unsafe {
        wk::usb::USBD_CreateHandle(
            fdo,
            lower,
            USBD_CLIENT_CONTRACT_VERSION_602,
            POOL_TAG,
            &raw mut handle,
        )
    };
    if !wk::NT_SUCCESS(status) || handle.is_null() {
        return unsafe { fail(ext, Stage::Handle, status) };
    }
    // SAFETY: see above.
    unsafe { (*ext).usbd_handle = handle.cast::<c_void>() };

    // (2) Read the full configuration descriptor into the extension buffer.
    // SAFETY: the extension buffer is nonpaged and CONFIG_DESC_MAX bytes long.
    let desc = unsafe { (*ext).config_desc.as_mut_ptr() };
    let total = match unsafe { read_config_descriptor(handle, lower, desc) } {
        Ok(total) => total,
        Err(nt) => return unsafe { fail(ext, Stage::ConfigDescriptor, nt) },
    };
    // SAFETY: see above.
    unsafe { (*ext).config_desc_len = total };
    // SAFETY: `total` bytes were just read into `desc`.
    diag::set_bin(D_CFG, unsafe {
        core::slice::from_raw_parts(desc, usize::from(total))
    });

    // (3) Select the configuration and harvest the bulk pipes.
    // SAFETY: PASSIVE_LEVEL; handle valid; descriptor fully populated.
    let sel = match unsafe { select_configuration(handle, lower, desc) } {
        Ok(sel) => sel,
        Err(nt) => return unsafe { fail(ext, Stage::SelectConfiguration, nt) },
    };
    // SAFETY: see above.
    unsafe {
        (*ext).config_handle = sel.configuration.cast::<c_void>();
        (*ext).bulk_in = sel.bulk_in.cast::<c_void>();
        (*ext).bulk_out = sel.bulk_out.cast::<c_void>();
        (*ext).interface_number = sel.interface_number;
    }
    diag::set_u32(D_IFACE, u32::from(sel.interface_number));
    diag::set_u32(D_EP_IN, sel.ep_in_info);
    diag::set_u32(D_EP_OUT, sel.ep_out_info);

    // (4) One URB, reused by the (serialized) worker for every transfer.
    let mut urb: wk::PURB = core::ptr::null_mut();
    // SAFETY: valid handle; `urb` is a valid out-pointer.
    let status = unsafe { wk::usb::USBD_UrbAllocate(handle, &raw mut urb) };
    if !wk::NT_SUCCESS(status) || urb.is_null() {
        return unsafe { fail(ext, Stage::WorkerUrb, status) };
    }
    // SAFETY: see above.
    unsafe { (*ext).urb = urb.cast::<c_void>() };

    // (5) Optional experiment hook: run Parameters\InitSeq (see run_init_seq).
    // SAFETY: PASSIVE_LEVEL; USB state complete; worker not yet running.
    unsafe { run_init_seq(ext) };

    // Realtek RTS51xx controllers have no SCSI firmware; if this one answers
    // the RTCR protocol, the driver becomes the SCSI target itself.
    // SAFETY: as above.
    if let Some(hw) = unsafe { chip::init(ext) } {
        diag::set_u32(w!("HwVersion"), u32::from(hw));
        unsafe { io::use_rtsx(ext) };
    }

    // (6) The worker thread that owns all USB traffic from here on.
    // SAFETY: PASSIVE_LEVEL; USB state is complete.
    let status = unsafe { io::start_worker(ext) };
    if !wk::NT_SUCCESS(status) {
        return unsafe { fail(ext, Stage::WorkerThread, status) };
    }

    diag::set_u32(D_STAGE, Stage::Ready as u32);
    diag::set_u32(D_STATUS, 0);
    true
}

/// Fetch the device's configuration descriptor (configuration + interface +
/// endpoint descriptors) with two synchronous `GET_DESCRIPTOR` control URBs:
/// first the 9-byte header to learn `wTotalLength`, then the whole descriptor.
///
/// # Safety
/// `handle` must be a live USBD handle, `lower` the target device object, and
/// `desc` must point at a writable nonpaged buffer of at least
/// [`CONFIG_DESC_MAX`] bytes.
unsafe fn read_config_descriptor(
    handle: wk::USBD_HANDLE,
    lower: *mut wk::DEVICE_OBJECT,
    desc: *mut u8,
) -> Result<u16, i32> {
    // First the 9-byte header, to learn `wTotalLength` (LE u16 at offset 2).
    // SAFETY: caller contract; `desc` holds at least CONFIG_DESC_MAX bytes.
    let got = unsafe { get_config_descriptor(handle, lower, desc, CONFIG_DESC_HEADER_LEN)? };
    if got < CONFIG_DESC_HEADER_LEN {
        return Err(STATUS_NOT_SUPPORTED);
    }
    // SAFETY: 9 header bytes were just written into `desc`.
    let total = u16::from_le_bytes([unsafe { *desc.add(2) }, unsafe { *desc.add(3) }]);
    if !(CONFIG_DESC_HEADER_LEN as usize..=CONFIG_DESC_MAX).contains(&usize::from(total)) {
        return Err(STATUS_NOT_SUPPORTED);
    }
    // Then the whole thing, with a fresh URB (reusing the completed URB left
    // the buffer untouched on this stack).
    let got = unsafe { get_config_descriptor(handle, lower, desc, u32::from(total))? };
    diag::set_u32(D_CFG_LEN, got);
    if got != u32::from(total) {
        return Err(STATUS_NOT_SUPPORTED);
    }
    Ok(total)
}

/// One `GET_DESCRIPTOR(CONFIGURATION, 0)` of `len` bytes into `desc`, on a
/// freshly allocated URB. Returns the number of bytes received.
///
/// # Safety
/// PASSIVE_LEVEL; live handle; `desc` writable for `len` bytes, nonpaged.
unsafe fn get_config_descriptor(
    handle: wk::USBD_HANDLE,
    lower: *mut wk::DEVICE_OBJECT,
    desc: *mut u8,
    len: u32,
) -> Result<u32, i32> {
    let mut urb: wk::PURB = core::ptr::null_mut();
    // SAFETY: PASSIVE_LEVEL; valid handle; `urb` is a valid out-pointer.
    let status = unsafe { wk::usb::USBD_UrbAllocate(handle, &raw mut urb) };
    if !wk::NT_SUCCESS(status) || urb.is_null() {
        return Err(status);
    }
    // SAFETY: `USBD_UrbAllocate` zeroed a full URB; this overlay fits in it.
    let req = urb.cast::<wk::_URB_CONTROL_DESCRIPTOR_REQUEST>();
    unsafe {
        (*req).Hdr.Function = wk::URB_FUNCTION_GET_DESCRIPTOR_FROM_DEVICE as u16;
        (*req).Hdr.Length = core::mem::size_of::<wk::_URB_CONTROL_DESCRIPTOR_REQUEST>() as u16;
        (*req).TransferBuffer = desc.cast::<c_void>();
        (*req).TransferBufferLength = len;
        (*req).DescriptorType = wk::USB_CONFIGURATION_DESCRIPTOR_TYPE as u8;
    }
    // SAFETY: fully initialized; synchronous at PASSIVE_LEVEL.
    let x = unsafe {
        submit(
            handle,
            lower,
            urb,
            CONTROL_TIMEOUT_MS,
            core::ptr::null_mut(),
        )
    };
    // SAFETY: the request is complete; read the result, then free exactly once.
    let got = unsafe { (*req).TransferBufferLength };
    unsafe { wk::usb::USBD_UrbFree(handle, urb) };
    if x.ok() {
        Ok(got)
    } else {
        Err(if x.nt < 0 { x.nt } else { x.usbd })
    }
}

/// What [`select_configuration`] harvests.
struct Selected {
    configuration: wk::USBD_CONFIGURATION_HANDLE,
    bulk_in: wk::USBD_PIPE_HANDLE,
    bulk_out: wk::USBD_PIPE_HANDLE,
    interface_number: u8,
    /// `(EndpointAddress << 16) | MaximumPacketSize` for diagnostics.
    ep_in_info: u32,
    ep_out_info: u32,
}

/// Select the configuration for the first interface and return the
/// configuration handle plus the bulk IN and bulk OUT pipe handles.
///
/// # Safety
/// `handle` must be a live USBD handle, `lower` the target device object, and
/// `desc` must point at a complete configuration descriptor.
unsafe fn select_configuration(
    handle: wk::USBD_HANDLE,
    lower: *mut wk::DEVICE_OBJECT,
    desc: *mut u8,
) -> Result<Selected, i32> {
    let config = desc.cast::<wk::USB_CONFIGURATION_DESCRIPTOR>();

    // Locate the first interface descriptor by walking the configuration
    // descriptor (`USBD_ParseConfigurationDescriptorEx` is not exported by
    // `usbdex`, so we parse manually).
    // SAFETY: `desc`/`config` point at the descriptor we read, and the walk is
    // bounded by `bLength`/`wTotalLength`.
    let iface = unsafe {
        let total = usize::from((*config).wTotalLength);
        let mut off = usize::from((*config).bLength);
        let mut found: *mut wk::USB_INTERFACE_DESCRIPTOR = core::ptr::null_mut();
        while off + 2 <= total {
            let b_len = usize::from(*desc.add(off));
            let b_type = *desc.add(off + 1);
            if b_len == 0 {
                break;
            }
            if b_type == 4 {
                // USB_INTERFACE_DESCRIPTOR_TYPE
                found = desc.add(off).cast::<wk::USB_INTERFACE_DESCRIPTOR>();
                break;
            }
            off += b_len;
        }
        found
    };
    if iface.is_null() {
        return Err(STATUS_NOT_SUPPORTED);
    }
    // SAFETY: `iface` points at a complete interface descriptor.
    let interface_number = unsafe { (*iface).bInterfaceNumber };

    // One interface entry plus the mandatory NULL terminator entry.
    // SAFETY: plain data; zero is a valid terminator value.
    let mut list: [wk::USBD_INTERFACE_LIST_ENTRY; 2] = unsafe { core::mem::zeroed() };
    list[0].InterfaceDescriptor = iface;

    let mut urb: wk::PURB = core::ptr::null_mut();
    // SAFETY: PASSIVE_LEVEL; valid handle; list is NULL-terminated; `urb` is a
    // valid out-pointer. The helper allocates and fills the select-config URB.
    let status = unsafe {
        wk::usb::USBD_SelectConfigUrbAllocateAndBuild(
            handle,
            config,
            list.as_mut_ptr(),
            &raw mut urb,
        )
    };
    if !wk::NT_SUCCESS(status) || urb.is_null() {
        return Err(status);
    }

    // SAFETY: URB built by USBD; synchronous send at PASSIVE_LEVEL.
    let x = unsafe {
        submit(
            handle,
            lower,
            urb,
            CONTROL_TIMEOUT_MS,
            core::ptr::null_mut(),
        )
    };
    if !x.ok() {
        // SAFETY: `urb` came from USBD; freed exactly once.
        unsafe { wk::usb::USBD_UrbFree(handle, urb) };
        return Err(if x.nt < 0 { x.nt } else { x.usbd });
    }

    // `list[0].Interface` points *into* the URB; harvest everything before
    // `USBD_UrbFree` invalidates it.
    let iface_info = list[0].Interface;
    if iface_info.is_null() {
        unsafe { wk::usb::USBD_UrbFree(handle, urb) };
        return Err(STATUS_NOT_SUPPORTED);
    }

    let mut sel = Selected {
        // SAFETY: `urb` is a select-configuration URB.
        configuration: unsafe {
            (*urb.cast::<wk::_URB_SELECT_CONFIGURATION>()).ConfigurationHandle
        },
        bulk_in: core::ptr::null_mut(),
        bulk_out: core::ptr::null_mut(),
        interface_number,
        ep_in_info: 0,
        ep_out_info: 0,
    };
    // SAFETY: `iface_info` points into the live URB.
    let pipe_count = unsafe { (*iface_info).NumberOfPipes };
    for i in 0..pipe_count {
        // SAFETY: `Pipes` is a flexible array of `NumberOfPipes` entries.
        let pipe = unsafe { (*iface_info).Pipes.as_ptr().add(i as usize) };
        // SAFETY: `pipe` points at a valid `USBD_PIPE_INFORMATION`.
        let (pipe_type, endpoint, mps, pipe_handle) = unsafe {
            (
                (*pipe).PipeType,
                (*pipe).EndpointAddress,
                (*pipe).MaximumPacketSize,
                (*pipe).PipeHandle,
            )
        };
        if pipe_type == wk::_USBD_PIPE_TYPE::UsbdPipeTypeBulk {
            let info = (u32::from(endpoint) << 16) | u32::from(mps);
            if endpoint & USB_ENDPOINT_DIRECTION_MASK != 0 {
                sel.bulk_in = pipe_handle;
                sel.ep_in_info = info;
            } else {
                sel.bulk_out = pipe_handle;
                sel.ep_out_info = info;
            }
        }
    }

    // SAFETY: all information has been copied out; free the URB.
    unsafe { wk::usb::USBD_UrbFree(handle, urb) };

    if sel.bulk_in.is_null() || sel.bulk_out.is_null() {
        return Err(STATUS_NOT_SUPPORTED);
    }
    Ok(sel)
}

// ---- Transport primitives (worker thread, PASSIVE_LEVEL) ---------------------

/// One synchronous bulk transfer on `pipe` using the worker's URB.
///
/// `dir_in` transfers device → host into `buf`; otherwise host → device from
/// `buf`. Short IN transfers are allowed and reported through `len`.
///
/// # Safety
/// PASSIVE_LEVEL, worker thread only (it owns `ext.urb`); `buf` must be
/// nonpaged and valid for `len` bytes.
pub unsafe fn bulk(
    ext: *mut AdapterExtension,
    pipe: sp::PVOID,
    buf: *mut u8,
    len: u32,
    dir_in: bool,
    timeout_ms: u32,
) -> Xfer {
    // SAFETY: the worker is the only user of `urb` once bring-up is done.
    let (handle, lower, urb) = unsafe { worker_urb(ext) };
    let req = urb.cast::<wk::_URB_BULK_OR_INTERRUPT_TRANSFER>();
    // SAFETY: the URB allocation is at least `size_of::<URB>()` bytes, which
    // covers this overlay; zeroing resets `hca`/`UrbLink` for reuse.
    unsafe {
        core::ptr::write_bytes(req, 0, 1);
        (*req).Hdr.Function = wk::URB_FUNCTION_BULK_OR_INTERRUPT_TRANSFER as u16;
        (*req).Hdr.Length = core::mem::size_of::<wk::_URB_BULK_OR_INTERRUPT_TRANSFER>() as u16;
        (*req).PipeHandle = pipe as wk::USBD_PIPE_HANDLE;
        (*req).TransferFlags = if dir_in {
            wk::USBD_TRANSFER_DIRECTION_IN | wk::USBD_SHORT_TRANSFER_OK
        } else {
            0
        };
        (*req).TransferBufferLength = len;
        (*req).TransferBuffer = buf.cast::<c_void>();
    }
    // SAFETY: fully built URB; PASSIVE_LEVEL; the I/O gate lives in `ext`.
    let mut x = unsafe { submit(handle, lower, urb, timeout_ms, io::gate(ext)) };
    // SAFETY: the transfer is complete; the URB is ours again.
    x.len = unsafe { (*req).TransferBufferLength };
    if !x.ok() && x.usbd != USBD_STATUS_STALL_PID {
        // Only a stall leaves a meaningful partial count.
        x.len = 0;
    }
    x
}

/// `URB_FUNCTION_SYNC_RESET_PIPE_AND_CLEAR_STALL` on `pipe` (clears a halt and
/// resets the data toggle).
///
/// # Safety
/// As for [`bulk`].
pub unsafe fn reset_pipe(ext: *mut AdapterExtension, pipe: sp::PVOID, timeout_ms: u32) -> Xfer {
    // SAFETY: as for `bulk`.
    let (handle, lower, urb) = unsafe { worker_urb(ext) };
    let req = urb.cast::<wk::_URB_PIPE_REQUEST>();
    // SAFETY: as for `bulk`.
    unsafe {
        core::ptr::write_bytes(req, 0, 1);
        (*req).Hdr.Function = wk::URB_FUNCTION_SYNC_RESET_PIPE_AND_CLEAR_STALL as u16;
        (*req).Hdr.Length = core::mem::size_of::<wk::_URB_PIPE_REQUEST>() as u16;
        (*req).PipeHandle = pipe as wk::USBD_PIPE_HANDLE;
    }
    // SAFETY: as for `bulk`.
    unsafe { submit(handle, lower, urb, timeout_ms, io::gate(ext)) }
}

/// Bulk-Only Mass Storage Reset: class request 0xFF to the interface
/// (BOT rev 1.0 §3.1). Readies the device for the next CBW after a phase
/// error; the caller must then clear both bulk pipes.
///
/// # Safety
/// As for [`bulk`].
pub unsafe fn mass_storage_reset(ext: *mut AdapterExtension, timeout_ms: u32) -> Xfer {
    // SAFETY: as for `bulk`.
    let (handle, lower, urb) = unsafe { worker_urb(ext) };
    let req = urb.cast::<wk::_URB_CONTROL_VENDOR_OR_CLASS_REQUEST>();
    // SAFETY: as for `bulk`; `interface_number` is fixed after bring-up.
    unsafe {
        core::ptr::write_bytes(req, 0, 1);
        (*req).Hdr.Function = wk::URB_FUNCTION_CLASS_INTERFACE as u16;
        (*req).Hdr.Length = core::mem::size_of::<wk::_URB_CONTROL_VENDOR_OR_CLASS_REQUEST>() as u16;
        (*req).TransferFlags = 0; // host -> device, no data stage
        (*req).Request = BOT_RESET_REQUEST;
        (*req).Value = 0;
        (*req).Index = u16::from((*ext).interface_number);
    }
    // SAFETY: as for `bulk`.
    unsafe { submit(handle, lower, urb, timeout_ms, io::gate(ext)) }
}

/// The worker's USB handles.
///
/// # Safety
/// `ext` must be the live adapter extension after a successful bring-up.
unsafe fn worker_urb(
    ext: *mut AdapterExtension,
) -> (wk::USBD_HANDLE, *mut wk::DEVICE_OBJECT, wk::PURB) {
    // SAFETY: fields are written once during bring-up, before the worker runs.
    unsafe {
        (
            (*ext).usbd_handle as wk::USBD_HANDLE,
            (*ext).lower.cast::<wk::DEVICE_OBJECT>(),
            (*ext).urb as wk::PURB,
        )
    }
}

/// One control transfer on the default pipe with a raw 8-byte setup packet.
/// Direction comes from bit 7 of `bmRequestType`; `buf` holds `wLength` bytes.
///
/// # Safety
/// PASSIVE_LEVEL, worker context (uses `ext.urb`); `buf` nonpaged, valid for
/// `wLength` bytes (may be null when `wLength == 0`).
pub unsafe fn control(
    ext: *mut AdapterExtension,
    setup: [u8; 8],
    buf: *mut u8,
    timeout_ms: u32,
) -> Xfer {
    let (handle, lower, urb) = unsafe { worker_urb(ext) };
    let req = urb.cast::<wk::_URB_CONTROL_TRANSFER>();
    let len = u32::from(u16::from_le_bytes([setup[6], setup[7]]));
    let dir_in = setup[0] & 0x80 != 0;
    // SAFETY: the URB allocation covers this overlay; zeroed for reuse.
    unsafe {
        core::ptr::write_bytes(req, 0, 1);
        (*req).Hdr.Function = wk::URB_FUNCTION_CONTROL_TRANSFER as u16;
        (*req).Hdr.Length = core::mem::size_of::<wk::_URB_CONTROL_TRANSFER>() as u16;
        (*req).TransferFlags = wk::USBD_DEFAULT_PIPE_TRANSFER
            | if dir_in {
                wk::USBD_TRANSFER_DIRECTION_IN | wk::USBD_SHORT_TRANSFER_OK
            } else {
                0
            };
        (*req).TransferBufferLength = len;
        (*req).TransferBuffer = buf.cast::<c_void>();
        (*req).SetupPacket = setup;
    }
    let mut x = unsafe { submit(handle, lower, urb, timeout_ms, io::gate(ext)) };
    x.len = unsafe { (*req).TransferBufferLength };
    x
}

/// Experiment hook: execute `Parameters\InitSeq` (REG_BINARY) at bring-up so
/// vendor init sequences can be tried without rebuilding the driver.
///
/// Records, back to back:
/// * control transfer: 8-byte setup packet, then `wLength` data bytes if OUT;
/// * `FF nn`: delay `nn` x 10 ms;
/// * `FE ll hh` + data: bulk OUT of `hhll` bytes on the bulk OUT pipe;
/// * `FD ll hh`: bulk IN of up to `hhll` bytes on the bulk IN pipe.
///
/// Every step is logged to `InitLog` as `[kind, idx, 0, 0, nt, usbd, len]`
/// and IN data is appended to `InitData` (up to 1 KiB).
///
/// # Safety
/// PASSIVE_LEVEL; USB bring-up complete; worker not running.
unsafe fn run_init_seq(ext: *mut AdapterExtension) {
    let mut seq = [0u8; 1024];
    let n = diag::get_bin(w!("InitSeq"), &mut seq);
    if n == 0 {
        return;
    }
    // Scratch I/O buffer: reuse the (idle) worker bounce buffer, nonpaged.
    let buf = unsafe { io::bounce_ptr(ext) };
    let mut log = [0u8; 16 * 48];
    let mut logged = 0usize;
    let mut data = [0u8; 1024];
    let mut data_len = 0usize;
    let mut i = 0usize;
    let mut idx = 0u8;
    while i < n && logged < 48 {
        let kind = seq[i];
        let x = match kind {
            0xFF if i + 1 < n => {
                let ms = i64::from(seq[i + 1]) * 10;
                let mut t = wk::LARGE_INTEGER {
                    QuadPart: -ms * 10_000,
                };
                let _ = unsafe {
                    wk::ntddk::KeDelayExecutionThread(
                        wk::_MODE::KernelMode as wk::KPROCESSOR_MODE,
                        0,
                        &raw mut t,
                    )
                };
                i += 2;
                Xfer {
                    nt: 0,
                    usbd: 0,
                    len: 0,
                }
            }
            0xFE | 0xFD if i + 2 < n => {
                let len =
                    usize::from(u16::from_le_bytes([seq[i + 1], seq[i + 2]])).min(io::BOUNCE_LEN);
                i += 3;
                let out = kind == 0xFE;
                if out {
                    let m = len.min(n - i);
                    unsafe { core::ptr::copy_nonoverlapping(seq.as_ptr().add(i), buf, m) };
                    i += m;
                }
                let pipe = unsafe {
                    if out {
                        (*ext).bulk_out
                    } else {
                        (*ext).bulk_in
                    }
                };
                #[allow(clippy::cast_possible_truncation)]
                let x = unsafe { bulk(ext, pipe, buf, len as u32, !out, 3_000) };
                if !out {
                    let m = (x.len as usize).min(data.len() - data_len);
                    unsafe {
                        core::ptr::copy_nonoverlapping(buf, data.as_mut_ptr().add(data_len), m)
                    };
                    data_len += m;
                }
                x
            }
            _ if i + 8 <= n => {
                let mut setup = [0u8; 8];
                setup.copy_from_slice(&seq[i..i + 8]);
                i += 8;
                let len = usize::from(u16::from_le_bytes([setup[6], setup[7]])).min(io::BOUNCE_LEN);
                let dir_in = setup[0] & 0x80 != 0;
                if !dir_in {
                    let m = len.min(n - i);
                    unsafe { core::ptr::copy_nonoverlapping(seq.as_ptr().add(i), buf, m) };
                    i += m;
                }
                let x = unsafe { control(ext, setup, buf, 3_000) };
                if dir_in {
                    let m = (x.len as usize).min(data.len() - data_len);
                    unsafe {
                        core::ptr::copy_nonoverlapping(buf, data.as_mut_ptr().add(data_len), m)
                    };
                    data_len += m;
                }
                x
            }
            _ => break,
        };
        let e = &mut log[logged * 16..logged * 16 + 16];
        e[0] = kind;
        e[1] = idx;
        e[4..8].copy_from_slice(&x.nt.to_le_bytes());
        e[8..12].copy_from_slice(&x.usbd.to_le_bytes());
        e[12..16].copy_from_slice(&x.len.to_le_bytes());
        logged += 1;
        idx = idx.wrapping_add(1);
    }
    diag::set_bin(w!("InitLog"), &log[..logged * 16]);
    diag::set_bin(w!("InitData"), &data[..data_len]);
}

// ---- IRP plumbing ------------------------------------------------------------

/// Completion routine: wake the submitter and keep ownership of the IRP.
unsafe extern "C" fn urb_complete(
    _device: wk::PDEVICE_OBJECT,
    _irp: wk::PIRP,
    context: wk::PVOID,
) -> wk::NTSTATUS {
    // SAFETY: `context` is the submitter's KEVENT, which outlives the IRP
    // because the submitter always waits for this signal before returning.
    let _ = unsafe { wk::ntddk::KeSetEvent(context.cast::<wk::KEVENT>(), 0, 0) };
    STATUS_MORE_PROCESSING_REQUIRED
}

/// Synchronously submit a `USBD_*`-allocated URB with a timeout.
///
/// If `gate` is non-null the IRP is published there while in flight so that
/// [`io::abort_all`] can cancel it, and the submit is refused (with
/// `STATUS_CANCELLED`) when a reset is pending.
///
/// On timeout the IRP is cancelled and we still wait for its completion, so
/// the URB and buffers are never touched by the USB stack after we return.
///
/// # Safety
/// PASSIVE_LEVEL (it blocks); `lower` must be the target device object and
/// `urb` a live URB from a `USBD_*` allocation routine.
unsafe fn submit(
    handle: wk::USBD_HANDLE,
    lower: *mut wk::DEVICE_OBJECT,
    urb: wk::PURB,
    timeout_ms: u32,
    gate: *mut io::Gate,
) -> Xfer {
    // SAFETY: `lower` is a valid device object.
    let stack_size = unsafe { (*lower).StackSize };
    // SAFETY: PASSIVE_LEVEL.
    let irp = unsafe { wk::ntddk::IoAllocateIrp(stack_size, 0) };
    if irp.is_null() {
        return Xfer::failed(STATUS_INSUFFICIENT_RESOURCES);
    }

    // SAFETY: plain data; initialized before use.
    let mut event: wk::KEVENT = unsafe { core::mem::zeroed() };
    unsafe {
        wk::ntddk::KeInitializeEvent(&raw mut event, wk::_EVENT_TYPE::NotificationEvent, 0);
    }

    // SAFETY: `irp` is freshly allocated with `stack_size` locations; the next
    // location is the lower driver's.
    unsafe {
        (*irp).IoStatus.__bindgen_anon_1.Status = STATUS_NOT_SUPPORTED;
        let next = next_stack_location(irp);
        (*next).MajorFunction = wk::IRP_MJ_INTERNAL_DEVICE_CONTROL as u8;
        (*next).Parameters.DeviceIoControl.IoControlCode = IOCTL_INTERNAL_USB_SUBMIT_URB;
        // Required for USBD-allocated URBs (instead of setting Argument1).
        wk::usb::USBD_AssignUrbToIoStackLocation(handle, next, urb);
        (*next).CompletionRoutine = Some(urb_complete);
        (*next).Context = (&raw mut event).cast::<c_void>();
        (*next).Control = SL_INVOKE_ALWAYS;
    }

    // Publish for cancellation, unless a reset already wants the pipe idle.
    // SAFETY: `gate` is null or points into the live adapter extension.
    if !gate.is_null() && !unsafe { io::gate_enter(gate, irp) } {
        unsafe { wk::ntddk::IoFreeIrp(irp) };
        return Xfer::failed(STATUS_CANCELLED);
    }

    // SAFETY: `lower` is valid and `irp` fully prepared.
    let _ = unsafe { wk::ntddk::IofCallDriver(lower, irp) };

    let mut timeout = wk::LARGE_INTEGER {
        QuadPart: -(i64::from(timeout_ms) * 10_000),
    };
    // SAFETY: PASSIVE_LEVEL; `event` outlives the wait.
    let waited = unsafe {
        wk::ntddk::KeWaitForSingleObject(
            (&raw mut event).cast::<c_void>(),
            wk::_KWAIT_REASON::Executive,
            wk::_MODE::KernelMode as wk::KPROCESSOR_MODE,
            0,
            &raw mut timeout,
        )
    };
    if waited == STATUS_TIMEOUT {
        // SAFETY: we still own the IRP (completion returns
        // MORE_PROCESSING_REQUIRED), so cancelling it is always safe.
        unsafe {
            let _ = wk::ntddk::IoCancelIrp(irp);
            // The IRP references `event` on this stack frame, so we must not
            // return before it completes. Keep waiting, but re-cancel and
            // record it every 5 s so a stuck transfer is visible.
            let mut stuck = 0u32;
            loop {
                let mut t = wk::LARGE_INTEGER {
                    QuadPart: -50_000_000,
                };
                let w = wk::ntddk::KeWaitForSingleObject(
                    (&raw mut event).cast::<c_void>(),
                    wk::_KWAIT_REASON::Executive,
                    wk::_MODE::KernelMode as wk::KPROCESSOR_MODE,
                    0,
                    &raw mut t,
                );
                if w != STATUS_TIMEOUT {
                    break;
                }
                stuck += 5;
                diag::set_u32(w!("IrpStuckSec"), stuck);
                let _ = wk::ntddk::IoCancelIrp(irp);
            }
        }
    }

    if !gate.is_null() {
        // SAFETY: as above; must happen before the IRP is freed.
        unsafe { io::gate_leave(gate) };
    }

    // SAFETY: the IRP has completed and is ours.
    let mut nt = unsafe { (*irp).IoStatus.__bindgen_anon_1.Status };
    unsafe { wk::ntddk::IoFreeIrp(irp) };
    if waited == STATUS_TIMEOUT {
        // The cancelled IRP usually reports STATUS_CANCELLED; say what happened.
        nt = STATUS_IO_TIMEOUT;
    }
    // SAFETY: every URB starts with its header.
    let usbd = unsafe { (*urb.cast::<wk::_URB_HEADER>()).Status };
    Xfer { nt, usbd, len: 0 }
}

/// `IoGetNextIrpStackLocation(irp)`.
///
/// `IoGetNextIrpStackLocation` is a macro (`Irp->Tail.Overlay...`), so we
/// reach into the IRP tail union directly.
///
/// # Safety
/// `irp` must be a valid IRP.
unsafe fn next_stack_location(irp: *mut wk::IRP) -> *mut wk::_IO_STACK_LOCATION {
    // SAFETY: valid IRP.
    let current = unsafe {
        (*irp)
            .Tail
            .Overlay
            .__bindgen_anon_2
            .__bindgen_anon_1
            .CurrentStackLocation
    };
    // SAFETY: the next stack location is one slot below the current one.
    unsafe { current.sub(1) }
}
