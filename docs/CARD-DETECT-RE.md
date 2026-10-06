# RtsUer.sys — card-detect reverse-engineering

> **Outcome (2026-10-06).** This is a static analysis of the *vendor* driver,
> kept as reference. Its hypotheses (§8) and recommendations (§9) were written
> before SoraCard talked to the hardware and are **superseded**:
>
> * The RTS5129 has no SCSI firmware — SoraCard drives it with Realtek's
>   register protocol and acts as the SCSI target itself, so §9's "probe with
>   `TEST UNIT READY` over BOT" does not apply to this chip.
> * The controller's card-detect bit is **stuck at "present"** with the slot
>   empty ([`RTS5129-notes.md`](RTS5129-notes.md#card-detect-the-cd-bit-is-stuck)).
>   That explains the vendor symptoms better than the guard-structure
>   hypotheses below: a detector that trusts the CD bit never sees an insert,
>   and only notices a removal when I/O finally fails.
> * What SoraCard actually does (removable LUN that never disappears, CMD13
>   liveness check with transparent re-attach, UNIT ATTENTION on insert) is in
>   [`ARCHITECTURE.md` §6](ARCHITECTURE.md#6-card-detect-recovery-eject-the-actual-fix).
>   §9 items 3 and 4 (stable removable LUN, sense-based change reporting)
>   match what was built; items 1, 2 and 6 do not.

Static analysis of the vendor driver to recover **how it detects SD-card
insert/remove**, so SoraCard can do it correctly.

* Binary: `RtsUer.sys`, 822336 bytes, version `10.0.26100.31289`,
  md5 `9ad2b5d0bd545ebd101d873c483d4498` (extracted from `441d6a27-…_88a2af93….cab`
  in the repository root).
* PE32+ x86-64, image base `0x140000000`. Sections: `.text`
  `0x140001000–0x1400b6b5e`, `.rdata` `0x1400b7000`, `.data` `0x1400bc000`,
  `PAGE` `0x1400c0000`, `INIT` `0x1400c2000`.
* Method: `objdump -d -M intel` (full 213600-line listing), `strings -a/-el`,
  `objdump -p` import table, `.pdata` function ranges, and a small Python
  dataflow scanner. No hand-rolled PE parser (an earlier hand-rolled attempt
  produced garbage).
* **The driver exports no symbols, but it embeds 322 `rts_*` debug/trace
  function names as ASCII strings.** Matching each name's string VA to the
  `lea … # 0x…` that references it yields a near-complete symbol map. This is
  the single most valuable artifact of this analysis — all function names
  below are recovered this way and marked `rts_*`.

---

## 0. TL;DR — the mechanism in one paragraph

Card presence is **polled**, not interrupt-driven. At device start the driver
spawns a dedicated system thread, `rts_carddetect_thread` (`0x14003b340`), via
`rts_start_carddetectthread` (`0x14003d4ec`). That thread waits for a "device
started" event, then loops: **sleep `CardDetectInterval` ms**
(`[info+0x20]`, default **200 ms**), probe the card, and if the media state or
card identity changed, publish the change to PnP with
**`IoInvalidateDeviceRelations(pdo, 0)`**. The probe is a SCSI
**`TEST UNIT READY`** (CDB `0x00`) sent over Bulk-Only Transport — the driver
re-implements BOT itself (it builds CBW signature `'USBC'` = `0x43425355`) —
plus a vendor CDB and an SD CID/CSD comparison
(`rts_get_card_id_info` → `rts_check_card_id_status_changed`). There are **no
kernel timers at all** (`KeSetTimer`/`KeInitializeTimer`/`KeSetTimerEx`/
`KeCancelTimer` are absent from the import table); all timing is
`KeDelayExecutionThread` inside worker threads.

The reason the vendor driver is broken is therefore **not** "polling is
disabled" in the abstract: the machinery exists and uses sane 100–200 ms
intervals. The failure is in the **gating and notification path** — the poll
thread only re-evaluates a media-change bitmask (`[ctx+0x768]`) that is set by
narrowly-guarded helpers, and (as measured on the live system) the driver
presents **no LUN at
all** when the card is out, so PnP has nothing to re-notice. Details and ranked
hypotheses in §8.

---

## 1. Import table — what the driver is, and is not

Full `objdump -p` import list (135 functions). This shapes everything:

| Fact | Evidence | Consequence |
|---|---|---|
| **No `KeSetTimer` / `KeInitializeTimer*` / `KeSetTimerEx` / `KeCancelTimer`** | absent from `ntoskrnl` imports (only `KeInitializeEvent`, `KeSetEvent`, `KeClearEvent`, `KeInitializeMutex/Semaphore`, `KeDelayExecutionThread`) | timing is thread + `KeDelayExecutionThread`, i.e. **polling**, not timer callbacks |
| **No `StorPort*` / `ScsiPort*` imports** | absent | monolithic driver: it implements BOT + SCSI itself and creates its own FDO/PDOs |
| Threads/work-items present | `PsCreateSystemThread` (3 sites), `PsTerminateSystemThread` (3), `IoAllocateWorkItem`/`IoQueueWorkItem` (10) | 3 worker threads + work-item offload |
| PnP notification primitives | `IoInvalidateDeviceRelations` (3 sites), `IoRegisterDeviceInterface` (1), `IoSetDeviceInterfaceState` (5), `IoRegisterShutdownNotification` | media change is published by **invalidating device relations** |
| Bulk/control USB | only `USBD_CreateConfigurationRequestEx`, `USBD_ParseConfigurationDescriptorEx` (rest is hand-built URBs) | driver builds CBW/URBs itself |
| `IoCreateDevice`, `IoAttachDeviceToDeviceStack`, `IoBuildSynchronousFsdRequest` | present | FDO + child PDO stack |

The thread entry points (from `PsCreateSystemThread` call sites):

| Thread | Entry (start routine) | Created by | Handle field | Start context |
|---|---|---|---|---|
| card-detect | **`rts_carddetect_thread` `0x14003b340`** | `rts_start_carddetectthread` `0x14003d4ec` @ `0x14003d5b1` | `ctx+0xb0` | device extension |
| CSQ / pending-IRP | `0x14003ca80` (`IoCsqRemoveNextIrp`, `KeWaitForMultipleObjects`) | `0x14003d690` @ `0x14003d72d` | `ctx+0xa8` | device extension |
| monitor | `0x14003ce00` (polls `pollingInterval`) | `0x14003d7e4` @ `0x14003d8a5` | `ctx+0xa0` | device extension |

`rts_start_carddetectthread` (`0x14003d4ec`) first calls
`rts_carddetect_needed` (`0x14003b188`), and only creates the thread if it
returns true; it signals `ctx+0xb8` (an event) and then issues
`PsCreateSystemThread(…, StartRoutine = 0x14003b340, StartContext = ctx)`:

```
14003d563  call 0x14003b188            ; rts_carddetect_needed(ctx)
14003d56a  je   0x14003d629            ; -> do not start thread
14003d570  lea  rcx,[rbx+0xb8]         ; signal "started" event
14003d57c  call [KeSetEvent]
14003d582  lea  rax,[rip-…] # 0x14003b340   ; rts_carddetect_thread
14003d589  mov  [rsp+0x30],rbx         ; StartContext = ctx
14003d58e  mov  [rsp+0x28],rax         ; StartRoutine
14003d5b1  call [PsCreateSystemThread]
```

`rts_stop_carddetectthread` = `0x14003d998..0x14003dae5`.

---

## 2. Where the registry parameters live: `info = ctx + 0x5c0`

The previous notes recovered the loader defaults but could not find consumers.
The missing link is one allocation:

```
14002aec6  lea  rbx,[rdi+0x5c0]      ; rbx = &ctx->params
14002aed2  mov  r8d,0xf6
14002aed8  call 0x140097f00          ; memset(ctx+0x5c0, 0, 0xf6)
14002aedd  mov  rcx,rdi
14002aee0  call 0x140025894          ; rts_load_driver_parameters
14002aee5  mov  [rdi+0x6b8],rbx      ; ctx->info = ctx+0x5c0   <-- KEY
```

So the ubiquitous `info` pointer (`[ctx+0x6b8]`, referenced 257×) is just a
pointer **to the parameter block at `ctx+0x5c0`**. Therefore every
"`[info+off]`" access in the driver is really `[ctx + 0x5c0 + off]`. This is why
a naive search for `[reg+0x5e0]` found nothing: the functional code always goes
through the `info` pointer.

Two loaders, selected by device family in the dispatcher `0x14001eadc`
(`al = BYTE [rcx+0x90]`; `0` → USTOR loader, `1` → UVSTOR loader, else no-op):

| Symbol | Range | Registry key |
|---|---|---|
| `rts_load_dev_parameters_ustor` | `0x14001eb04..0x140021626` | `\REGISTRY\Machine\System\CurrentControlSet\Services\RTSUER\USTOR` |
| `rts_load_dev_parameters_uvstor` | `0x140021628..0x140025892` | `…\RTSUER\UVSTOR` |
| `rts_load_driver_parameters` | `0x140025894..0x140025bb0` | `…\RTSUER\Parameters` |

The live registry shows the device uses **UVSTOR**, so loader B
(`rts_load_dev_parameters_uvstor`) is the relevant one. Both loaders are the
same code shape: `query(name) → mov [rdi+off],eax`; byte params use
`mov BYTE PTR [rdi+off],al`, and booleans are packed (`or DWORD PTR [rdi+o],eax`).

### 2.1 Parameter → offset → consumer (UVSTOR loader B)

`off` is relative to `info` (= `ctx+0x5c0`); `ctx` column = `0x5c0+off`.
"Consumer" is the instruction that reads the field back at runtime (excluding
the loader itself), found with a dataflow scan that tracks the register holding
`[ctx+0x6b8]`.

| Parameter | `info+` | `ctx+` | Default | Runtime consumer |
|---|---|---|---|---|
| `RemovableControl` | `0x07` (byte) | `0x5c7` | `0x18` (24) | `0x14002de9c`, `0x140030941` |
| `SetNoneRemovable` | `0x0a` (byte) | `0x5ca` | 0 | `0x140025d4b` |
| `FixedLunIconLow` | `0x11` | `0x5d1` | 0 | (icon path) |
| `FixedLunIconHigh` | `0x15` | `0x5d5` | 0 | `0x14001b3a8/ab` |
| **`pollingInterval`** | `0x1c` | `0x5dc` | **100** | **`0x14003ce7f`, `0x14003d009`** (monitor-thread sleep) |
| **`CardDetectInterval`** | `0x20` | `0x5e0` | **200** | **`0x14003b413`** (carddetect-thread sleep) |
| `IdleInterval` | `0x24` | `0x5e4` | 10 | loader only (clamped `>=1` at `0x14001fa0b`) |
| `USBDMinimumTransferTimeout` | `0x28` | `0x5e8` | 300 | `0x14003e99d` |
| `TURRetryTimes` | `0x38` (byte) | `0x5f8` | 3 | `0x14003b394` (carddetect retry bound) |
| `DelinkLatency` | `0x3a` | `0x5fa` | 0 | `0x14003c376`, `0x140046129`, … |
| `FirstLoadDelinkLatency` | `0x3e` | `0x5fe` | 600000 | `0x14003c37f` |
| `ForceDelinkLatency` | `0x42` | `0x602` | 0 | `0x14003c2fd` |
| `MaxTransferLength` | `0x46` | `0x606` | 1048576 | `0x14000f795` |
| **`MediaRemovable`** | `0x4b` (byte) | `0x60b` | **1** | **`0x14002b74b`, `0x140030cad`** |
| **`PollingPipe`** | `0xc4` (byte) | `0x684` | 0 | **`0x14007a45e`** |
| **`CheckMediaChange`** | `0xc9` (dword bit) | `0x689` | 0 | **`0x14007a4d3`** |
| `DelayTimeAfterResume` | `0xea` | `0x6aa` | 0 | `0x1400365ca`, `0x14003661e` |
| (un-named, next param `ForceLedAlwaysOn`) | `0xe1` | `0x6a1` | — | `0x14007a509` |
| (slow-poll flag) | `0xf2` | `0x6b2` | — | `0x14003b421`, `0x14003ea46` |

Notes / corrections to the earlier notes:

* `CheckMediaChange` is **not** bit 0. The loader computes
  `eax = (value == 1) ? 0x01000000 : 0` and does
  `or DWORD PTR [rdi+0x689],eax` (`0x1400252fb–0x14002531c`). So it is **bit 24**
  of the dword at `info+0xc9`.
* `MediaRemovable` is consumed when the child PDO's removable flag/capability
  is derived:
  ```
  140030ca5  mov  rax,[r12+0x6b8]
  140030cad  cmp  BYTE PTR [rax+0x4b],dil   ; MediaRemovable
  140030cb1  je   short skip
  140030cb7  or   DWORD PTR [r14+0x34],0x1  ; set removable bit on PDO
  ```
  and in the interface/register path at `0x14002b74b` where it is turned into a
  `0x180`/`0x181` selector for the call at `0x1400c008c`.
* `PollingPipe` and `CheckMediaChange` are read together in the parameter→
  firmware-command descriptor builder at `0x140079f90..0x14007a573` (see §5.3).

---

## 3. The card-detect thread, step by step

`rts_carddetect_thread` = `0x14003b340..0x14003c787` (~3.5 KB of code).

Prologue/startup:

```
14003b371  mov  rax,[rcx+0x6b8]      ; info
14003b37b  mov  r15,rcx              ; r15 = ctx
14003b38d  ...
14003b394  movzx eax,BYTE PTR [rax+0x38]   ; TURRetryTimes -> [rbp+3]
14003b382  add  rcx,0xd0
14003b3b0  call [KeWaitForSingleObject]    ; wait for ctx+0xd0 ("device started")
```

Compute the poll timeout from `CardDetectInterval` (`info+0x20`), converting
milliseconds to a **negative relative LARGE_INTEGER** (100 ns units):

```
14003b40c  mov  rcx,[r15+0x6b8]          ; info
14003b413  mov  eax,DWORD PTR [rcx+0x20] ; <-- CardDetectInterval (default 200)
14003b416  imul rax,rax,0xffffffffffffd8f0  ; * -10000  (ms -> 100 ns, negative)
14003b41d  mov  [rbp+0xf],rax            ; LARGE_INTEGER timeout
14003b421  cmp  BYTE PTR [rcx+0xf2],r12b ; "slow" flag?
14003b428  je   0x14003b47a
14003b42a  add  rax,rax                  ; if set, double the interval
14003b42d  mov  [rbp+0xf],rax
...
14003b4a5  call [KeDelayExecutionThread] ; <-- the poll sleep
```

The same idiom appears in the monitor thread with `pollingInterval`:

```
14003ce71  mov  rax,[rbx+0x6b8]
14003ce7f  mov  ecx,DWORD PTR [rax+0x1c]  ; pollingInterval (default 100)
14003ce82  imul rax,rcx,0xffffffffffffd8f0
14003ce90  call [KeDelayExecutionThread]
...
14003d009  mov  ecx,DWORD PTR [rax+0x1c]  ; again after processing
14003d01a  call [KeDelayExecutionThread]
```

Main body (slot loop + notification). `ctx+0x768` is a **bitmask of slots whose
media state changed**; `ctx+0x76a` is the current slot; `ctx+0x3b0` gates the
whole loop; `ctx+0x418 + slot*8` is the per-LUN object pointer:

```
14003b641  cmp  BYTE PTR [r15+0x3b0],0
14003b649  jle  0x14003c1e2                 ; no LUNs -> nothing to do
...
14003b667  dl = 1 << slot
14003b67d  test BYTE PTR [r15+0x768],dl     ; slot marked changed?
14003b684  je   0x14003b6fd
14003b686  ... DbgPrint "rts_carddetect_thread" ...
14003b6d3  mov  rcx,[r15+0x18]              ; ctx->pdo
14003b6d7  xor  edx,edx
14003b6d9  call [IoInvalidateDeviceRelations]   ; <-- publish change to PnP
14003b6df  movzx eax,[r15+0x768]
14003b6e7  btr  eax,ebx                     ; clear the slot's change bit
14003b6ed  mov  [r15+0x768],al
```

For slots not flagged, it fetches the per-LUN object and, if `[info+0xd2] == 1`,
calls the change checker `0x14003c930` (which itself calls
`rts_test_unit_ready` and, on a transition, `IoInvalidateDeviceRelations` at
`0x14003ca3a`). It then calls the vendor read `0x1400458e0`, compares SD CID
data with `RtlCompareMemory` (`0x14003bc32`), and invokes the identity/type
change helpers `rts_check_card_type_changed` (`0x14003bfc3`) and
`rts_check_card_id_status_changed` (`0x14003bfce`). Finally it can tear the
thread down (`PsTerminateSystemThread`, `0x14003c6a0`).

### 3.1 `rts_carddetect_needed` (`0x14003b188`) — the start gate

```
info = [ctx+0x6b8]
r12 = 0
if (BYTE [ctx+0x3b0] > 0) {                 ; LUN(s) configured
    if (BYTE [info+0x2] != 0)  goto mark;
    if (BYTE [ctx+0x4f8] == 0) return 0;    ; not removable? -> no polling
    if (BYTE [info+0x37] == 0) return 0;
mark:
    if (BYTE [info+0x1] == 0) r12 = 1;
}
return r12;
```

If this returns false the card-detect thread is **never created**. `info+0x1/2/
0x37` and `ctx+0x4f8/0x3b0` are chip/instance capability flags set by the
chip-specific init code (not the registry params we recovered). This is a prime
suspect for "insert never detected" (§8).

### 3.2 Change signalling — `rts_check_card_id_status_changed` / `_type_changed`

These two helpers are the only writers of the pending-change bitmask
`ctx+0x768` (besides its clear in the thread):

| Function | Range | Sets |
|---|---|---|
| `rts_check_card_id_status_changed` | `0x14001853c..0x140018747` | bit `0x200` in `ctx+0x768` when CID data changed |
| `rts_check_card_type_changed` | `0x140018748..0x1400188b3` | bit `0x200` in `ctx+0x768` when card type changed |

Both early-return unless a stack of flags is set. For the CID change checker:

```
14001857b  rax = [ctx+0x6b8]
140018582  cmp BYTE [rax+0x9],0        ; info+0x9
140018586  je  out
140018590  cmp BYTE [obj+0x8c],0       ; per-LUN "mounted/id"
140018597  je  out
14001859d  cmp BYTE [ctx+0x6cd],0
1400185a4  je  out
1400185aa  cmp BYTE [ctx+0x6ca],0
1400185b1  je  out
1400185d0  call 0x14001a4f8            ; rts_get_card_id_info(ctx, slot, …)
1400185f1  call 0x1400979f0            ; memcmp(card id, stored id)
1400185f8  je  out
140018627  mov r15d,0x200              ; prepare the 0x200 pending bit
...        (log + OR into ctx+0x768)
```

The type checker additionally requires `info+0x3 != 0` (`0x140018837`) and calls
`0x140028f7c` to compare card types.

---

## 4. The probe: SCSI `TEST UNIT READY` over BOT

`rts_test_unit_ready` = `0x140045aa8..0x140045bf2`. It builds a **6-byte SCSI
CDB** and dispatches it through the BOT path:

```
140045b9d  and  DWORD PTR [rsp+0x48],0x0    ; zero CDB
140045bbb  al = dl (lun) << 5
140045bb8  mov  BYTE PTR [rsp+0x51],al      ; CDB[1] = LUN<<5
140045bb2  mov  r9d,6                       ; CDB length = 6
140045bd2  call 0x140038bc8                 ; BOT executor
```

`[rsp+0x50]` (CDB[0]) is left `0x00` → **`TEST UNIT READY`**. The BOT executor
`0x140038bc8` allocates a `'SCMD'` command block and fills a Command Block
Wrapper with the BOT signature `'USBC'`:

```
140038c00  mov  r8d,0x444d4353    ; 'SCMD' pool tag
140038c06  call [ExAllocatePoolWithTag]
140038cae  mov  DWORD PTR [rbp+0x508],0x43425355   ; CBW signature "USBC"
140038d1f  call 0x14003e6d0       ; rts_bulkonly_startio
```

`rts_bulkonly_startio` = `0x14003e6d0..0x14003e949`;
`rts_bulkonly_reset_recovery` = `0x14003e464`; bulk timeout helper
`rts_bulk_txrx_timeout_urb_mdl` = `0x14003e2f0`. So card presence is determined
by whether the bulk `TEST UNIT READY` succeeds — a card-out condition returns
CHECK CONDITION / not-ready, which the driver turns into sense data (§7).

`rts_test_unit_ready` also has a firmware-vs-driver short-circuit: when
`[info+0xf3] == 1` it first issues an extra vendor command via `0x140045bf4`
with command word `0xc080`, then proceeds with the TUR.

---

## 5. Vendor commands, CID/CSD reads, and the interrupt endpoint

### 5.1 Vendor control requests

* `rts_build_vendor_request_urb` = `0x14003df84..0x14003e113`: allocates an
  `0x88`-byte request block tagged `'MC12'`
  (`mov r8d,0x3231434d`), sets the URB header (`Length=0x88`, function byte
  `0x17`) and the setup fields (`[rsi+0x80]` bRequestType, `[rsi+0x81]`
  bRequest, `[rsi+0x82]` wValue, `[rsi+0x84]` wIndex), then submits through
  `0x14003e958`.
* `rts_usb_interface_control_in` = `0x140045110..0x1400453a6`;
  `rts_usb_interface_control_out` = `0x140044db0..0x140045106` — interface-level
  vendor control transfers (register read/write used for chip bring-up: see
  `rts_ctrl_fdo_read_register` / `rts_ctrl_fdo_write_register`).
* The single caller of `rts_build_vendor_request_urb` is the small wrapper
  `0x140007524` (called from the XD/CF register path), which sets
  `bRequest = 2`, `r9b = 0x40`.

### 5.2 Vendor SCSI CDBs

Two thin CDB builders are used by the card-detect/identity paths:

* `0x140045bf4`: builds a CDB whose first word is `0x0ef0` (bytes `F0 0E`) and
  carries a caller-supplied `wValue`-like parameter; used by
  `rts_test_unit_ready` (`0xc080`) and by
  `rts_config_autodelink_by_vendor_cmd` (`0x1400453a8`).
* `0x1400458e0`: first word `0x09f0` (bytes `F0 09`), reads 16 bytes into
  `ctx+0x37c`, stores a response byte into `ctx+0x38c`. Called from
  `rts_carddetect_thread` at `0x14003b7eb`.

`0xF0` is a **vendor-specific SCSI opcode**, so these are vendor "get status /
get card info" commands distinct from the standard `TEST UNIT READY`.

### 5.3 SD identity (CID/CSD) comparison

`rts_get_card_id_info` = `0x14001a4f8..0x14001a6ac`, with variants
`rts_get_card_id_info_for_driver_based` (`0x14004986c`) and
`rts_get_card_id_info_for_firmware_based` (`0x140049964`). The card-detect
thread calls it and memcmps the result (`0x1400185f1`), and compares raw 16-byte
blocks with `RtlCompareMemory` at `0x14003bc32`. The per-card-type readers
(`rts_get_sd_card_id`, `rts_get_sd_card_id_3882/_5888`, `rts_get_ms_card_id_*`,
`rts_get_card_info_with_mac`/`_with_no_mac`) sit under it.

### 5.4 Interrupt endpoint — findings

The driver **implements Bulk-Only Transport on the bulk pipes**; card presence
is obtained by issuing SCSI/vendor commands, not by waiting on an interrupt
URB. Concretely:

* The SCSI transfer path explicitly references endpoint address **`0x83`**
  (`cmp al,0x83` at `0x1400388e4`, `cmp BYTE PTR [rcx],0x83` at
  `0x140038a25`) to select a 56-byte (`0x38`) buffer at `0x1400bcb90`.
* `mov BYTE PTR [rsp+0x20],0x81/0x82/0x83` sites (`0x14004fb9f`,
  `0x14004fee8`, `0x1400631cb`, `0x140069cfd`, `0x14008f3aa`) pass endpoint
  addresses into the transfer helper `0x140074160` — these are chip-family
  specific register/bulk endpoints (0x81/0x82), not a card-detect notification.
* The configuration is parsed with `USBD_ParseConfigurationDescriptorEx` /
  `USBD_CreateConfigurationRequestEx` (`0x1400422a2`, `0x140042328`), which sets
  up all three pipes; there is **no code path that arms a persistent interrupt
  IN URB with a completion routine that signals media change.** The monitor
  thread and card-detect thread are woken by timeouts, not by pipe events.
* `PollingPipe` (default 0) is only copied into the firmware command descriptor
  built by `0x140079f90` (`info+0xc4 → [rdi+0x2164]`); it selects which pipe
  the device firmware should poll, not a host-side interrupt wait.

**Conclusion:** in this driver, card-detect is 100 % poll-based
(thread + `KeDelayExecutionThread` + TUR/vendor CDB). If the RTS5129 offers an
interrupt endpoint suitable for card-detect, this driver does not use it for
that purpose. (The 3-pipe descriptor and the `0x83` reference are consistent
with bulk IN = `0x83` used for BOT data; a vendor command over bulk is the card
status read.)

---

## 6. Media-change → PnP / storage-stack notification

### 6.1 PnP device-relations invalidation

The driver is a bus-like function driver: each LUN is a child PDO
(`rts_create_child_pdo` = `0x14002b6a8`, child list returned by
`rts_pnp_pdo_query_dev_relations` = `0x140030f1c`). Media change is published
by calling `IoInvalidateDeviceRelations(ctx->pdo, 0)`, which forces PnP to
re-issue `IRP_MN_QUERY_DEVICE_RELATIONS(BusRelations)`:

| Site | Context |
|---|---|
| `0x14003b6d9` | `rts_carddetect_thread`: slot's `ctx+0x768` change bit set |
| `0x14003c193` | `rts_carddetect_thread`: insert/mount transition |
| `0x14003ca3a` | `0x14003c930` change checker (TUR transition) |

`rts_stop_carddetectthread` / `rts_device_will_delink` (`0x14003c788`) tear down
on surprise removal.

### 6.2 SCSI sense / UNIT ATTENTION

`rts_analysis_request_sense_data` = `0x140036f70..0x140037204` interprets
returned sense data and transitions the driver's cached media state. The
embedded trace strings spell out the state machine:

| String | VA |
|---|---|
| `Media changed found for scsi %s` | `0x14009c0a0` |
| `both original cmd and request sense fail` | `0x14009c110` |
| `change request sense from media present to No media` | `0x14009c180` |
| `change request sense from media present to Media changed` | `0x14009c1e0` |
| `change request sense from media changed to No media` | `0x14009c230` |
| `change request sense from media changed to media changed` | `0x14009c270` |

The handlers at `0x14000f230..0x14000f563` and `0x14001024c` use sense ASC
`0x3A` (`MEDIUM NOT PRESENT`) — e.g. `cmp cl,0x3a` at `0x140036ff6` and
`mov DWORD PTR [rbp-0x1d],0x3a` at `0x140037a9d` — and report the transition to
the storage stack. Trace strings also show the SRB path:
`SRB_FUNCTION_EXECUTE_SCSI,The SRB is %s,Irp is 0x%p,Insert IRP to Queue`
(`0x14009b830`) and
`PASSIVE_LEVEL SCSIOP_MEDIUM_REMOVAL …` (`0x14009b790`).

`rts_ctrl_pdo_sffdisk_device_command` = `0x14000da18..0x14000e7d6` is the
per-PDO control/IOCTL handler; it contains explicit early-outs
`… card not exist,return` (`0x14009a670`) and `Card exist == 0`
(`0x14009ab70`), i.e. the LUN's "is there media?" gate.

### 6.3 Device interface

`IoRegisterDeviceInterface` (`0x14002b501`) + `IoSetDeviceInterfaceState`
(5 sites, e.g. `0x14002e7ed`, `0x14002f63e`) publish/retire the disk interface
on the child PDO. The driver also creates `\??\STORAGE#RemovableMedia#`
(`0x1400a17b0`) symlinks. `MediaRemovable` feeds the removable capability
(`or DWORD PTR [r14+0x34],0x1` at `0x140030cb7`).

---

## 7. Full recovered symbol map (card-detect relevant)

All ranges from `.pdata`; names from embedded trace strings.

```
rts_load_dev_parameters_ustor            0x14001eb04..0x140021626
rts_load_dev_parameters_uvstor           0x140021628..0x140025892   <- our device
rts_load_driver_parameters               0x140025894..0x140025bb0
rts_dev_accessible                       0x140019d4c..0x140019e85
rts_check_card_id_status_changed         0x14001853c..0x140018747
rts_check_card_type_changed              0x140018748..0x1400188b3
rts_detect_reset_card_status             0x1400197b0..0x140019d4c
rts_get_card_id_info                     0x14001a4f8..0x14001a6ac
rts_get_card_id_info_for_driver_based    0x14004986c..0x140049963
rts_get_card_id_info_for_firmware_based  0x140049964..0x140049ae8
rts_get_card_info_with_mac               0x14001a754..0x14001a967
rts_get_card_info_with_no_mac            0x14001a968..0x14001abf2
rts_fix_card_cd_edge_deglitch_width      0x140001a90..0x140001bd9
rts_fix_sd_card_insert                   0x14000228c..0x1400025f5
rts_start_carddetectthread               0x14003d4ec..0x14003d68f
rts_stop_carddetectthread                0x14003d998..0x14003dae5
rts_carddetect_needed                    0x14003b188..0x14003b33d
rts_carddetect_thread                    0x14003b340..0x14003c787   <- poll loop
rts_device_will_delink                   0x14003c788..0x14003c92d
rts_test_unit_ready                      0x140045aa8..0x140045bf2   <- probe
rts_build_vendor_request_urb             0x14003df84..0x14003e113
rts_control_pipe_transfer                0x14003ef2c..0x14003f133
rts_usb_interface_control_in             0x140045110..0x1400453a6
rts_usb_interface_control_out            0x140044db0..0x140045106
rts_bulk_txrx_timeout_urb_mdl            0x14003e2f0..0x14003e461
rts_bulkonly_startio                     0x14003e6d0..0x14003e949
rts_bulkonly_reset_recovery              0x14003e464..0x14003e6cf
rts_analysis_request_sense_data          0x140036f70..0x140037204
rts_ctrl_pdo_sffdisk_device_command      0x14000da18..0x14000e7d6
rts_create_child_pdo                     0x14002b6a8..0x14002b9b1
rts_pnp_pdo_query_dev_relations          0x140030f1c..0x14003123c
rts_pnp_pdo_should_be_reported_exist     0x1400323c8..0x1400325f8
rts_pnp_pdo_surprise_removal             0x1400325f8..0x1400329b1
```

Unnamed but important helper ranges used above:
`0x14001eadc` loader dispatch; `0x14002aee5` `info = ctx+0x5c0`;
`0x140038bc8` BOT CBW builder; `0x14003c930` TUR-transition checker;
`0x140079f90..0x14007a573` param→firmware command descriptor builder.

---

## 8. Why the vendor driver misses insert / is slow on remove — ranked hypotheses (superseded, see Outcome)

The mechanism exists and the intervals are short (200 ms / 100 ms). The bug is
in gating and state publication. Hypotheses, most likely first:

1. **The card-detect thread is never created** because
   `rts_carddetect_needed` (`0x14003b188`) returns false. Its result depends on
   runtime capability bytes (`info+0x1`, `info+0x2`, `info+0x37`,
   `ctx+0x4f8`, `ctx+0x3b0`) that are set by chip-specific init and are not the
   exposed registry switches. If any is wrong for this RTS5129 instance, no
   polling happens at all — which matches "insert is *never* detected".
   *Test:* breakpoint/log `rts_carddetect_needed` and the
   `rts_start_carddetectthread` `je` at `0x14003d56a`.

2. **The pending-change bitmask is never set.** `IoInvalidateDeviceRelations`
   only fires when bit `0x200` of `ctx+0x768` is set, and only
   `rts_check_card_id_status_changed` / `rts_check_card_type_changed` set it —
   both guarded by `info+0x9`, `ctx+0x6cd`, `ctx+0x6ca`, per-LUN `obj+0x8c`
   (`+info+0x3` for type). If a guard is false, the driver detects the card but
   never tells PnP. This matches "PnP sees nothing on insert".

3. **No LUN exists while the card is out** (confirmed empirically in the
   on the live system: `USBSTOR count = 0`). PnP device-relations invalidation is a no-op
   when the driver reports zero children, so there is nothing for the storage
   stack to re-evaluate; only after the child PDO is (re)created does the disk
   appear. A child-PDO-per-LUN bus driver must keep the LUN present with
   `RMB=1`/no-media, or reliably re-create it on insert.

4. **The removable capability is not propagated.** Although `MediaRemovable`
   defaults to 1 and `or DWORD PTR [r14+0x34],0x1` exists at `0x140030cb7`, the
   LUN was measured with `Capabilities = 0x14` (no removable bit). If that
   bit is cleared elsewhere, the volume/media layer will not run normal
   removable-media change detection.

5. **Removal delay is a timeout, not a poll.** The ~2 min 13 s removal delay is
   consistent with the detect thread not running (H1/H2) and removal being
   noticed only when the storage stack's I/O finally fails. Relevant timeouts:
   `USBDMinimumTransferTimeout` 300, `UrbTimeOut` 10, `CSWTimeout` 1.

None of these can be confirmed without a kernel trace; they are static
inferences from the guard structure, and H1/H2 are directly falsifiable with
the addresses above.

---

## 9. What SoraCard should do (superseded, see Outcome)

1. **Poll with a dedicated worker, not a timer.**
   `PsCreateSystemThread` (or `IoQueueWorkItem` on a periodic work item) whose
   body does, in a loop:
   `KeDelayExecutionThread(KernelMode, FALSE, &timeout)` where
   `timeout.QuadPart = -(interval_ms * 10000)`, `interval_ms ≈ 100–250`.
   (Exactly the vendor idiom: `imul rax, rax, -10000` at `0x14003b416`.)

2. **Probe with SCSI `TEST UNIT READY` over BOT.**
   Send CDB `{0x00, LUN<<5, 0, 0, 0, 0}` on the bulk pipe; build a real CBW with
   signature `0x43425355` (`'USBC'`), tag, transfer length, flags, LUN,
   CDB length 6. TUR is the cheapest reliable "is there media?" probe and needs
   no vendor-specific knowledge. Retry `TURRetryTimes` times (vendor default 3)
   before deciding absent.

3. **Keep the LUN present with removable semantics — the key fix.**
   Enumerate the child PDO (disk) **even with no card**, report
   `INQUIRY.RMB = 1` and the removable capability bit, and return
   `TEST UNIT READY` → CHECK CONDITION with sense key `0x02`/ASC `0x3A`
   (`MEDIUM NOT PRESENT`) when there is no media. Do **not** present zero
   children when the card is out; a stable PDO is what lets PnP notice change
   without a device restart (this is precisely what the vendor driver fails to
   provide).

4. **Publish the change through the storage/PnP stack.**
   On a probe transition, cache a pending-change flag and:
   * as a bus driver, call `IoInvalidateDeviceRelations(pdo, 0)` (the vendor's
     call at `0x14003b6d9`), or
   * as a LUN/function driver, complete the next `TEST UNIT READY`/`INQUIRY`
     with `SRB_STATUS_AUTOSENSE_VALID` + sense key `UNIT ATTENTION` (`0x06`),
     ASC `0x28` (`NOT READY TO READY CHANGE`) on insert and ASC `0x3A` on
     remove, so `disk.sys`/`partmgr` re-scan.
   The vendor equivalent is `rts_analysis_request_sense_data`
   (`0x140036f70`) and the "request sense from media present to …" state
   machine (§6.2).

5. **Debounce.** The vendor has `rts_fix_card_cd_edge_deglitch_width`
   (`0x140001a90`) and `rts_fix_sd_card_insert` (`0x14000228c`) — sample the
   probe N times / require stable state for 1–2 poll periods before
   publishing, to avoid churn on contact bounce.

6. **Do not rely on the interrupt endpoint for detect** unless a separate
   experiment proves it emits card events. This driver never arms an interrupt
   URBs for that purpose; `0x83` is used in the BOT/data path and `PollingPipe`
   is a firmware-side pipe selector.

7. **If the vendor register protocol is wanted**, the shortest path is
   `rts_build_vendor_request_urb` (`0x14003df84`, `'MC12'`) for control
   register reads, and the vendor SCSI opcode `0xF0` CDBs built at
   `0x140045bf4` (`F0 0E`) / `0x1400458e0` (`F0 09`). But TUR + removable LUN
   is sufficient for prompt insert/remove and avoids depending on undocumented
   silicon behaviour.

---

## 10. Verification tools used (reproducible)

```
objdump -h RtsUer.sys                      # sections
objdump -p RtsUer.sys                      # import table -> IAT slot names
objdump -d -M intel RtsUer.sys > disasm.txt
strings -a -t x RtsUer.sys                 # ASCII (incl. rts_* trace names)
strings -a -el -t x RtsUer.sys             # UTF-16 (registry param names)
python3  # .pdata function ranges + dataflow scan for [ctx+0x6b8]+off
```

The `rts_*` trace names are the crucial lever: each is referenced by a
`lea … # <VA>` whose enclosing `.pdata` range gives the function. Any future
work on this binary should start from that name→address map rather than from a
hand-rolled PE parser (an earlier attempt at one failed).
