# Contributing to SoraCard

Thanks for helping make SD card readers work properly on Windows.

## What this project is

A clean-room driver for USB card readers that vendor drivers handle badly
(broken media-change reporting being the usual sin), currently targeting the
Realtek RTS5129. It is **not** derived from any vendor driver source;
behaviour comes from the SD, SCSI and USB standards, the public Linux
`rtsx_usb` driver's description of the register protocol, observed hardware
behaviour, and analysis of the vendor binary's behaviour.

**Do not** put vendor driver source, decompiled code, or vendor binaries
(drivers, `.cab` packages) into this repository. Original code only.
Analysis notes may quote short disassembly excerpts where they document
behaviour needed for interoperability (as `docs/CARD-DETECT-RE.md` does).

Read [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) first.

## Architecture rules

1. **Protocol logic lives in `sora-core`.** It is `no_std`, allocation-free and
   `#![forbid(unsafe_code)]`, so it runs under plain `cargo test` on any OS.
   If you can express the logic without the kernel, do — and add tests.
   (Examples: the SD init state machine, RTCR packet encoding, the SCSI
   target's answers and media-change semantics.)
2. **The driver crate stays a thin shell.** `storport/` sequences hardware
   access, owns threads/IRPs/URBs and keeps `unsafe` at the FFI boundary.
3. **No panics in driver code.** A panic parks the thread; in the I/O worker
   that wedges all I/O and hangs device stop and shutdown. Use `get()` /
   checked arithmetic rather than indexing that can fail.
4. **Trace what you add.** There is no kernel debugger on the test machine;
   new behaviour should leave evidence in the registry trace (`diag.rs`) and
   be listed in [`docs/TESTING.md`](docs/TESTING.md#the-registry-trace).

## Testing

```sh
cargo test -p sora-core                                  # host-only, any OS
cargo clippy -p sora-core --all-targets -- -D warnings
cargo fmt --all --check && (cd storport && cargo fmt --check)
```

and, on the Windows build machine, `tools\build_storport.ps1 -Clippy` for the
driver crate. All of these pass today; keep it that way. The driver is built
and packaged in CI on `windows-latest` (see `.github/workflows/ci.yml`). On real hardware, follow
[`docs/TESTING.md`](docs/TESTING.md); the acceptance test is:

1. Boot with **no card**.
2. Insert a card → it must appear in **< 5 s** with no manual device restart.
3. Remove it → it must disappear in **< 5 s**, no stale "present" state.
4. Repeat 10×, then reboot with the card in and confirm it mounts.

## Adding a new reader

1. Capture the device's USB descriptors and find out whether it is genuine
   mass storage (Bulk-Only Transport) or register-programmed like the RTS5129.
2. Add the hardware ID to `storport/soracard.inx`.
3. Mass-storage readers use the BOT backend as-is. RTCR-family readers
   (`0BDA:0129/0139/0140` are listed already) share the protocol; package
   differences (QFN24 vs LQFP48 pin pulls and power setup) are handled in
   `chip::init` / `sora_core::rtsx_sd::sd_pulls`. Check bring-up and the
   speed modes with the trace (`SdSpeed`, `SdTuneMap`) and `tools\bench.ps1`.
4. Record the hardware findings in `docs/` (see `docs/RTS5129-notes.md`).

`sora_core::quirks` (a per-VID/PID quirk table) exists from the earlier KMDF
design but is not wired into the StorPort driver yet; backend selection today
is by probing (`chip::init`).

## Licence

By contributing you agree your work is dual-licensed under MIT or Apache-2.0,
at the user's option (`MIT OR Apache-2.0`).
