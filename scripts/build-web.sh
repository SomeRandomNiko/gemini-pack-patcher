#!/bin/sh
# Builds the static web app into dist/ (served as-is, e.g. by Cloudflare Workers).
# Needs: the wasm32-unknown-unknown Rust target and wasm-bindgen-cli matching the
# wasm-bindgen version in Cargo.lock.
set -eu
cd "$(dirname "$0")/.."
CARGO=${CARGO:-cargo}
WASM_BINDGEN=${WASM_BINDGEN:-wasm-bindgen}

"$CARGO" build --release --target wasm32-unknown-unknown -p gpp-wasm
rm -rf dist
mkdir -p dist/pkg
"$WASM_BINDGEN" --target web --no-typescript --out-dir dist/pkg \
	target/wasm32-unknown-unknown/release/gpp_wasm.wasm
cp web/* dist/
echo "Built dist/ ($(du -sh dist | cut -f1))"
