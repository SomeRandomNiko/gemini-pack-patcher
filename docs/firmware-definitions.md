# Firmware definition format

Each file in `firmware/*.toml` describes one stock `GEMINI_PACK.BIN` and the patches
available for it. All files are embedded into the CLI and the web app at build time;
adding a file is all it takes to support a new version.

```toml
id = "ltau_n-AR560R4"                 # unique, used by `gpp download`
name = "ltau_n AR560R4 (OTA)"         # shown to users
description = "..."                  # optional
sha256 = "3ef37f8e..."                # the exact stock file (lowercase hex)
size = 85555200
download_url = "http://.../GEMINI_PACK.BIN"   # optional, vendor download
update_index = "http://.../ltau_n/"           # optional, listing checked by `gpp check-updates`
```

## Patches

```toml
[[patch]]
id = "wifi-name"                      # unique within the file, used by `gpp apply -p`
title = "Custom Wi-Fi name"
description = "..."
warning = "..."                       # optional, highlighted in the UI
requires = ["devconfig"]              # optional, components to include (see below)

[[patch.param]]                       # optional, one per user input
id = "name"                           # used as {{name}} and in `--set wifi-name.name=...`
label = "Wi-Fi name"
help = "..."
placeholder = "My Car"
min_bytes = 1                         # default 1
max_bytes = 32
ascii_only = true                     # default false
forbidden_chars = '''='"\$`'''        # control characters are always rejected

[[patch.contribute]]                  # optional, adds a line to a named collection
collection = "devconfig"
value = "WifiName={{name}}"

[[patch.action]]
type = "byte_patch"
...
```

## Actions

All paths are relative to the root of the partition's file system. Only SquashFS
partitions (LZO) can be patched.

### `byte_patch`

```toml
type = "byte_patch"
partition = "spapp"
file = "lib/libappframework.so.1.0.0"
offset = 0x5013c                      # file offset
expect = "d8f76eed"                   # must be there, or the build fails
replace = "00bf00bf"                  # same length as expect
note = "..."                          # documentation only
```

### `replace_text`

Replaces exactly `count` (default 1) occurrences of `find` in a UTF-8 text file; any
other number of matches fails the build. Use TOML literal strings (`'''...'''`, the
first newline is dropped) to keep whitespace exact.

```toml
type = "replace_text"
partition = "spapp"
file = "appinfo.rc"
find = '''...'''
replace = '''...'''
```

### `add_file`

Adds a new regular file. Its directory must exist and the name must be unused.

```toml
type = "add_file"
partition = "spapp"
path = "bin/devname.sh"
mode = 0o755                          # default 0o755
content = '''...'''
```

## Templates

`find`, `replace` and `content` may contain placeholders:

* `{{param}}`: a parameter of the same patch (patch actions only).
* `{{collect:<name>}}`: all values contributed to collection `<name>` by the selected
  patches, joined with newlines (in definition order).

Unknown placeholders are rejected when the catalog loads (`cargo test` checks this).

## Components

Actions shared by several patches go into a component, which is applied once if any
selected patch `requires` it. Component actions run before patch actions and may only
use `{{collect:...}}` placeholders.

```toml
[[components.devconfig.action]]
type = "add_file"
...
```
