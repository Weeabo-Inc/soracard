# Building the driver

The driver is `storport/` — a **WDM StorPort miniport** built with
[`windows-drivers-rs`](https://github.com/microsoft/windows-drivers-rs).
`windows-drivers-rs` allows one WDK driver model per workspace, so `storport/`
is its own cargo workspace (WDM); the root workspace holds `sora-core` and the
superseded KMDF crate.

The protocol core needs nothing Windows-specific:

```sh
cargo test -p sora-core      # any OS
```

## Toolchain (as installed on the test laptop)

| Component | Version | Where |
|---|---|---|
| Rust | stable (1.98.1) via `rustup` | `%USERPROFILE%\.cargo\bin` (not on the SSH `PATH`; the scripts call it by full path) |
| `cargo-wdk` | 0.1.1 | `cargo install cargo-wdk --locked` |
| LLVM / libclang (for bindgen) | 17.0.6 | `C:\Program Files\LLVM` |
| VS 2022 Build Tools, `VCTools` workload | — | provides `cl.exe` / `link.exe` / `vcvars64.bat` |
| WDK | 10.0.26100.6584, **NuGet** package `Microsoft.Windows.WDK.x64` | `C:\packages\` |
| Windows SDK | 10.0.26100.1, NuGet `Microsoft.Windows.SDK.CPP(.x64)` | `C:\packages\` |

The NuGet WDK is the route Microsoft's own CI uses (no full Visual Studio).
`tools/install_toolchain.ps1` installs Rust, LLVM, the Build Tools and
cargo tools via winget (it also installs the winget SDK/WDK 22621, which the
build does not use); `tools/teardown.ps1` removes everything again.

## Build

From the repository on the Windows machine (works over SSH):

```pwsh
tools\build_storport.ps1                     # compile only (debug)
tools\build_storport.ps1 -Package            # compile + INF/CAT + test-sign → package
tools\build_storport.ps1 -Package -Release   # the same, optimized (LTO); what gets deployed
tools\build_storport.ps1 -Clippy             # lint the driver crate, warnings are errors
```

The script imports the MSVC environment from `vcvars64.bat`, puts LLVM on
`PATH`, sets the WDK variables `wdk-build` needs for a NuGet WDK, and runs
`cargo build`, `cargo clippy` or `cargo wdk build --profile dev|release
--target-arch amd64`:

```
WDKContentRoot    = C:\packages\Microsoft.Windows.WDK.x64.<ver>\c\
WDKBinRoot        = …\c\bin\10.0.26100.0
WDKToolRoot       = …\c\tools\10.0.26100.0
WindowsSdkBinPath = C:\packages\Microsoft.Windows.SDK.CPP.<ver>\c\bin
Version_Number    = 10.0.26100.0
NugetPackagesRoot = C:\packages
```

The package lands in
`storport\target\x86_64-pc-windows-msvc\debug\soracard_package\` (or
`release\`): `soracard.sys`, `soracard.inf`, `soracard.cat`. The release
`soracard.sys` is about 70 KB (debug about 190 KB). Installing it is covered in
[`TESTING.md`](TESTING.md).

`storport/build.rs` links `storport.lib` (from `WDKContentRoot\Lib\<ver>\km\x64`)
and `usbdex.lib` (the `USBD_*` helpers). StorPort bindings come from the
`storport-sys` crate; USB types and `USBD_*` from `wdk-sys` with the `usb`
feature.

## Signing

Test-signed only. The test machine has **test signing on** (`bcdedit
/set testsigning on`) and **Secure Boot off**, which test signing requires.

`wdk-build` signs with a certificate named `WDRLocalTestCert` in the store
`WDRTestCertStore`. Its own "create the cert if missing" step is unreliable
(see gotcha 6), so create it once by hand with `tools/make_test_cert.ps1`: a
keyed code-signing cert in `LocalMachine\My`, exported and imported as a PFX
into the **machine** `WDRTestCertStore`.

## CI

`.github/workflows/ci.yml` runs the `sora-core` host tests, `rustfmt` and
`clippy -D warnings` on Linux, and builds and packages the StorPort driver
(release profile) on `windows-latest` with the NuGet WDK. All of these pass
locally as of 2026-10-06 (91 host tests; `rustfmt` and `clippy` clean on
`sora-core` and on the driver crate). The repository is not under version
control yet, so the workflow itself has never run.

### Local tools (Linux development host)

The development host uses `rustup` installed in the home directory without
touching the shell profile (Fedora's own Rust stays the default), with the
`rustfmt` and `clippy` components:

```sh
~/.cargo/bin/cargo test -p sora-core
~/.cargo/bin/cargo fmt --all --check && (cd storport && ~/.cargo/bin/cargo fmt --check)
~/.cargo/bin/cargo clippy -p sora-core --all-targets -- -D warnings
```

The driver crate itself only builds on Windows (it needs the WDK); lint it
there with `tools\build_storport.ps1 -Clippy`.

## Gotchas (so you don't hit them again)

1. **`wdk-sys` USB bindings are behind a cargo feature**:
   `wdk-sys = { …, features = ["usb"] }`, otherwise USB functions fail with
   *"Failed to find function info for …"*.
2. **`wdk-macros` caches WDF function info** in a scratch JSON and does not
   invalidate it when the generated bindings change. After toggling features
   or WDK config, delete
   `target/<profile>/build/scratch-*/out/wdk_macros_ast_fragments/`.
3. **Driver package isolation**: binaries must go to `DIRID 13` (DriverStore),
   not `%12%`, or `infverif` fails with `ERROR(1322)`. Use
   `ServiceBinary = %13%\soracard.sys`.
4. **KMDF minor version** (KMDF crate only): `wdk-sys` defaults to KMDF 1.33
   (Windows 11); Windows 10 19045 ships 1.31 and refuses a newer binding
   (`CM_PROB_FAILED_DRIVER_ENTRY`). The root workspace pins
   `target-kmdf-version-minor = 31`. Irrelevant to the WDM StorPort crate.
5. **Duplicate `_fltused` / `__CxxFrameHandler3`**: `wdk-sys` already defines
   both; do not declare them again in the driver.
6. **Test certificate**: `wdk-build`'s existence check can import the public
   `.cer` (no private key) into the store, after which signing fails with
   *"No certificates were found that met all the given criteria"*; `makecert`
   may also be missing. Fix as described under Signing. From an SSH session,
   only the machine stores are writable (`NTE_PERM` on user key stores).
7. **Panics**: both profiles use `panic = "abort"`, and the driver's panic
   handler parks the thread (see [`ARCHITECTURE.md` §8](ARCHITECTURE.md#8-diagnostics)).
   Debug builds keep overflow and bounds checks — a slice-index mistake in the
   worker wedges all I/O, so avoid unchecked indexing.
8. The `cdylib` linker note about creating `soracard.dll.lib` is harmless.
