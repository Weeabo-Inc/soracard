// Copyright (c) SoraCard
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Bring-up diagnostics written to
//! `HKLM\SYSTEM\CurrentControlSet\Services\SoraCard\Parameters`.
//!
//! There is no kernel debugger on the test machine and DebugView cannot
//! capture kernel output there, so progress and per-command results are
//! recorded as registry values readable with `reg query`. Every function here
//! is best-effort and **PASSIVE_LEVEL only** (`RtlWriteRegistryValue`).

use core::ffi::c_void;
use core::sync::atomic::{AtomicBool, Ordering};

use wdk_sys as wk;

/// NUL-terminated UTF-16 literal, built at compile time.
macro_rules! w {
    ($s:literal) => {{
        const S: &str = $s;
        const N: usize = S.len() + 1;
        const W: [u16; N] = {
            let b = S.as_bytes();
            let mut o = [0u16; N];
            let mut i = 0;
            while i < b.len() {
                o[i] = b[i] as u16;
                i += 1;
            }
            o
        };
        &W
    }};
}
pub(crate) use w;

const PATH: &[u16] = w!("SoraCard\\Parameters");

/// `Parameters\Trace` != 0: record every command, poll and init step (the
/// bring-up trace). Off by default so steady-state I/O does not write the
/// registry.
static VERBOSE: AtomicBool = AtomicBool::new(false);

/// Read `Parameters\Trace` (PASSIVE_LEVEL, once per adapter start).
pub fn load_verbosity() {
    VERBOSE.store(get_u32(w!("Trace"), 0) != 0, Ordering::Relaxed);
}

/// Whether the detailed trace is on.
pub fn verbose() -> bool {
    VERBOSE.load(Ordering::Relaxed)
}

/// Create the Parameters key if needed. Call once before any `set_*`.
pub fn init() {
    // SAFETY: PASSIVE_LEVEL; `PATH` is a static NUL-terminated string. The API
    // takes `PWSTR` but does not write through it.
    let _ = unsafe {
        wk::ntddk::RtlCreateRegistryKey(wk::RTL_REGISTRY_SERVICES, PATH.as_ptr().cast_mut())
    };
}

fn write(name: &[u16], ty: u32, data: *const c_void, len: u32) {
    // SAFETY: PASSIVE_LEVEL; static NUL-terminated strings; `data` is valid
    // for `len` bytes for the duration of the call.
    let _ = unsafe {
        wk::ntddk::RtlWriteRegistryValue(
            wk::RTL_REGISTRY_SERVICES,
            PATH.as_ptr(),
            name.as_ptr(),
            ty,
            data.cast_mut(),
            len,
        )
    };
}

/// Record a `REG_DWORD`.
pub fn set_u32(name: &[u16], value: u32) {
    write(name, wk::REG_DWORD, (&raw const value).cast(), 4);
}

/// Record a `REG_BINARY` blob.
pub fn set_bin(name: &[u16], bytes: &[u8]) {
    #[allow(clippy::cast_possible_truncation)]
    write(
        name,
        wk::REG_BINARY,
        bytes.as_ptr().cast(),
        bytes.len() as u32,
    );
}

/// Read a `REG_DWORD` from the Parameters key; `default` if absent or mistyped.
pub fn get_u32(name: &[u16], default: u32) -> u32 {
    let mut value = default;
    let mut fallback = default;
    // SAFETY: plain data; a zeroed entry is the table terminator.
    let mut table: [wk::RTL_QUERY_REGISTRY_TABLE; 2] = unsafe { core::mem::zeroed() };
    table[0].Flags = wk::RTL_QUERY_REGISTRY_DIRECT | wk::RTL_QUERY_REGISTRY_TYPECHECK;
    table[0].Name = name.as_ptr().cast_mut();
    table[0].EntryContext = (&raw mut value).cast();
    table[0].DefaultType =
        (wk::REG_DWORD << wk::RTL_QUERY_REGISTRY_TYPECHECK_SHIFT) | wk::REG_DWORD;
    table[0].DefaultData = (&raw mut fallback).cast();
    table[0].DefaultLength = 4;
    // SAFETY: PASSIVE_LEVEL; DIRECT + TYPECHECK(REG_DWORD) writes exactly 4
    // bytes into `value`; the table is terminated.
    let status = unsafe {
        wk::ntddk::RtlQueryRegistryValues(
            wk::RTL_REGISTRY_SERVICES | wk::RTL_REGISTRY_OPTIONAL,
            PATH.as_ptr(),
            table.as_mut_ptr(),
            core::ptr::null_mut(),
            core::ptr::null_mut(),
        )
    };
    if wk::NT_SUCCESS(status) {
        value
    } else {
        default
    }
}

/// Read a `REG_BINARY` value from the Parameters key into `out`; returns the
/// number of bytes copied (0 if absent, mistyped or on any error).
pub fn get_bin(name: &[u16], out: &mut [u8]) -> usize {
    // Full path: \Registry\Machine\System\CurrentControlSet\Services\SoraCard\Parameters
    let key_path =
        w!("\\Registry\\Machine\\System\\CurrentControlSet\\Services\\SoraCard\\Parameters");
    let mut key_name = ustr(key_path);
    // SAFETY: plain data.
    let mut oa: wk::OBJECT_ATTRIBUTES = unsafe { core::mem::zeroed() };
    #[allow(clippy::cast_possible_truncation)]
    {
        oa.Length = core::mem::size_of::<wk::OBJECT_ATTRIBUTES>() as u32;
    }
    oa.ObjectName = &raw mut key_name;
    oa.Attributes = wk::OBJ_KERNEL_HANDLE | wk::OBJ_CASE_INSENSITIVE;
    let mut key: wk::HANDLE = core::ptr::null_mut();
    // SAFETY: PASSIVE_LEVEL; valid attributes and out-pointer.
    if unsafe { wk::ntddk::ZwOpenKey(&raw mut key, wk::KEY_READ, &raw mut oa) } < 0 {
        return 0;
    }
    let mut value_name = ustr(name);
    // KEY_VALUE_PARTIAL_INFORMATION header (12 bytes) + up to 1 KiB of data.
    let mut info = [0u8; 12 + 1024];
    let mut got = 0u32;
    // SAFETY: valid handle, name and buffer.
    let st = unsafe {
        wk::ntddk::ZwQueryValueKey(
            key,
            &raw mut value_name,
            wk::_KEY_VALUE_INFORMATION_CLASS::KeyValuePartialInformation,
            info.as_mut_ptr().cast(),
            info.len() as u32,
            &raw mut got,
        )
    };
    // SAFETY: we opened it.
    let _ = unsafe { wk::ntddk::ZwClose(key) };
    if st < 0 {
        return 0;
    }
    let ty = u32::from_le_bytes([info[4], info[5], info[6], info[7]]);
    let len = u32::from_le_bytes([info[8], info[9], info[10], info[11]]) as usize;
    if ty != wk::REG_BINARY {
        return 0;
    }
    let n = len.min(out.len()).min(info.len() - 12);
    out[..n].copy_from_slice(&info[12..12 + n]);
    n
}

/// `UNICODE_STRING` over a NUL-terminated static wide string (NUL excluded).
fn ustr(s: &[u16]) -> wk::UNICODE_STRING {
    let chars = s.iter().position(|&c| c == 0).unwrap_or(s.len());
    #[allow(clippy::cast_possible_truncation)]
    let bytes = (chars * 2) as u16;
    wk::UNICODE_STRING {
        Length: bytes,
        MaximumLength: bytes,
        Buffer: s.as_ptr().cast_mut(),
    }
}
