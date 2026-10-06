# The vendor driver (`RTSUER`)

What is known about Realtek's own driver for this reader, why SoraCard
replaces it, and how to go back to it. The original investigation was done on
the same laptop under Windows 11 (build 26200) in September 2026; the laptop
now runs Windows 10, where the same vendor packages are still in the
DriverStore.

## Versions

| Package | Driver | Notes |
|---|---|---|
| `rtsuerd3.inf` | `RtsUer.sys` 418,784 B, 2016-10-27 | Old Lenovo-era driver. **Do not use.** Its INF writes `EnableAutoDelink=1`, `FirstLoad=1`; with no card at boot the driver start hung ~63.5 s (Driver Watchdog events 902/903), then PnP surprise-removed the reader (event 1010 → Code 45) |
| `rtsuer.inf` (generic) | `RtsUer.sys` 822,336 B, `10.0.26100.31289`, DriverVer 2024-12-26 | The usable vendor driver. Cab `441d6a27-2069-4846-99c0-d847453c31fc_88a2af93aa43e7701811fc6f8265f791bbf58153.cab` (1,914,520 B), from the Microsoft Update Catalog (search `10.0.26100.31289`; the catalog's automated download fails, download it in a browser) |
| `RtsUerLenovo.inf` | — | Wrong package: only matches `USB\VID_17EF&PID_30C0/30C1`, not this device (cab `8198054b-…e7ca1c3e….cab`, 1,911,946 B). Also avoid `RtsUerD3`, `RtsUerNLPM`, `rtsuer32` |

Both cabs currently sit in the repository root of the working copy for
reference. They are Realtek binaries: keep them out of version control (see
`CONTRIBUTING.md`).

## The bug SoraCard fixes

Measured with the generic 2024 driver (card out at boot, then inserted):

* **Insert is never detected.** No PnP events, no disk; only
  `pnputil /restart-device` on the reader made the card appear.
* **Removal is detected ~2 min 13 s late** (event 1010 surprise-remove); until
  then the volume stays mounted and `Present=True`.
* With the card out, the driver presents **no LUN at all**, so PnP has nothing
  to re-evaluate when a card arrives.
* The LUN, when present, reports `Capabilities = 0x14` (no removable bit) and
  `RemovalPolicy = 2`.

Power was ruled out (selective suspend disabled, Fast Startup off). Root cause,
found while building SoraCard: the controller's **card-detect bit stays
"present" with the slot empty** ([`RTS5129-notes.md`](RTS5129-notes.md#card-detect-the-cd-bit-is-stuck)),
so a driver that trusts it never sees a change and only notices a removal
when I/O to the missing card finally fails. The vendor driver's card-detect
code is analysed in [`CARD-DETECT-RE.md`](CARD-DETECT-RE.md).

## No configuration fix exists

The vendor driver reads its tuning parameters from
`HKLM\SYSTEM\CurrentControlSet\Services\RTSUER\UVSTOR`. They were recovered
from the 2024 binary and the relevant ones were tried (`MediaRemovable`,
`SetNoneRemovable`, `CheckMediaChange`, short intervals, `EnableAutoDelink`):
none changes the hotplug behaviour. `NonRemovable`, which the INF writes, is
not read by either driver generation at all.

### How parameters are read

For each parameter the loader does:

```
movl $default,  0x30(%rbp)     ; compiled-in default (or r12d=1 / r15d=0)
lea  <param name>, %rdx
mov  %r14, %rcx                ; pParas
call 0x14001DF08               ; query HKLM\...\Services\RTSUER\UVSTOR
mov  0x30(%rbp), %eax
mov  %eax, <offset>(%rdi)      ; store into pParas
```

So: a value present in the service key's `UVSTOR` subkey overrides the default;
absent values fall back to the defaults below.

* `r12d = 1`, `r15d = 0` are set once in the prologue (`0x14001EB97`,
  `0x14001EBD5`) and reused as boolean defaults.
* Some values are packed as flag bits, e.g. `CheckMediaChange` is **bit 0 of the
  dword at `pParas+0x689`** (`or %eax,0x689(%rdi)`, `eax = (value==1)`).

### Full parameter table (defaults)

| Parameter | Default | | Parameter | Default |
|---|---|---|---|---|
| ShowPowerManagePage | (empty) | | TURRetryTimes | 3 |
| Pattern | 0 | | EnableQuickDelink | 0 |
| MAC | 1 | | DelinkLatency | 0 |
| Icon | 1 | | FirstLoadDelinkLatency | 600000 |
| IconGroup | 2 | | ForceDelinkLatency | 0 |
| EnableSMBIOS | 0 | | MaxTransferLength | 1048576 |
| ParaType | 0 | | SscEnable | 0 |
| **RemovableControl** | **0x18 (24)** | | **MediaRemovable** | **1** |
| diffSTCAfterSuspend | 1 | | SeeQVaultInsertCSQEnable | 0 |
| KeepDiskStateDuringSuspend | 1 | | ForceDisableWP | 0 |
| **SetNoneRemovable** | **0** | | KeepDxIgnoreTUREnable | 0 |
| SetEHCIKey | 1 | | QuickShutdown | 1 |
| DeleteEHCIKey | 0 | | ScsiComplianceTest | 0 |
| NrEhciBeenSet | 0 | | EnableU3SequentialRW | 0 |
| FirstLoad | 0 | | SdErasePatch | 1 |
| ContainerIdMode | 3 | | LedOffWhenCardExist | 0 |
| **FixedShownLun** | **0** | | LowerMMCFrequency | 1 |
| FixedLunIconLow | 0 | | DisableMsProHG2KMode | 0 |
| FixedLunIconHigh | 0 | | Patch5710LPM | 0 |
| WWEnable | 1 | | DisableOcpFor5176EA | 0 |
| SSEnable | 1 | | FixPhyFor5176EA | 0 |
| PoFxIdleTime | 20 | | SEL_SD_SSC_1MHZ | 0 |
| D3ColdEnable | 0 | | ForceHSMode | 0 |
| RTD3Support | 0 | | EnableSDR25 | 0 |
| **pollingInterval** | **100** | | ForceCloseAutoDelink | 0 |
| **CardDetectInterval** | **200** | | ForceGE | 1 |
| **IdleInterval** | **10** | | FixRomSdTxPhase | 1 |
| USBDMinimumTransferTimeout | 300 | | RecoveryForceResetPort | 1 |
| DiskBusType | 7 (USB) | | ForceDisableLPMFor5176E | 1 |
| UsbTransreqType | 2 | | SortingModeFor5176E | 1 |
| TurboEnable | 1 | | phyWriteEnable | 0 |
| KeepDxIgnoreIoctlEnable | 1 | | UrbTimeOut | 10 |
| EnableCprm | 1 | | CSWTimeout | 1 |
| WaitDevPowUpBeforeSuspend | 0 | | CloseLPM1 | 1 |
| SupportXD | 1 | | SmartCardPatchCount | 6 |
| SupportMS | 1 | | BusyWait | 0 |
| SupportMMC | 1 | | FT2_hide_disk | 0 |
| ReloadFSAfterSuspend | 0 | | RTS5355CorePowerUpHS400 | 0 |
| ShowPdoIconAndLabelMode | 1 | | DelayTimeAfterResume | 0 |
| EnableAutoDelink | 0 | | | |

### Findings

1. **Forcing removable media is a no-op.** `MediaRemovable` already defaults
   to `1` and `SetNoneRemovable` to `0`. Setting them explicitly changes
   nothing (confirmed empirically).
2. **`FixedShownLun` defaults to `0`** — the LUN is *not* being forced to look
   fixed by configuration, so the non-removable appearance comes from elsewhere.
3. **`CheckMediaChange` defaults to `0`** (bit 0 of `pParas+0x689`) — media-change
   checking is off unless set. Setting it to `1` + aggressive intervals did **not**
   fix hotplug either.
4. **`EnableAutoDelink` defaults to `0`** but the service key carried `1` left
   over from the 2016 INF. Restoring it to `0` had no observable effect.
5. The defect is therefore not configuration but detection: see
   [The bug SoraCard fixes](#the-bug-soracard-fixes).

## Going back to the vendor driver

1. Install the generic package over SoraCard (with the device present):
   `pnputil /add-driver <extracted cab>\RtsUer.inf /install /force`
2. Optionally remove the SoraCard packages:
   `pnputil /delete-driver oemNN.inf /uninstall /force` for each package whose
   original name is `soracard.inf` (`pnputil /enum-drivers` lists them).

> ⚠️ **Never** run `pnputil /delete-driver <vendor oemNN.inf> /uninstall` for
> the bound vendor package while its service is disabled: on the Windows 11
> install this deleted the `RTSUER` service key and left the reader present
> but unbound. Recovery was re-running `pnputil /add-driver …\RtsUer.inf
> /install /force` with the device present.

Reverting to the vendor driver also brings back Realtek's drive icon/label
helper (`RsCRIcon.dll`), which SoraCard does not replace.
