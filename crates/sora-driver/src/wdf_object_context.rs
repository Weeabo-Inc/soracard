// Copyright (c) SoraCard
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Minimal WDF object-context plumbing.
//!
//! KMDF lets a driver attach a typed context to any WDF object. The `wdk` crate
//! does not (yet) ship these macros, so we carry a small, well-understood copy
//! of the pattern. A context is how we keep per-device state (the USB target,
//! pipes, card state) without globals.

use wdk_sys::{PCWDF_OBJECT_CONTEXT_TYPE_INFO, WDF_OBJECT_CONTEXT_TYPE_INFO};

#[repr(transparent)]
pub struct WDFObjectContextTypeInfo(WDF_OBJECT_CONTEXT_TYPE_INFO);
// SAFETY: the wrapped info is constant static data.
unsafe impl Sync for WDFObjectContextTypeInfo {}

impl WDFObjectContextTypeInfo {
    #[must_use]
    pub const fn new(inner: WDF_OBJECT_CONTEXT_TYPE_INFO) -> Self {
        Self(inner)
    }

    #[must_use]
    pub const fn get_unique_type(&self) -> PCWDF_OBJECT_CONTEXT_TYPE_INFO {
        let inner = core::ptr::from_ref::<Self>(self).cast::<WDF_OBJECT_CONTEXT_TYPE_INFO>();
        // SAFETY: repr(transparent) guarantees identical layout.
        unsafe { *inner }.UniqueType
    }
}

macro_rules! wdf_get_context_type_info {
    ($context_type:ident) => {
        paste::paste! {
            crate::[<WDF_ $context_type:snake:upper _TYPE_INFO>].get_unique_type()
        }
    };
}
pub(crate) use wdf_get_context_type_info;

macro_rules! wdf_declare_context_type_with_name {
    ($context_type:ident , $casting_function:ident) => {
        paste::paste! {
            type [<WDFPointerType$context_type>] = *mut $context_type;

            #[link_section = ".data"]
            pub static [<WDF_ $context_type:snake:upper _TYPE_INFO>]: crate::wdf_object_context::WDFObjectContextTypeInfo =
                crate::wdf_object_context::WDFObjectContextTypeInfo::new(
                    WDF_OBJECT_CONTEXT_TYPE_INFO {
                        Size: crate::WDF_OBJECT_CONTEXT_TYPE_INFO_SIZE,
                        ContextName: concat!(stringify!($context_type), '\0').as_bytes().as_ptr().cast(),
                        ContextSize: core::mem::size_of::<$context_type>(),
                        UniqueType: core::ptr::addr_of!([<WDF_ $context_type:snake:upper _TYPE_INFO>]).cast(),
                        EvtDriverGetUniqueContextType: None,
                    });

            /// # Safety
            /// `handle` must be a WDF object created with this context type.
            pub unsafe fn $casting_function(handle: wdk_sys::WDFOBJECT) -> [<WDFPointerType$context_type>] {
                unsafe {
                    wdk_sys::call_unsafe_wdf_function_binding!(
                        WdfObjectGetTypedContextWorker,
                        handle,
                        crate::wdf_object_context::wdf_get_context_type_info!($context_type),
                    ).cast()
                }
            }
        }
    };
}
pub(crate) use wdf_declare_context_type_with_name;

macro_rules! wdf_declare_context_type {
    ($context_type:ident) => {
        paste::paste! {
            crate::wdf_object_context::wdf_declare_context_type_with_name!(
                $context_type,
                [<wdf_object_get_ $context_type:snake>]
            );
        }
    };
}
pub(crate) use wdf_declare_context_type;
