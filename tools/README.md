# tools

PowerShell / cmd scripts for the Windows test machine. Run them from the repo
checkout on that machine, elevated (an administrator SSH session is).
Workflow: [`docs/TESTING.md`](../docs/TESTING.md); build setup:
[`docs/BUILDING.md`](../docs/BUILDING.md).

## Build and deploy

| Script | Purpose |
|---|---|
| `install_toolchain.ps1` | Install Rust, LLVM 17, VS Build Tools and cargo tools (winget). The NuGet WDK/SDK in `C:\packages` is separate (see BUILDING.md). |
| `make_test_cert.ps1` | One-time: create the `WDRLocalTestCert` signing certificate in the machine stores and trust it |
| `build_storport.ps1` | Build the driver; `-Package` also produces the signed package, `-Release` the optimized build, `-Clippy` lints instead of building |
| `deploy_live.ps1` | Install the package with `pnputil` over the running reader and show the state (`-Release` for the optimized package); needed when the INF changes |
| `redeploy.ps1` | Fast iteration: swap the `.sys` in the DriverStore and restart the reader (`-Release` for the optimized build, `-StageOnly`: swap only, for a reboot) |
| `stage_sys.cmd` | The `.sys` swap itself (must run as SYSTEM; argument `debug`/`release`; used by `redeploy.ps1`) |
| `teardown.ps1` | Hand the reader back to the vendor driver and remove everything SoraCard installed |

## Test and inspect

| Script | Purpose |
|---|---|
| `sp_state.ps1` | Reader, disks, volumes, the driver's registry trace and recent System errors |
| `hotplug_log.ps1` / `.cmd` | Log card/disk/volume state changes to `C:\Users\Public\hpwatch.log` (the `.cmd` is for scheduled tasks) |
| `io_test.ps1` | Raw read consistency/throughput, write + verify-after-restart, read-only `chkdsk` |
| `bench.ps1` | Sequential read/write throughput with the driver's per-phase timing |
| `eject.ps1` | Eject the card like Explorer does (lock, dismount, eject media) |
| `sleep_test.ps1` | Put the machine into S3 sleep after a delay (run as a SYSTEM task) |

## Vendor driver investigation (historical)

Used while the vendor `RTSUER` driver was bound; they inspect its registry
parameters and state, not SoraCard's. See [`docs/VENDOR-DRIVER.md`](../docs/VENDOR-DRIVER.md).

| Script | Purpose |
|---|---|
| `set_params.ps1` | Back up and set `RTSUER\UVSTOR` parameters |
| `state_check.ps1` | Dump vendor driver, service and device state |
| `hotplug_watch2.ps1` | Hotplug logger for the vendor driver's USBSTOR disk |
