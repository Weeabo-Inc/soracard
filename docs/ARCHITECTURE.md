# Architecture

How the SoraCard driver works, why it is shaped this way, and the lessons that
got it working. Everything here describes the code in `storport/` and
`crates/sora-core/` as of 2026-10-06 (verified on hardware: hotplug, eject,
write protect, sleep/resume, SDR50 reads and writes).

## 1. The stack

```
 Windows storage stack: partmgr / volmgr / exFAT, disk.sys + classpnp
          │  SCSI commands in SRBs
          ▼
 StorPort (in-box port driver: PnP, claim/queue handling, LUN enumeration)
          │  HwStorStartIo(SRB)
          ▼
 soracard.sys — StorPort *virtual* miniport           storport/src/
   lib.rs     callbacks ─► io.rs  SRB queue ─► worker thread
                                   │
                                   ├─ SCSI target  (sora_core::scsi_target)
                                   ├─ card detect  (io.rs detect_step)
                                   └─ SD host      (sdhost.rs ─► chip.rs)
          │  URBs in IRPs we own (usb.rs)
          ▼
 USB stack (usbhub/xHCI) ─► RTS5129 controller ─► SD card
```

The reader binds as a **SCSIAdapter**-class device (`storport/soracard.inx`).
StorPort enumerates one LUN (target 0, LUN 0) and `disk.sys` sits on it, so
the card appears as an ordinary removable disk.

## 2. Why a StorPort miniport (and not KMDF)

The first attempt (`crates/sora-driver/`, kept for reference only) was a KMDF bus driver that created its own disk PDO.
It loaded, read descriptors and got `disk.sys` to bind, but it had to
**emulate a SCSI port driver**: `classpnp` sends the port-only SRB set
(`CLAIM_DEVICE`, `RELEASE_DEVICE`, `ATTACH_DEVICE`, queue lock/flush…), and
`CLAIM_DEVICE` must return the port driver's own LUN device object. Returning
the raw PDO made `classpnp` call into the wrong object → bugcheck `0x7E`.

StorPort *is* that port driver: it owns claim/attach/release/queue and PnP
for the LUN, and calls the miniport only for real I/O. So the driver became a
StorPort miniport. KMDF lessons that still apply (KMDF 1.31 on Windows 10,
signing) are in [`BUILDING.md`](BUILDING.md).

## 3. Why the driver is the SCSI target

The RTS5129 enumerates as vendor class `FF`, subclass `06`, protocol `50`,
which *looks* like SCSI-over-Bulk-Only-Transport — but the chip has **no SCSI
firmware**. It speaks Realtek's register protocol ("RTCR"): the host reads and
writes controller registers over ep0 or in batches over the bulk pipes, and
drives the SD bus itself. (The inbox `USBSTOR` driver fails to start it for
this reason.) So the driver:

1. initialises the controller (`chip::init`),
2. initialises the SD card with the SD protocol (`sdhost::bring_up_card`, state
   machine in `sora_core::sd`),
3. answers every SCSI command itself (`sora_core::scsi_target`), turning
   READ/WRITE into SD block transfers (`sdhost::transfer`).

At bring-up, `chip::init` probes for an RTCR controller; if it answers,
`io::use_rtsx` selects this backend. If it does not, the worker falls back to
plain **Bulk-Only Transport** pass-through (`sora_core::bot`/`bot_plan`) for
genuine mass-storage readers. On the RTS5129 the RTCR backend is always used.

## 4. Bring-up (`lib.rs`, `usb.rs`, `chip.rs`)

StorPort setup, as a virtual miniport (the reader has no hardware resources
of its own):

* `DriverEntry` fills `VIRTUAL_HW_INITIALIZATION_DATA`
  (`AdapterInterfaceType = Internal`, `MapBuffers =
  STOR_MAP_NON_READ_WRITE_BUFFERS`, `AutoRequestSense`, `MultipleRequestPerLu`)
  and calls `StorPortInitialize`.
* `HwStorFindAdapter` (PASSIVE_LEVEL) does the USB bring-up below, then sets
  `VirtualDevice = TRUE`, one bus / target / LUN, `MaximumTransferLength`
  (512 KiB on the RTCR backend, 64 KiB for Bulk-Only), and **`ScatterGather =
  TRUE`, `Master = TRUE`**: StorPort rejects adapters that are not bus-master
  S/G, even virtual ones (`STATUS_DEVICE_CONFIGURATION_ERROR` otherwise).
* **USB bring-up runs inside `HwStorFindAdapter`.** A
  `HwStorPassiveInitializeRoutine` requested from `HwStorInitialize` is never
  delivered to a virtual miniport on Windows 10, and the undelivered request
  made every later stop/remove of the adapter hang.

`usb::passive_initialize` then (recorded as `UsbStage` 1…7):

1. `USBD_CreateHandle` (FDO + lower device object from
   `StorPortGetDeviceObjects`),
2. reads the configuration descriptor,
3. selects the configuration and records the bulk IN/OUT pipe handles,
4. allocates the single URB the worker reuses,
5. runs the optional `Parameters\InitSeq` experiment hook (absent normally),
6. probes the controller with `chip::init`: if it answers the RTCR protocol,
   the RTCR backend is used; `chip::init` also detects the package (QFN24 or
   LQFP48, from `CARD_SHARE_MODE` bit 2) and applies the base configuration,
7. starts the worker thread.

`Parameters\Arm` is a development kill switch, armed by default: `0` loads
the adapter without starting the data path (every SCSI request gets
`NO_DEVICE`, `UsbStage = 0xA0`), `1` arms for one start only, absent or `2`
is the normal state. See [`TESTING.md`](TESTING.md#driver-parameters).

When the adapter stops (`HwStorFreeAdapterResources`), the worker is joined
and the SD slot is powered down before USB is closed, so a card is never left
powered at 1.8 V signaling with no driver managing it.

## 5. The I/O path (`io.rs`)

`HwStorStartIo` runs at up to DISPATCH_LEVEL and must not block, while USB
transfers need PASSIVE_LEVEL. So it only enqueues `EXECUTE_SCSI` SRBs into a
32-entry ring and signals **one system worker thread**, which owns every USB
transfer and completes the SRB. Other SRB functions (flush, shutdown, power,
resets) are answered inline.

* Each USB transfer is a URB in an IRP the driver allocates itself, with a
  completion routine returning `STATUS_MORE_PROCESSING_REQUIRED`. Transfers
  are timed out with `KeWaitForSingleObject`, cancelled with `IoCancelIrp`,
  and the worker always waits for the completion, so a wedged reader cannot
  free memory the USB stack still uses.
* Each SRB gets one deadline shorter than its `TimeOutValue`, bounding all of
  its transfers together.
* Resets (`HwStorResetBus`, `SRB_FUNCTION_RESET_*`) complete the queued SRBs,
  cancel the in-flight transfer and let the worker complete the in-flight SRB
  — exactly-once completion.
* **Sector I/O is done in place**: the SRB's buffer (its locked system
  mapping from `StorPortGetSystemAddress`) is handed straight to the USB
  stack, with no copy. One SRB of up to **512 KiB** is one SD multi-block
  command (CMD18/CMD25 + CMD12; CMD17/CMD24 for one block), the same limit as
  Linux's `rtsx_usb_sdmmc`. Per-transfer driver overhead (command, setup,
  status and stop phases) is about 0.4 ms; the data phase is the rest
  (`Perf*` trace values).
* A failed transfer is retried once after dropping to default speed; if it
  still fails, the card is checked and, if it lost its state, re-attached
  (§6) and the transfer retried again. Only then does the SRB fail with a
  medium error.
* Non-sector commands (INQUIRY, MODE SENSE, …) are answered through a small
  bounce buffer. The Bulk-Only backend still copies through a 64 KiB bounce
  buffer.

## 6. Card detect, recovery, eject: the actual fix

The vendor driver fails because the controller's **card-detect bit is stuck
at "present"** on this board (see [`RTS5129-notes.md`](RTS5129-notes.md)). So
SoraCard decides presence by **whether the card answers**:

* `disk.sys`/`classpnp` poll removable media with TEST UNIT READY (several
  times a second here). Every non-sector command runs one card-detect step
  (`detect_step`) before it is answered; sector I/O skips it.
* **Card ready:** CMD13 (SEND_STATUS) to the card's RCA at most every 500 ms
  (`CHECK_MS`, one retry). If it does not answer, the slot is power-cycled
  and the card re-initialised (`recover_card`):
  * the **same card** (same CID) answers → re-attached transparently;
    Windows never sees a media change and open files survive (`Recoveries`).
    This is what makes resume from sleep and transient glitches invisible;
  * a **different card** answers → reported as a media change;
  * **nothing** answers → removed: slot powered down, NOT READY / MEDIUM NOT
    PRESENT (`02/3A/00`).
* **No card:** a full bring-up attempt at most every 1 s (`RETRY_MS`), every
  2 s once the slot has been empty for a minute (`SLOW_RETRY_MS`; still well
  inside the 5 s insert target). Success → the next command gets UNIT
  ATTENTION / MEDIUM MAY HAVE CHANGED (`06/28/00`), so Windows re-reads
  capacity and mounts the volume.
* Any change in the raw ep0 status word triggers a check immediately; a
  failed sector transfer forces one.
* **Resume:** `HwStorAdapterControl(ScsiRestartAdapter)` flags the worker,
  which re-runs controller init (the reader may have lost power) and forces a
  card check before the next command; the card is re-attached as above.
* **Eject** (START STOP UNIT with LoEj, i.e. Explorer's "Eject"): the card is
  powered down and reported absent. It stays ejected while it is still in the
  slot, which a cheap probe (power up, CMD0, CMD8/CMD55; tens of ms) checks
  once per second; once nothing answers, normal detection resumes and the
  next insert mounts. A load request (LoEj with Start) cancels the eject.
* **Write protect:** the slot's lock switch (status word bit 3 while a card is
  in) or the card's CSD makes the medium write-protected (MODE SENSE WP bit,
  writes refused).

The LUN itself **never disappears**: with no card it stays present as a
removable disk with no media (`INQUIRY` reports `RMB = 1`). That stable LUN is
what lets Windows notice changes without any PnP re-enumeration. The vendor
driver drops its LUN entirely, which is the other half of its bug.

## 7. SD host (`sdhost.rs`, `chip.rs`, `sora_core::{rtsx, rtsx_sd, sd}`)

* **Commands** are RTCR batches: write the 6-byte command frame into
  `SD_CMD0..4`, set `SD_CFG2` for the response type, start `SD_TRANSFER`,
  have the controller wait for `TRANSFER_END`, then read back the response
  registers and `SD_STAT1` (CRC status). Application commands get CMD55 first.
* **Bring-up** (`bring_up_card`) always starts with a power cycle (card reset,
  pads back to 3.3 V), then picks the fastest mode that works:

  | Mode | Bus | How |
  |---|---|---|
  | **UHS-I SDR50** | 100 MHz, 1.8 V, 4-bit | ACMD41 with S18R+XPC; if the card answers S18A: CMD11, then the host switch (clock free-running, DAT lines must go low, clock gated, pads + LDO to 1.8 V, 10 ms, DAT lines must come back high). After init: CMD6 current limit (up to 800 mA), CMD6 SDR50, SD 3.0 timing at 100 MHz (no clock doubling, variable-phase clocks), then RX tuning |
  | **High Speed** | 50 MHz, 3.3 V | CMD6 query + switch (group 1, function 1), then 50 MHz with the SD 2.0 push/sample points ¼ cycle ahead/late |
  | **Default** | 25 MHz | always available |

  Any failure on the 1.8 V path power-cycles the card and retries at 3.3 V;
  a failed High Speed switch stays at default speed. `Parameters\Uhs` and
  `Parameters\HighSpeed` (default on) can disable either.
* **Tuning** (SDR50): fixed TX phase 1, then the 16 RX sample phases are swept
  with CMD19 (64-byte tuning block, CRC-checked by the controller) three
  times; the phase in the middle of the longest window that passed every
  sweep is used (`SdTuneMap`, `SdPhase`). On the test card phases 1–14 pass.
* **Init** state machine (`sora_core::sd::Init`): slow identification clock
  (30 MHz SSC through the SD divider), 74+ clocks, CMD0 → CMD8 → ACMD41
  (polled, ~1 s max) → [CMD11] → CMD2 → CMD3 → CMD9 → CMD7 → ACMD6 (4-bit) →
  CMD16 (SDSC only). Bounded to 5 s.
* **Block I/O**: the command batch, then a data-phase batch with
  `STAGE_DATA_IN/OUT`, then a bulk transfer of the data, then a 4-byte status.
* After any failed command or transfer the controller's FSM error state is
  cleared (`SFSM_ED`), or it stays wedged.
* The SD pin pulls differ by package (QFN24 on this laptop, LQFP48 on
  RTS5139-class boards); `chip::init` picks the table.

## 8. Diagnostics

There is no kernel debugger on the test machine, so the driver records its
state as registry values under
`HKLM\SYSTEM\CurrentControlSet\Services\SoraCard\Parameters` (`diag.rs`). By
default it writes only what is cheap and useful: bring-up results, the last
card bring-up (speed, tuning, CID), state-change counters, errors, and
command/performance counters every 256 commands. `Parameters\Trace = 1` turns
on the detailed trace (every command for the first 512, every poll, every SD
init step as it happens). The value reference is in
[`TESTING.md`](TESTING.md#the-registry-trace).

A Rust panic parks the thread: at PASSIVE_LEVEL it records `PanicLine` /
`PanicFile` and sleeps (so it shows up in the trace instead of silently pinning
a CPU core); a panic inside the worker still wedges I/O and makes device stop
and shutdown hang, so panics must not happen. Avoid unchecked slicing in
driver code.

## 9. Code map

| Concern | Where |
|---|---|
| StorPort callbacks, adapter config, resume hook | `storport/src/lib.rs` |
| USB bring-up, URB/IRP transport, `Arm` switch | `storport/src/usb.rs` |
| SRB queue, worker, SCSI target glue, card detect / recovery / eject, trace | `storport/src/io.rs` |
| RTCR register access, controller init, package detection, status poll | `storport/src/chip.rs` |
| SD power, clocks, commands, bring-up, High Speed, SDR50 + tuning, transfers | `storport/src/sdhost.rs` |
| `StorPortGetDeviceObjects` / `GetSystemAddress` wrappers | `storport/src/spx.rs` |
| Registry trace, `Trace` switch | `storport/src/diag.rs` |
| RTCR packets, register map, status parsing | `sora_core::rtsx` |
| SD-on-RTCR batches: commands, data reads, timing, signal voltage, tuning, phases, clocks, pin pulls | `sora_core::rtsx_sd` |
| SD protocol: commands, OCR/CSD, CMD6 switch status, init state machine | `sora_core::sd` |
| SCSI target (INQUIRY, TUR, capacity, sense, media change) | `sora_core::scsi_target` |
| BOT pass-through backend | `sora_core::{bot, bot_plan, scsi, scsi_emu}` |
| Not used by the StorPort driver yet | `sora_core::{card, quirks}` (debounce policy and per-VID/PID quirk table from the KMDF era) |
