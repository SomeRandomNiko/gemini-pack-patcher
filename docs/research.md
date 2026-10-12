# Research notes

How the patches were found, what they change, and how the patcher rebuilds the firmware.
All offsets refer to the stock **ltau_n AR560R4** OTA
(`sha256 3ef37f8e…9223`, 85,555,200 bytes).

## The device

A [Roadtop CarPlay / Android Auto retrofit unit for Audi](https://it.aliexpress.com/item/1005004428235119.html) (a "decoder" box
between the car's MMI and the screen), firmware family "ltau_n", based on a SunPlus 8368
SoC (ARMv7, Thumb-2 code). It runs Linux with a Qt application stack. The box talks to
the car through a separate MCU (`mcusrv` relays its messages), which reads the CAN bus
and switches the box's power. Updates come as a single `GEMINI_PACK.BIN` on a USB stick,
started from the settings page (*System update*, password `8032`) or over the air.

## The update container (`GEMINI_PACK.BIN`)

| Offset | Content |
|---|---|
| `0x000` | Header, magic `GEMINI`. `0xd8`: length (LE u32), `0xdc`: offset (LE u32) of the region covered by the ASCII MD5 at `0xea` (preceded by the tag `MD5SUM` at `0xe0`). |
| `0x400` | The ISP image (`$isp_image_header_offset`). At `+0x20` a U-Boot script uImage (the *init script*) that loads the *main script*. |
| main script | A U-Boot script uImage (header CRC + data CRC). It checks the partition table, then writes each partition. |

For each partition the main script contains `echo Updating <name> ...`, followed by
chunks of up to 2 MiB:

```
setexpr isp_update_file_offset $isp_image_header_offset + 0x34bbc00 && ...
fatload $isp_if $isp_dev $isp_ram_addr $isp_update_file_name 0x200000 $isp_update_file_offset
...
md5sum $isp_ram_addr 0x200000 md5sum_value
if test "$md5sum_value" = 29efaa359ba1a656fd6ca86e7b06c400 ; then
```

So every chunk is MD5-checked after it has been written to flash. Nothing in the
container is signed, so it can be modified as long as all checksums are kept consistent:

1. Write the new partition bytes into its chunks. The partition size must not change.
2. Replace each chunk's MD5 inside the script text. It is the same length, so nothing moves.
3. Recompute the main script's uImage data CRC and header CRC.
4. Recompute the header MD5.

`gpp verify` performs all of these checks; `gpp extract` dumps every partition.

The application lives in the **`spapp`** partition (32 MiB slot; 23,576,576 bytes of chunks):
a SquashFS 4.0 image, LZO (`lzo1x_999`, level 9), 32 KiB blocks, mounted at
`/application`. Its `appinfo.rc` (an Android-init-style file) starts the services, among
them `Launcher` (the main UI process) and `mcusrv`.

## Patch 1: keep Android Auto running after ignition off

**Symptom:** switching the engine off immediately ends Android Auto (and the phone's
Bluetooth/Wi-Fi connection), although the unit stays powered for a while.

**Finding the code:** the libraries are not stripped of dynamic symbols, so names are
readable (`llvm-nm -DC`). The MCU's ignition (ACC) state arrives as a CAN message
(IDs `0x1e` / `0x32`) handled by `spappframework::KnobManager::updateUpMsg` in
`lib/libappframework.so.1.0.0`. On a change of the ACC state it calls
`GpBTDevManager::connectAcconoff(bool)`:

```
5012e: blx  KnobController::keyEvent
50132: ldr.w r3, [pc, #0x49c]        ; GpBTDevManager instance
50136: movs  r1, #0x0                ; ACC off -> connectAcconoff(false)
50138: ldr   r3, [r5, r3]
5013a: ldr   r0, [r3]
5013c: blx   GpBTDevManager::connectAcconoff(bool)   <-- patched
50140: b     0x50154
50142: cmp   r1, #0x1                ; ACC on -> jumps back to 0x5013a with r1 = 1
...
50154: ldr   r3, [sp, #0x18]         ; store the new ACC state
```

Tracing the `Launcher` process (strace, via a temporary debug build) while switching
the engine off showed the sequence `connectAcconoff false` → `changeMode` →
`stopCheckConnect type: 256` → "AA try to exit" → "disable WiFi AndroidAuto" → ByeBye.
`connectAcconoff(false)` saves the current phone mode and switches to mode `0x100`
("no device"), which ends Android Auto. `connectAcconoff(true)` restores the mode.

**Patch:** `libappframework.so.1.0.0 @ 0x5013c`: `d8 f7 6e ed` → `00 bf 00 bf`
(the `blx` becomes two `nop`s). Both directions share this call, so it is skipped for
ignition off and on, and the new ACC state is still stored. Power-off when the door
is opened or the CAN bus goes quiet is decided by the MCU and is unaffected.

A first attempt patched the branch at `0x50144` (`bne` → `b`). That only skipped the
ACC-*on* direction and had no effect, which is why the strace step was needed.

## Patches 2 and 3: custom Wi-Fi and Bluetooth names

The stock names are `Car` + part of the MAC address. The firmware already contains a
mechanism for custom names, `wifiBtConfig` in `lib/libapis.so.1.0.0`: it reads
`key=value` lines from `/media/flash/userdata/devConfig`
(`wifiBtConfig::getDevName(QString &name, const QString &key)`), but only if
`wifiBtConfig::isEnableConfig(GPCustConfig::getUiStyle())` is true, which it is not for
this unit's UI style. Every name lookup has this shape:

```
91a68: blx   wifiBtConfig::isEnableConfig(QString const&)
91a6c: mov   r6, r0
...
91a74: cbnz  r6, 0x91a82             <-- patched to "b 0x91a82"
91a76: ...                           ; default: QString("Car"...) + MAC
91a82: blx   wifiBtConfig::getInstance()
91a86: ldr   r1, [pc, #0x2f8]        ; "WifiName"
...
91a98: blx   wifiBtConfig::getDevName(QString&, QString const&)
91aaa: blx   QString::operator!=("")
91ab0: beq   0x91a76                 ; empty  -> default name
91ab4: beq   0x91a76                 ; not found -> default name
```

Making the check unconditional means the box uses the value from `devConfig` when the
key is present and non-empty, and otherwise falls back to exactly the stock behaviour.
Each key is looked up on its own, so the two names are independent:

| Site | Key | Change |
|---|---|---|
| `libapis.so.1.0.0 @ 0x91a74` `PlatformControlPrivate::getPlatformMacName` | `WifiName` | `2e b9` → `05 e0` (`cbnz` → `b`) |
| `libapis.so.1.0.0 @ 0xb7504` `WifiControlPrivate::initAccessoryName` | `WifiName` | `34 d0` → `00 bf` (`beq` → `nop`) |
| `libapis.so.1.0.0 @ 0x91e50` `PlatformControlPrivate::getBtPlatformMacName` | `BtName` | `2e b9` → `05 e0` (`cbnz` → `b`) |
| `module/libandroidautomodule.so @ 0x36522` `AABTSrvCbk::BTSrv_ServiceCBK` | `BtName` | `67 b3` → `00 bf` (`cbz` → `nop`) |

`devConfig` lives on the writable user-data partition, so the patcher also adds
`/application/bin/devname.sh` and a oneshot `devname` service in `appinfo.rc`, started
together with `mcusrv`. The script waits for `/media/flash/userdata`, then writes the
selected `WifiName=` / `BtName=` lines, but only when the content differs.
Because the script and the `Launcher` start at the same time, the very first boot can
still show the old name; the next ignition cycle uses the new one.

## Rebuilding the file system

The `spapp` image leaves only **3,867 bytes** free in its partition. Rebuilding it with
`mksquashfs` works on Linux, but only with exactly the vendor's compressor settings.
Other LZO implementations compress about 3.5% worse than `lzo1x_999`, which would
overflow the partition. So `gpp-core` rewrites the image itself:

* Unchanged files: their compressed data blocks and fragments are copied byte-for-byte,
  in the original order. Files that shared data through de-duplication keep sharing it.
* Changed files: each block that is still identical is copied. For a block touched by a
  byte patch, the changed bytes are usually stored as LZO *literals*, so they are
  replaced inside the compressed stream and the block keeps its exact size. The result
  must decompress to the expected data, otherwise the block is recompressed.
* `appinfo.rc` lives in a fragment block. That fragment is rebuilt with the new text.
* Inode, directory, fragment and id tables are regenerated.

The result for all three patches uses 23,574,769 bytes (1,807 free). The decompressor
in `gpp-core` is as strict as the kernel's `lzo1x_decompress_safe` (no overruns, no
trailing input). Every compressed buffer the tool writes is checked against it, and the
final image is read back and compared file-by-file with the expected tree.

## Validation history

* The custom-name build ("9_final") was flashed and used in the car. The tool's output
  for the same names has identical file contents (checked with `unsquashfs` and
  `diff -r`), and all partitions other than `spapp` are byte-identical.
* The old Python build scripts' validator also accepts the tool's output.
