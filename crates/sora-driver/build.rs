// Copyright (c) SoraCard
// SPDX-License-Identifier: MIT OR Apache-2.0

fn main() -> anyhow::Result<()> {
    Ok(wdk_build::configure_wdk_binary_build()?)
}
