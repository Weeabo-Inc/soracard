// Copyright (c) SoraCard
// SPDX-License-Identifier: MIT OR Apache-2.0

fn main() -> anyhow::Result<()> {
    wdk_build::configure_wdk_binary_build()?;

    // StorPort miniports link against storport.lib, which lives in the WDK's
    // km\x64 lib directory. Derive it from the environment wdk-build uses.
    if let (Ok(root), Ok(ver)) = (
        std::env::var("WDKContentRoot"),
        std::env::var("Version_Number"),
    ) {
        println!("cargo:rustc-link-search=native={root}\\Lib\\{ver}\\km\\x64");
    }
    println!("cargo:rustc-link-lib=storport");
    // USBD_* helpers (USBD_CreateHandle, USBD_SelectConfigUrbAllocateAndBuild, …).
    println!("cargo:rustc-link-lib=usbdex");

    Ok(())
}
