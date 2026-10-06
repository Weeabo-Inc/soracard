# SoraCard

A clean-room Windows driver for the **Realtek RTS5129 USB SD card reader**
(`USB\VID_0BDA&PID_0129`), replacing the vendor `RTSUER` driver whose
media-change handling is broken: with it, a card inserted after boot is never
seen and a removal takes over two minutes to register.

Written in Rust: a **StorPort virtual miniport** (`storport/`) over a
host-testable, `no_std` protocol core (`crates/sora-core`).

## Status: working

Verified on the test laptop (Lenovo IdeaPad 110-15ISK, model 80UD; Windows 10
Home 22H2, build 19045) on 2026-10-06:

| Check | Result |
|---|---|
| Cold boot with the slot empty, then insert | card detected and mounted automatically |
| Hot remove | volume removed within ~1 s |
| Hot insert | volume mounted within ~1–2 s |
| Repeated remove/insert cycles | clean every time, no errors |
| Sequential read | **≈ 33 MB/s** (UHS-I SDR50; the first working version did 8.5) |
| Sequential write | 8–16 MB/s (limited by the card) |
| Data integrity at 100 MHz | raw-read hashes identical to 3.3 V reads; writes read back bit-exact; `chkdsk` clean |
| Sleep (S3) / resume with the card in | transparent: volume stays mounted, no media change |
| Eject from Explorer | card powered down, "No Media" until pulled and reinserted |
| Write-protect switch | locked card is read-only, writes refused |
| Hard power-off, then cold boot | comes back clean |
| Fresh install defaults | works with no registry settings |

The card shows up as an ordinary removable USB disk; Windows mounts it like
any other drive.

### Features

* **Prompt hotplug** without a working card-detect line: presence is decided
  by whether the card answers.
* **Fastest bus mode the card supports**: UHS-I SDR50 (100 MHz, 1.8 V, tuned
  sampling phase), else SD High Speed (50 MHz), else default speed, with
  automatic fallback on errors.
* **512 KiB transfers in place** (no copies), ~0.4 ms driver overhead each.
* **Transparent recovery**: a card that loses its state (sleep, a glitch) is
  re-attached without Windows noticing, if it is the same card.
* **Eject**, **write protect**, **sleep/resume**, removable-media semantics.
* SDSC, SDHC and SDXC cards; QFN24 and LQFP48 controller packages.

## How it works (one paragraph)

The RTS5129 is not a USB mass-storage device: it has no SCSI firmware. The host
programs the controller's registers directly (Realtek's "RTCR" protocol over
the control and bulk pipes) and speaks the SD protocol to the card itself. So
SoraCard is a StorPort miniport that **acts as the SCSI target**: Windows'
disk stack sends SCSI commands, and the driver answers them by driving the SD
card through the controller. Card presence is decided by **whether the card
answers SD commands**: the controller's card-detect bit is stuck at
"present" on this board, which is the root cause of the vendor driver's
behaviour. Details: [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md).

## Layout

```
storport/            the driver: StorPort virtual miniport (WDM, cdylib)
  src/lib.rs           DriverEntry and the StorPort callbacks
  src/usb.rs           USB bring-up (USBD handle, config, pipes) + URB transport
  src/chip.rs          RTCR register access, controller init, package detection
  src/sdhost.rs        SD power, clocks, commands, bring-up (High Speed, SDR50,
                       tuning), block I/O
  src/io.rs            SRB queue + worker thread, SCSI target, card detect,
                       recovery, resume, eject
  src/diag.rs          registry trace (there is no kernel debugger)
  soracard.inx         INF template (SCSIAdapter class)
crates/sora-core/    pure, no_std, #![forbid(unsafe_code)] logic, host-tested:
                     RTCR packets and SD batches, SD protocol + init state
                     machine (incl. UHS-I), SCSI target, BOT framing
crates/sora-driver/  SUPERSEDED first attempt (KMDF bus driver); see ARCHITECTURE.md
docs/                architecture, building, testing, hardware and vendor notes
tools/               build / deploy / test scripts for the Windows test machine
```

## Documentation

| Doc | What it covers |
|---|---|
| [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) | How the driver works, design decisions, and the lessons that got it working |
| [`docs/BUILDING.md`](docs/BUILDING.md) | Toolchain, building and signing the package |
| [`docs/TESTING.md`](docs/TESTING.md) | Deploying to the test machine, the registry trace, the acceptance test |
| [`docs/RTS5129-notes.md`](docs/RTS5129-notes.md) | Hardware facts: USB identity, endpoints, RTCR protocol, the stuck card-detect bit |
| [`docs/VENDOR-DRIVER.md`](docs/VENDOR-DRIVER.md) | The vendor `RTSUER` driver: its bugs, its parameters, how to restore it |
| [`docs/CARD-DETECT-RE.md`](docs/CARD-DETECT-RE.md) | Static analysis of the vendor driver's card-detect code |
| [`tools/README.md`](tools/README.md) | What each script does |

## Building and testing

Host tests for the protocol core run anywhere:

```sh
cargo test -p sora-core
```

The driver builds on Windows with the WDK; see [`docs/BUILDING.md`](docs/BUILDING.md).
Deployment and verification on real hardware: [`docs/TESTING.md`](docs/TESTING.md).

## Roadmap

- [x] Rule out a configuration fix for the vendor driver
- [x] StorPort miniport loads and binds to the reader
- [x] USB bring-up, RTCR controller init, SD card initialisation
- [x] SCSI target: the card appears as a removable disk; reads and writes
- [x] Hotplug: insert and remove detected without a device restart
- [x] Performance: 512 KiB in-place transfers, SD High Speed, UHS-I SDR50
      (≈ 33 MB/s read, near the practical USB 2.0 limit), release build
- [x] Sleep/resume, eject, write protect, transparent card recovery
- [x] Code hygiene: `rustfmt` and `clippy -D warnings` clean on both crates
- [ ] Full acceptance run: 10 hotplug cycles + reboots, with timed latencies
- [ ] Other readers in the family: 0BDA:0139 and 0BDA:0140 are in the INF
      (and LQFP48 packages are handled) but untested; SDSC/SD v1 cards
      untested (no such card at hand)
- [ ] Put the repository under version control so CI can run
- [ ] Proper (non-test) signing and an installer story

## Teardown

`tools/teardown.ps1` on the test machine removes every installed component
(Rust, LLVM, VS Build Tools, SDK, WDK, cargo tools), the test-signing state, the
SoraCard test certificate and any SoraCard driver packages we published.
