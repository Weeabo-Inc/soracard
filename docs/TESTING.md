# Testing on hardware

How to deploy a build to the test machine, read what the driver did, and run
the acceptance test. Building is covered in [`BUILDING.md`](BUILDING.md); the
scripts are listed in [`tools/README.md`](../tools/README.md).

## The test machine

Lenovo IdeaPad 110-15ISK (80UD), Windows 10 Home 22H2 (19045), Spanish
locale (tool output and event text are in Spanish). Test signing on, Secure
Boot off, Fast Startup off (so "Shut down" is a real cold boot). Reachable
over SSH as an administrator; an SSH session is already elevated. The repo
lives at `C:\Users\Marcos\src\SoraCard`.

Reader instance: `USB\VID_0BDA&PID_0129\20100201396000000`.

## Deploying

Add `-Release` to every command below for the optimized build (what the
results were measured with); without it the debug build is used.

**First install** (or after changing the INF):

```pwsh
tools\build_storport.ps1 -Package -Release
tools\deploy_live.ps1 -Release   # pnputil /add-driver /install, then shows the state
```

Each `pnputil` install publishes another `oemNN.inf`; old SoraCard packages
can be removed with `pnputil /delete-driver oemNN.inf /uninstall /force`.

**Iterating on code** (INF unchanged): much faster, no new package:

```pwsh
tools\build_storport.ps1 -Package -Release
tools\redeploy.ps1 -Release      # stage the .sys as SYSTEM, restart the reader, show state
```

`redeploy.ps1` swaps `soracard.sys` inside the bound DriverStore folder
(`tools\stage_sys.cmd`, run as SYSTEM through a one-shot scheduled task,
because only SYSTEM may write there) and restarts the device. Before
deploying, lint with `tools\build_storport.ps1 -Clippy`.

**If the driver is wedged** (a stuck worker thread, see below), a device
restart or install hangs. Use `tools\redeploy.ps1 -StageOnly`, then reboot;
if shutdown hangs, hold the power button. A forced power-off is safe for the
card as long as nothing is being written to it at that moment.

## Driver parameters

Under `HKLM\SYSTEM\CurrentControlSet\Services\SoraCard\Parameters`. All are
optional; the defaults are what a normal install wants.

| Value | Default | Meaning |
|---|---|---|
| `Arm` | 2 (armed) | Development kill switch for the data path. `0`: the adapter loads but answers every SCSI request with `NO_DEVICE` (`UsbStage = 0xA0`). `1`: armed for the **next start only** (reset to 0 at start, so a crashing build cannot loop across boots). `2` or absent: armed. |
| `AllowWrites` | 1 | `0` presents every card write-protected. The card's own lock switch always applies. |
| `Uhs` | 1 | `0` never asks cards for 1.8 V / UHS-I SDR50. |
| `HighSpeed` | 1 | `0` keeps 3.3 V cards at default speed (25 MHz). |
| `Trace` | 0 | `1` enables the detailed trace (see below). |
| `BusType` | 7 | Written by the INF: report the bus as USB. |
| `InitSeq` | absent | Optional REG_BINARY experiment hook run at bring-up. |

Changes apply at the next adapter start (`Arm`, `AllowWrites`, `Trace`) or the
next card bring-up (`Uhs`, `HighSpeed`).

## The registry trace

There is no kernel debugger, so the driver writes its state to the same key.
Values persist across reboots and restarts, so **a value can be stale from
an earlier run**; compare against `ArmedAtStart`/`FindAdapterCalls` or delete
the values first. Counters are read-modify-write in the registry, so after a
hard power-off they can lose the last few minutes (the hive was not flushed).
`tools\sp_state.ps1` dumps the key plus device, disk and volume state.

By default only cheap values are written: bring-up, card bring-up results,
state-change counters, errors (the first 64), and counters every 256
commands. Values marked **(Trace)** need `Trace = 1`.

| Value(s) | What it tells you |
|---|---|
| `FindAdapterCalls`, `PassiveInitCalls`, `ConfigApplied`, `InitIrqlPlus1` | StorPort called us / bring-up ran |
| `ArmedAtStart` | the `Arm` value seen at this start |
| `UsbStage`, `UsbStatus` | bring-up progress: 1 entered, 2 USBD handle, 3 config descriptor, 4 select config, 5 worker URB, 6 worker thread, **7 ready**, `0xA0` disarmed; `UsbStatus` = NTSTATUS of a failure |
| `UsbConfigDesc`, `UsbIface`, `UsbEpIn`, `UsbEpOut` | descriptor and pipes found |
| `ChipInit`, `ChipId`, `HwVersion`, `CardStatusAtInit` | RTCR controller init (`ChipInit = 1` ok; `0xE1..0xE3` = step that failed); `ChipId` = `[HW_VERSION, CARD_SHARE_MODE, CFG_MODE_1, CFG_MODE]` |
| `WorkerStatus` | NTSTATUS of creating the worker thread |
| `SdInit`, `SdLog`, `SdCid`, `SdBlocks`, `SdFlags`, `LastInitMs` | last card init (recorded when the result changes, so an empty slot does not rewrite it every second): result (`1` ok, `0xE100` controller/USB error, `0xE2xx` command `xx` failed, `0xE300` no card, `0xE301` bad CMD8 echo, `0xE302` power-up timeout, `0xE303` 1.8 V switch failed, `0xE4xx` protocol error at command `xx`, `0xE500` card status error); per-command log (8-byte entries `[cmd, result, payload×4, 0, 0]`, result 0 = response, 1 = USB error, 3 = controller error, 4 = CRC, 5 = short, 6 = framing, 7 = card status error; cmd `0xFE` = host voltage switch, result 0 ok); CID; capacity in blocks; flags (bit 0 high capacity, bit 1 write-protected, bit 2 1.8 V/UHS); bring-up duration in ms |
| `SdSpeed` | bus mode after bring-up: `0` default, `1` High Speed, `2` SDR50; `0xE001`/`0xE002`/`0xE003` = High Speed query / switch / host clock failed |
| `SdUhsError` | why the last 1.8 V attempt fell back to 3.3 V (same codes as `SdInit`) |
| `SdCurrentLimit`, `SdTuneMap`, `SdPhase` | SDR50: current limit selected (3 = 800 mA), RX phases that passed tuning (bit i = phase i), chosen phase |
| `SpeedFallbacks` | transfers that failed and dropped the card to default speed |
| `CardPresent`, `Inserts`, `Removals`, `Recoveries`, `Restarts`, `Ejects` | card-detect state; cumulative counts of inserts, removals, transparent re-attaches of the same card, adapter restarts (resume), ejects |
| `PollLog`, `PollNext`; `PollCount`, `PollLast` (Trace) | raw ep0 status poll: ring of the last 32 *changes* as `[lo, hi, op, ok, seconds-since-boot:u32]`; total count and last `[lo, hi, op, ok]` |
| `Cmds`, `CmdErrors`, `History`, `HistoryNext` | SCSI commands handled / failed; ring of the last 64 as 16-byte `[op, cdb1, srb_status, scsi_status, sense key, asc, ascq, fail, len:u32, xfer:u32]`. NOT READY answers while the slot is empty count as errors. Every command for the first 512 with Trace. |
| `PerfXfers`, `PerfBlocks`, `PerfCmdMs`, `PerfSetupMs`, `PerfDataMs`, `PerfStatusMs`, `PerfStopMs` | sector transfers since the adapter started: count, 512-byte blocks, and total time per phase (command, data-phase setup, data, status, CMD12 stop). `PerfBlocks × 512 / PerfDataMs` is the bus throughput |
| `SdStage` (Trace) | SD init stage reached (9 = finished) |
| `XferLog`, `XferNext`, `LastXfer*`, `Resets`, `IrpStuckSec` | USB transfer outcomes; `IrpStuckSec` appears only if a cancelled transfer refuses to complete |
| `Inquiry`, `Capacity10`, `VaFallback` | Bulk-Only Transport backend only (not used on the RTS5129) |
| `PanicLine`, `PanicFile` | a Rust panic happened at PASSIVE_LEVEL: the thread is parked and I/O is wedged |
| `FreeStage` | adapter teardown progress (3 = complete) |

Diagnosing a wedge: `Cmds` not increasing and no new disk activity (with
Trace, `SdStage` frozen). Check `PanicLine` first, then `IrpStuckSec`. A
pinned CPU core in the System process (`(Get-Process -Id 4).TotalProcessorTime`)
means a thread is spinning.

## Hotplug logger

`tools\hotplug_log.ps1` writes a timestamped line to
`C:\Users\Public\hpwatch.log` whenever the card state, the SoraCard disk or the
volume changes (1 s sampling). To have it running from boot, as SYSTEM:

```pwsh
schtasks /create /f /tn SoraHpWatch /ru SYSTEM /sc onstart /tr C:\Users\Marcos\src\SoraCard\tools\hotplug_log.cmd
schtasks /run /tn SoraHpWatch      # start it now as well
```

## Read/write test

```pwsh
tools\io_test.ps1 -Read            # raw reads, consistency + throughput (1 MB requests)
tools\io_test.ps1 -Write           # creates E:\soracard_wtest.*
# restart the reader (or re-insert the card) so nothing comes from the cache
tools\io_test.ps1 -Verify          # hash read back from the card + read-only chkdsk
```

The raw-read hash at offset 100000 MB is a fixed fingerprint of the test card
(`A6D72AC7690F53BE` on the "Datos" card): it must not change with the bus
mode.

## Benchmark

```pwsh
tools\bench.ps1                    # 256 MB raw read in 4 MB requests + 128 MB write-through
```

Prints throughput plus the driver's per-phase time. Use large requests: with
1 MB requests the application's own round trips dominate (≈ 22 MB/s read
instead of ≈ 33). Write speed depends heavily on the card's internal state;
repeat a few times before comparing builds.

## Eject, write protect, sleep

```pwsh
tools\eject.ps1                    # what Explorer's Eject does; the disk shows "No Media"
```

Write protect: slide the card's (or adapter's) lock switch to LOCK and
reinsert; `Get-Disk` shows `IsReadOnly = True` and writes fail with "media is
write protected".

Sleep: from SSH, run the sleep as a SYSTEM task so it survives the session,
then wake the machine with the power button:

```pwsh
schtasks /create /f /tn SoraSleep /ru SYSTEM /sc once /st 23:59 /tr "powershell -NoProfile -ExecutionPolicy Bypass -File C:\Users\Marcos\src\SoraCard\tools\sleep_test.ps1"
schtasks /run /tn SoraSleep
```

After resume, `Restarts` and `Recoveries` should each go up by one, with no
`Removals`/`Inserts`, and the volume should still be mounted.

## Acceptance test

1. Remove the card, **shut down** (Fast Startup off), power on.
2. Insert the card → the volume must mount within 5 s, no manual restart.
3. Remove it → the volume must disappear within 5 s.
4. Repeat 10×; then reboot with the card in and confirm it mounts.
5. No `PanicLine`, no `IrpStuckSec`, `Inserts`/`Removals` match the cycles.

### Results (2026-10-06, release build)

| Step | Result |
|---|---|
| Cold boot, slot empty → insert | mounted ~2 s after insertion, SDR50, writable |
| **No registry settings** (fresh-install defaults: no `Arm`, no `AllowWrites`), reader restart, insert | armed by default, SDR50, writable |
| Remove / insert cycles | 7 cycles (4 at default speed, 3 at SDR50), each clean: removal within the logger's 1 s sampling, re-mount 1–2 s after insertion |
| Card in at driver start / after device restart | mounts at SDR50 (bring-up incl. 1.8 V switch and tuning ≈ 0.4–0.9 s) |
| Sequential read (`bench.ps1`) | 8.5 MB/s (first version) → 20.8 (512 KiB + High Speed) → **33 MB/s (SDR50)** |
| Sequential write | 4.3 MB/s (first version) → 8–16 MB/s (card-limited; driver overhead ≈ 0.4 ms per 512 KiB) |
| Integrity at SDR50 | offset-100000 fingerprint unchanged, 32 MB write read back bit-exact after restart, `chkdsk` clean |
| SDR50 tuning on the test card | phases 1–14 pass (`SdTuneMap = 0x7FFE`), phase 8 used, 800 mA limit |
| Sleep (S3) / resume, card in | transparent at High Speed and at SDR50 (`Restarts` + `Recoveries`, no media change) |
| Eject → pull → reinsert | "No Media" while ejected in the slot; pull noticed; reinsert mounts |
| Lock switch | locked: read-only, writes refused; unlocked: writable |
| Hard power-off, then cold boot | clean (no crash dump); registry changes from the last minutes were lost (hive not flushed), including deleted settings coming back |

**Still to do:** the full 10-cycle run with exact latencies (the 1 s logger
cannot resolve sub-second detection); hibernate; other cards (SDSC/SD v1,
non-UHS SDHC) and the 0139/0140 readers.
