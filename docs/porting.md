# Porting the patches to a new firmware version

When the vendor publishes a new version, the existing definitions will not accept it
(the SHA-256 differs) and the offsets have most likely moved. This is the procedure to
add it.

## 1. Get and unpack the new firmware

```sh
gpp check-updates                       # lists versions on the vendor server; new ones are marked
curl -o new/GEMINI_PACK.BIN http://.../ltau_n-ARxxxRy/GEMINI_PACK.BIN
sha256sum new/GEMINI_PACK.BIN; stat -c %s new/GEMINI_PACK.BIN
gpp verify new/GEMINI_PACK.BIN          # the container format must still be understood
gpp extract new/GEMINI_PACK.BIN new/parts
unsquashfs -d new/spapp new/parts/spapp.bin
```

Do the same for the currently supported version (e.g. into `old/`). This lets you
compare the old and new code side by side.

If `gpp verify` fails or `spapp` is no longer SquashFS/LZO, the format has changed and
`gpp-core` needs work first.

## 2. Find each patch site

The libraries keep their dynamic symbols, so start from the function names in
[research.md](research.md):

```sh
LIB=new/spapp/lib/libappframework.so.1.0.0
llvm-nm -DC $LIB | grep KnobManager::updateUpMsg       # demangled names
llvm-nm -D  $LIB | grep KnobManager11updateUpMsg       # mangled name for the next command
llvm-objdump -d --triple=thumbv7a-linux-gnueabihf \
    --disassemble-symbols=_ZN14spappframework11KnobManager11updateUpMsgEiiiii $LIB \
    | grep -B4 -A6 connectAcconoff
```

For each patch, find the same instruction as in the old version:

| Patch | Look for |
|---|---|
| aa-keep-alive | the `blx GpBTDevManager::connectAcconoff` in `KnobManager::updateUpMsg` (`libappframework`) |
| wifi-name | the branch on the `isEnableConfig` result before the `"WifiName"` lookup in `PlatformControlPrivate::getPlatformMacName` and `WifiControlPrivate::initAccessoryName` (`libapis`) |
| bt-name | the same before `"BtName"` in `PlatformControlPrivate::getBtPlatformMacName` (`libapis`) and `AABTSrvCbk::BTSrv_ServiceCBK` (`module/libandroidautomodule.so`) |

The replacement depends on the instruction:
* a call that should disappear becomes `00 bf 00 bf` (two Thumb `nop`s);
* a conditional branch that should always skip the default becomes an unconditional `b`
  to the same target;
* a conditional branch that should never be taken becomes `00 bf`.

Re-encode branches whose distance changed rather than copying the old bytes. Also check
that the files and lines that `appinfo.rc` edits rely on still exist.

## 3. Write the definition

Copy the old `firmware/*.toml` to a new file. Update `id`, `name`, `description`,
`sha256`, `size`, `download_url`, and every `offset` / `expect` / `replace`. `expect`
must be the new version's original bytes, and the build refuses to run if they don't match.

## 4. Test

```sh
cargo test                                            # catalog still loads
cargo run -p gpp-cli -- apply new/GEMINI_PACK.BIN -o out/GEMINI_PACK.BIN \
    -p aa-keep-alive -p wifi-name --set "wifi-name.name=Test" -p bt-name --set "bt-name.name=Test"
```

All checks must pass, and the free space must stay positive. Then flash it on a real
unit and test each patch separately before publishing the definition. Mark untested
definitions clearly in their `description`.
