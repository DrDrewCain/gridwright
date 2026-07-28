#!/usr/bin/env bash
#
# Build the browser bundle into `pkg/`, which `index.html` imports.
#
# This existed as two commands in an HTML comment, which is a fine place to
# read them and a bad place to run them from: every rebuild during the canvas
# work was those two lines retyped, and the `--no-typescript` flag that keeps
# the `.d.ts` files out of `pkg/` was remembered about half the time.
#
# No wasm-opt pass. `wasm-opt -Oz` would take another 15-20% off, but it is not
# in the toolchain this repo already requires, and a build script that needs a
# tool the reader does not have is a build script they will work around.

set -euo pipefail

here=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)
root=$(cd -- "$here/../.." && pwd -P)

if ! command -v wasm-bindgen >/dev/null; then
  echo "wasm-bindgen not found. cargo install wasm-bindgen-cli" >&2
  exit 1
fi

# `--lib` only. The crate also has a `main.rs` for the native window, and
# building that for wasm fails on the winit event loop rather than on anything
# to do with this bundle.
cargo build \
  --manifest-path "$root/Cargo.toml" \
  -p gridwright-studio \
  --lib \
  --release \
  --target wasm32-unknown-unknown

wasm-bindgen \
  --target web \
  --no-typescript \
  --out-dir "$here/pkg" \
  "$root/target/wasm32-unknown-unknown/release/gridwright_studio.wasm"

size=$(wc -c <"$here/pkg/gridwright_studio_bg.wasm")
brotli=$(brotli -c -q 11 "$here/pkg/gridwright_studio_bg.wasm" 2>/dev/null | wc -c || echo 0)
printf 'pkg/gridwright_studio_bg.wasm  %.1f MB' "$(bc -l <<<"$size/1048576")"
if [ "$brotli" -gt 0 ]; then
  # The number that matters is what crosses the wire. Any static host worth
  # using serves this precompressed, and the raw size is off by a factor of
  # four or so.
  printf '  (%.1f MB brotli)' "$(bc -l <<<"$brotli/1048576")"
fi
printf '\n'
