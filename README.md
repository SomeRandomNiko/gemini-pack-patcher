# gemini-pack-patcher

Patch the stock firmware of Android Auto / CarPlay head units that are updated with a
`GEMINI_PACK.BIN` file (SunPlus 8368 platform). Developed and tested on a
[Roadtop CarPlay / Android Auto unit for Audi](https://it.aliexpress.com/item/1005004428235119.html) (firmware "ltau_n").

This repository does **not** contain or distribute any firmware. You download the
original update from the vendor and the tool patches it on your own computer. The
patches are published as instructions, and the tool refuses any input that is not
byte-for-byte the expected stock file.

## Patches

| Patch | What it does |
|---|---|
| **Keep Android Auto running after ignition off** | Stock firmware ends the Android Auto session (and the phone's Bluetooth/Wi-Fi link) as soon as the ignition turns off. With this patch the session keeps running after the engine is switched off; the unit still powers down as before when you leave the car (door opened or the car's CAN bus going to sleep). |
| **Custom Wi-Fi name** | Replaces the stock `Car` + MAC Wi-Fi name (used by wireless Android Auto) with your own. |
| **Custom Bluetooth name** | Replaces the stock `Car` + MAC Bluetooth name with your own. |

Each patch can be applied on its own. How they were found and how they work is
documented in [docs/research.md](docs/research.md).

### Supported firmware

| Version | SHA-256 of the stock `GEMINI_PACK.BIN` |
|---|---|
| ltau_n AR560R4 (OTA, 2026-09-03), [Roadtop unit for Audi](https://it.aliexpress.com/item/1005004428235119.html) | `3ef37f8e1f993611e091a8941f28464bc194481c86d6d02664ae5bf7ba9c9223` |

Newer versions need their own definition; see [docs/porting.md](docs/porting.md).
`gpp check-updates` tells you whether the vendor has published one.

## Usage

### In the browser

Open the web app, follow the link to the vendor download, drop the downloaded
`GEMINI_PACK.BIN` into the page, tick the patches you want, enter the names and press
**Apply selected patches**. The file is processed locally in your browser (WebAssembly)
and is never uploaded. The vendor only serves plain HTTP, so your browser may warn about
an "insecure download"; the tool verifies the file's SHA-256 before doing anything.

### Command line

```sh
gpp list                                        # supported versions and patches
gpp download ltau_n-AR560R4                     # fetch the stock file from the vendor, verify its hash
gpp apply ltau_n-AR560R4/GEMINI_PACK.BIN -o patched/GEMINI_PACK.BIN \
    -p aa-keep-alive \
    -p wifi-name --set "wifi-name.name=My Car" \
    -p bt-name   --set "bt-name.name=My Car"
gpp verify patched/GEMINI_PACK.BIN              # the updater's integrity checks
gpp check-updates                               # look for versions this tool doesn't know yet
gpp extract GEMINI_PACK.BIN parts/              # dump all partitions (for research/porting)
```

Every build is verified before it is written: all pack checksums, every file of the
rewritten file system compared against the expected content, and every byte outside the
patched partition compared against the stock file.

### Flashing

1. Copy the patched file to the root of a FAT32 USB stick. It must be named exactly `GEMINI_PACK.BIN`.
2. Plug the stick into the head unit and open the decoder settings page → *System update* (password `8032`) → *USB*.
3. Keep the unit powered until the update has finished.

If the unit reports "no ISPBOOOT.BIN here!", stop: nothing was flashed. After the first
boot a new Wi-Fi/Bluetooth name may only appear after one more ignition cycle, and your
phone may need to pair again.

**To undo:** flash the original stock `GEMINI_PACK.BIN` the same way. Any official update
also replaces the patched firmware.

> [!WARNING]
> Use at your own risk. Modified firmware may void your warranty and an interrupted or
> failed update can leave the unit unusable. With the Android Auto patch the unit stays
> powered while the phone is connected, which draws on the car battery.

## Building

Requires Rust (stable) with the `wasm32-unknown-unknown` target, and
[`wasm-bindgen-cli`](https://crates.io/crates/wasm-bindgen-cli) at the version pinned in
`crates/gpp-wasm/Cargo.toml` for the web app.

```sh
cargo build --release -p gpp-cli          # CLI at target/release/gpp
cargo test                                # unit tests
./scripts/build-web.sh                    # static web app in dist/
python3 -m http.server -d dist 8000       # try it at http://localhost:8000
```

Tests against real firmware run when the files are provided (they are not in the repo):

```sh
GPP_STOCK=/path/to/stock/GEMINI_PACK.BIN cargo test --release
```

### Deploying

The web app is a static site. `wrangler.toml` serves `dist/` with Cloudflare Workers
static assets; `.github/workflows/deploy.yml` builds and deploys on every push to `main`
once the repository secrets `CLOUDFLARE_API_TOKEN` and `CLOUDFLARE_ACCOUNT_ID` and the
repository variable `DEPLOY_ENABLED=true` are set. `web/_headers` sets a strict
Content-Security-Policy (the page makes no external requests).
Any other static host works too.

## Repository layout

| Path | Contents |
|---|---|
| `firmware/` | One patch definition per stock firmware ([format](docs/firmware-definitions.md)) |
| `crates/gpp-core` | Pack format, SquashFS reader/rewriter, LZO, patch engine, verification |
| `crates/gpp-cli` | The `gpp` command-line tool |
| `crates/gpp-wasm` | WebAssembly bindings for the web app |
| `web/` | The web app (plain HTML/CSS/JS, runs the core in a Web Worker) |
| `docs/` | Research notes, definition format, porting guide |

## Legal

This project contains only original code and documentation: descriptions of the stock
firmware (file names, offsets, original and replacement bytes) and tools that apply them
to a copy you obtained yourself. It is not affiliated with or endorsed by the head unit's
manufacturer, vendor or any car maker; all trademarks belong to their owners.

Licensed under the [MIT License](LICENSE).
