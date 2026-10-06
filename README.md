<div align="center">
	<img width=140 src="assets/cover.svg" />
	<h2>soracard</h2>
</div>

[![License](https://img.shields.io/badge/license-MIT%20%2F%20Apache--2.0-blue.svg?style=flat-square)]()
[![Platform](https://img.shields.io/badge/platform-Windows%20x64-0078D6.svg?style=flat-square)]()
[![Language](https://img.shields.io/badge/language-Rust-orange.svg?style=flat-square)]()
[![Status](https://img.shields.io/badge/status-hotplug%20%2B%20SDR50%20proven%20on%20hardware-brightgreen.svg?style=flat-square)]()

### Insert a card, it shows up. Pull it, it's gone.

A clean-room Windows driver for the **Realtek RTS5129 USB SD card reader** (`USB\VID_0BDA&PID_0129`), the one soldered into a lot of budget laptops. It replaces Realtek's `RTSUER` driver, which never notices a card inserted after boot and takes over two minutes to notice one being removed.

Written for a Lenovo IdeaPad 110 whose card reader only worked if you rebooted with the card already in. Rust: a StorPort virtual miniport (`storport/`) over a `no_std`, host-tested protocol core (`crates/sora-core`).

---

### Why does this exist?

Realtek's driver was probed until there was nothing left to configure. Every registry knob it reads was recovered from the binary, the ones that matter already default to sane values, and forcing them changes nothing. The inbox `USBSTOR` driver cannot start the device at all. So: our own driver.

Then the real cause turned up, in a trace of the reader's status word while a card was pulled and pushed back:

```
slot      status word    "SD present" bit
card in   0x0001         1
card out  0x0009         1      <- still "present"
card out  0x0001         1      <- no change at all
```

> **The card-detect bit is stuck at "present".**
>
> On this board the controller's CD line never reports an empty slot. A driver that trusts it never sees an insert, and only notices a removal when I/O to the missing card finally times out.
>
> **So soracard does not ask the slot. It asks the card.**
>
> A ready card gets a status command (CMD13) at most every 500 ms; an empty slot gets an initialisation attempt every second. Whoever answers is there.

The full story, with the vendor driver's card-detect code taken apart, is in [`docs/CARD-DETECT-RE.md`](docs/CARD-DETECT-RE.md) and [`docs/VENDOR-DRIVER.md`](docs/VENDOR-DRIVER.md).

---

### What does this do?

The card shows up as an ordinary removable disk, and Windows does the rest. Measured on the IdeaPad (Windows 10 22H2) with a Kingston Canvas Select Plus 128 GB:

| Check | Result |
|---|---|
| Cold boot with the slot empty, then insert | mounted ~2 s after insertion |
| Hot remove / hot insert | gone within ~1 s / back within 1–2 s, every cycle |
| Sequential read | **≈ 33 MB/s** (UHS-I SDR50, near the practical USB 2.0 limit) |
| Sequential write | 8–16 MB/s, limited by the card |
| Integrity at 100 MHz | raw-read hashes identical to 3.3 V reads, writes read back bit-exact, `chkdsk` clean |
| Sleep (S3) and resume with the card in | transparent: volume stays mounted, no media change |
| Eject from Explorer | card powered down, "No Media" until it is pulled and reinserted |
| Lock switch | locked card is read-only, writes refused |
| Hard power-off, then cold boot | comes back clean |
| Fresh install | works with no registry settings at all |

The modes it brings a card up in, fastest first:

| Mode | Bus | When |
|---|---|---|
| **SDR50** | 100 MHz, 1.8 V, tuned sample phase | UHS-I cards (they answer S18A) |
| **High Speed** | 50 MHz, 3.3 V | most SDHC/SDXC cards |
| **Default** | 25 MHz | everything else, and the fallback after errors |

The first working version did 8.5 MB/s. The jump to 33 came from 512 KiB transfers done in place (no copies, ~0.4 ms of driver overhead each), then High Speed, then SDR50 with phase tuning.

---

### How do I use it?

It is test-signed, so the machine needs test signing on (which means Secure Boot off):

```powershell
bcdedit /set testsigning on        # then reboot
```

Build and install, from a Windows machine with the WDK (setup in [`docs/BUILDING.md`](docs/BUILDING.md)):

```powershell
tools\make_test_cert.ps1                       # once: the signing certificate
tools\build_storport.ps1 -Package -Release     # build + sign the driver package
tools\deploy_live.ps1 -Release                 # install over the running reader
```

The protocol core builds and tests anywhere:

```console
$ cargo test -p sora-core
test result: ok. 91 passed; 0 failed
```

Everything is on by default. The optional settings live under `HKLM\SYSTEM\CurrentControlSet\Services\SoraCard\Parameters`:

| Value | Default | Meaning |
|---|---|---|
| `Uhs` | `1` | `0` never asks cards for 1.8 V / SDR50 |
| `HighSpeed` | `1` | `0` keeps 3.3 V cards at 25 MHz |
| `AllowWrites` | `1` | `0` presents every card read-only |
| `Trace` | `0` | `1` turns on the detailed registry trace |
| `Arm` | armed | `0` loads the adapter without touching the card (a development kill switch) |

To go back to Realtek's driver, `tools\teardown.ps1` reinstalls it (from the DriverStore, if it is still there) and removes everything soracard put on the machine.

---

### How it works under the hood

#### The chip has no SCSI firmware

The RTS5129 enumerates as `FF/06/50`, which *looks* like USB mass storage. It isn't. The host programs the controller's registers directly (Realtek's "RTCR" protocol, in batches over the bulk pipes) and speaks the SD protocol to the card itself. So soracard is a **StorPort miniport that is the SCSI target**: Windows sends SCSI commands, the driver answers them by driving the SD card.

#### Why StorPort

The first version was a KMDF bus driver. It got `disk.sys` to bind, then crashed in `classpnp`, because a bus driver has to emulate a SCSI port driver's private handshake (`CLAIM_DEVICE` must return the port's own device object). StorPort *is* that port driver, so it handles all of it and the miniport only sees real I/O.

#### A LUN that never leaves

With no card, the disk stays present as removable media with nothing in it (`INQUIRY` reports `RMB = 1`). An insert answers the next poll with `UNIT ATTENTION`, a removal with `NOT READY / MEDIUM NOT PRESENT`, and Windows does the mounting. No PnP re-enumeration, which is the other half of the vendor driver's bug: it drops its LUN entirely.

#### Re-attach, don't re-mount

If a ready card stops answering, the slot is power-cycled and the card brought up again. Same CID: it is re-attached silently, and Windows never sees a media change. Different CID: media change. Nothing: removed. That one path covers resume from sleep, glitches, a swapped card and a removal.

More in [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md).

---

### What it can and can't do

**Can do:**
- Detect insert and removal promptly on a board whose card-detect line is broken
- Run UHS-I cards at SDR50 with phase tuning, and fall back cleanly when a mode fails
- Survive sleep, eject, the lock switch and a hard power-off
- Explain itself: progress, speeds, tuning windows, per-phase transfer timing and panics all land in the registry trace ([`docs/TESTING.md`](docs/TESTING.md))

**Can't do:**
- Go past USB 2.0: about 33 MB/s is the ceiling. DDR50 and SDR104 are not implemented (Linux doesn't enable them for this chip either).
- MMC, Memory Stick, xD or SDIO cards. SD memory cards only.
- Load without test signing. There is no production signature.
- Promise anything about hardware it hasn't met: one RTS5129 (QFN24 package) and one card have been tested. `0BDA:0139` and `0BDA:0140` are in the INF and LQFP48 packages are handled, but untested.

---

### Project layout

```
soracard/
├── assets/
│   └── cover.svg
├── storport/                 the driver: StorPort virtual miniport (WDM)
│   ├── soracard.inx          INF template (SCSIAdapter class)
│   └── src/
│       ├── lib.rs            DriverEntry and the StorPort callbacks
│       ├── usb.rs            USB bring-up and URB transport
│       ├── chip.rs           RTCR register access, controller init
│       ├── sdhost.rs         SD power, clocks, bring-up, SDR50 tuning, block I/O
│       ├── io.rs             worker thread, SCSI target, card detect, recovery, eject
│       └── diag.rs           registry trace
├── crates/
│   ├── sora-core/            no_std, #![forbid(unsafe_code)], host-tested protocol logic
│   └── sora-driver/          the superseded KMDF attempt, kept for reference
├── docs/                     architecture, building, testing, hardware and vendor notes
└── tools/                    build, deploy and test scripts for the Windows machine
```

| Doc | What it covers |
|---|---|
| [`ARCHITECTURE.md`](docs/ARCHITECTURE.md) | How the driver works and the lessons that got it working |
| [`BUILDING.md`](docs/BUILDING.md) | Toolchain, building and signing |
| [`TESTING.md`](docs/TESTING.md) | Deploying, the registry trace, the acceptance test and its results |
| [`RTS5129-notes.md`](docs/RTS5129-notes.md) | Hardware facts: endpoints, RTCR protocol, the stuck CD bit, speed modes |
| [`VENDOR-DRIVER.md`](docs/VENDOR-DRIVER.md) | Realtek's driver: its bugs, its parameters, how to go back to it |
| [`CARD-DETECT-RE.md`](docs/CARD-DETECT-RE.md) | The vendor driver's card-detect code, taken apart |

---

### Credits

- The Linux [`rtsx_usb`](https://github.com/torvalds/linux/tree/master/drivers/mmc/host) drivers, the public description of the RTCR register protocol and the UHS-I sequence
- [windows-drivers-rs](https://github.com/microsoft/windows-drivers-rs) and [storport-sys](https://github.com/phdye-windows/storport-sys) for the kernel bindings
- The SD Association's simplified Physical Layer specification, which is drier than this README

Dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option.

---

<div align="center">
	<br/>
	<i>it answers, or it isn't there.</i>
	<br/>
	<sub>built for a card reader that only worked if you rebooted with the card in.</sub>
</div>
