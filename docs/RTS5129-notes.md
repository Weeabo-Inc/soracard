# RTS5129 — hardware notes

Facts about the reader as observed on the test laptop (Lenovo IdeaPad
110-15ISK, model 80UD), recorded by the driver's registry trace unless stated
otherwise. Driver design built on these facts: [`ARCHITECTURE.md`](ARCHITECTURE.md).

## USB identity

```
InstanceId      USB\VID_0BDA&PID_0129\20100201396000000
HardwareID      USB\VID_0BDA&PID_0129&REV_3960, USB\VID_0BDA&PID_0129
CompatibleIDs   USB\Class_FF&SubClass_06&Prot_50, …
Location        root hub port 6 (soldered to the board, internal USB 2.0)
```

Configuration descriptor (`UsbConfigDesc`, 39 bytes):

```
09 02 27 00 01 01 04 A0 FA      1 interface, bus-powered, remote wakeup, 500 mA
09 04 00 00 03 FF 06 50 05      interface 0: 3 endpoints, class FF/06/50
07 05 01 02 00 02 00            EP 0x01 bulk OUT, 512 bytes
07 05 82 02 00 02 00            EP 0x82 bulk IN,  512 bytes
07 05 83 03 03 00 0A            EP 0x83 interrupt IN, 3 bytes, interval 10
```

`FF/06/50` reads as "vendor class, SCSI transparent, Bulk-Only Transport", but
**the chip has no SCSI firmware**: CBWs are not answered as mass storage. The
inbox `USBSTOR` driver, bound by hand, fails to start it
(`CM_PROB_FAILED_START`, `0xC0000001`).

## The RTCR register protocol

The controller is programmed register by register (the same family the Linux
`rtsx_usb` driver supports: RTS5129/5139/5179):

* **ep0 vendor requests**: masked register write (`bRequest 0`, address in
  `wValue` with flag `0xC0`, mask/value in `wIndex`), single register read
  (flag `0x80`), and the **status poll** (`bRequest 2`, 2 bytes).
* **Batches on the bulk pipes**: an 8-byte header (`"RTCR"`, packet type,
  big-endian op count, stage flags) followed by four-byte ops (write / read /
  check-until). Stage flags announce a response and/or a following data phase
  (`STAGE_RESPONSE`, `STAGE_DATA_IN`, `STAGE_DATA_OUT`). Read and check ops each return one
  response byte on bulk IN, rounded up to 4 bytes.
* After a failed batch the command FSM must be cleared (`SFSM_ED` ←
  `0xF8`), otherwise the controller stays wedged.

Packet format and register map: `crates/sora-core/src/rtsx.rs`; SD command and
transfer batches: `crates/sora-core/src/rtsx_sd.rs`.

## Controller identity

`ChipId` = `03 00 02 83` at first power-up: `HW_VERSION = 0x03`,
`CARD_SHARE_MODE = 0x00` (QFN24 package: bit 2, the LQFP select, is clear;
the low bits read `01` once the driver has selected the SD card share mode),
`CFG_MODE_1 = 0x02` (RTS5179-class pull control applies), `CFG_MODE = 0x83`
(crystal-free part: PHY register `0xC2` is set to `0x7C` for a stable USB
clock).

## Card detect: the CD bit is stuck

The ep0 status poll returns a 16-bit word: bit 0 SD present, bit 1 MS, bit 2
xD, bit 3 write-protect. Traced live while a card was pulled and reinserted
(2026-10-06):

| Slot | Raw status word |
|---|---|
| card in | `0x0001` |
| card out | `0x0009` on one pull, `0x0001` (no change at all) on another |

**Bit 0 never clears with the slot empty.** Bit 3 (write-protect contact)
sometimes reads "protected" with no card, so it is only trusted while a card
is answering; then it does follow the lock switch (verified with the
adapter's latch). The status word therefore cannot tell whether a card is
present; the driver decides presence by whether the card answers SD commands
([`ARCHITECTURE.md` §6](ARCHITECTURE.md#6-card-detect-recovery-eject-the-actual-fix)).
This is also why the vendor driver never sees an insert
([`VENDOR-DRIVER.md`](VENDOR-DRIVER.md)).

The interrupt endpoint `0x83` is not used; no card-detect events have been
observed or relied on.

## Bus speed modes

The controller does what the Linux `rtsx_usb_sdmmc` driver advertises for it:
4-bit bus, SD High Speed, and UHS-I SDR12/SDR25/SDR50 with 1.8 V signaling
(its own LDO and pad control). Not DDR50 or SDR104.

| Mode | Clock | Status |
|---|---|---|
| Default | 25 MHz (SSC 50 MHz, doubled) | works |
| High Speed | 50 MHz (SSC 100 MHz, doubled) | works |
| SDR50 | 100 MHz (SSC 100 MHz, not doubled, variable-phase clocks) | works; RX phases 1–14 of 16 pass tuning on the test card |

## Sibling chips and packages

Linux binds the same driver to `0BDA:0129` (this laptop), `0BDA:0139` and
`0BDA:0140`; SoraCard's INF lists all three. The LQFP48 package (common on
RTS5139 boards) needs different SD pin pull values and an extra LDO/pull setup
at init; `chip::init` detects it and both are handled. Only the QFN24 RTS5129
has been tested.

## Test card

**Kingston Canvas Select Plus 128 GB microSDXC** (SDCS2/128GB, label
`31698-005 A00LF`, made in Taiwan) in Kingston's full-size SD adapter (label
`3500007-002.A00LF`), which carries the lock latch. UHS-I (answers S18A to the 1.8 V request); its CID reads
manufacturer `0x9F`, OEM `TI`, product `SD128`. Single exFAT partition labelled `Datos`;
`0xE8F6000` = 244,277,248 blocks = 125,069,950,976 bytes. Bring-up to SDR50
(1.8 V switch, CMD6 current limit 800 mA + SDR50, tuning) takes ≈ 0.4–0.9 s.

## Performance

Release build, SDR50, 512 KiB transfers in place:

| | |
|---|---|
| Sequential read | ≈ 33 MB/s (4 MB requests), near the practical USB 2.0 limit |
| Sequential write | 8–16 MB/s write-through, limited by the card |
| Driver overhead per 512 KiB transfer | ≈ 0.4 ms (command, setup, status, stop) |

History: 8.5 MB/s read at 25 MHz with 64 KiB bounce-buffered transfers;
20.8 MB/s at High Speed with 512 KiB in-place transfers.
