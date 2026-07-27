#!/usr/bin/env bash
#
# Build the gridwright engine for the browser.
#
# Two steps, neither of which is optional: cargo produces a raw
# `wasm32-unknown-unknown` module whose exports speak in pointers and lengths,
# and wasm-bindgen writes the JavaScript that turns those into strings and byte
# arrays. Loading the cargo artifact directly would give you `load_bytes(ptr,
# len)` and no way to build the arguments.
#
# The output is content-addressed: the file names carry the first 8 hex digits
# of the sha256 of the wasm cargo produced, and `pkg/manifest.json` names the
# current build. That is what makes `Cache-Control: immutable` in vercel.json
# honest — a changed engine is a changed URL, so nothing has to be revalidated
# and nothing can go stale. `manifest.json` itself is the one file served
# uncached, and it is 200 bytes.
#
# Idempotent: same input, same output names, and `pkg/` is rebuilt from empty
# every run so a previous build's hashed files cannot linger and be served.
#
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root="$(cd "$here/.." && pwd)"
pkg="$here/pkg"

crate="gridwright-worker"
target="wasm32-unknown-unknown"
artifact="$root/target/$target/release/gridwright_worker.wasm"

die() {
    printf 'build.sh: %s\n' "$*" >&2
    exit 1
}

# --- preconditions -----------------------------------------------------------
#
# Checked up front and by name, because every one of these fails much later and
# much more confusingly than it needs to.

command -v cargo >/dev/null 2>&1 || die "cargo not found on PATH"
command -v wasm-bindgen >/dev/null 2>&1 || die \
    "wasm-bindgen not found on PATH; install it with
    cargo install wasm-bindgen-cli --version <the version in Cargo.lock>"

[ -f "$root/Cargo.lock" ] || die "no Cargo.lock at $root; run from a checkout"

# The CLI and the crate are one program split across a build boundary: the crate
# emits a description of the bindings and the CLI reads it. A mismatch is not a
# warning, it is "invalid schema version" partway through, so it is caught here
# where the message can say what to do about it.
locked_bindgen="$(
    awk '/^name = "wasm-bindgen"$/ { found = 1; next }
         found && /^version = / { gsub(/["]/, "", $3); print $3; exit }' \
        "$root/Cargo.lock"
)"
[ -n "$locked_bindgen" ] || die "could not find the wasm-bindgen version in Cargo.lock"

cli_bindgen="$(wasm-bindgen --version | awk '{ print $2 }')"
[ "$cli_bindgen" = "$locked_bindgen" ] || die \
    "wasm-bindgen CLI is $cli_bindgen but Cargo.lock pins the crate at $locked_bindgen.
    They must match. Fix with:
    cargo install wasm-bindgen-cli --version $locked_bindgen --force"

if command -v rustup >/dev/null 2>&1; then
    rustup target list --installed | grep -qx "$target" || die \
        "the $target target is not installed; add it with
    rustup target add $target"
fi

if command -v shasum >/dev/null 2>&1; then
    sha256() { shasum -a 256 "$1" | awk '{ print $1 }'; }
elif command -v sha256sum >/dev/null 2>&1; then
    sha256() { sha256sum "$1" | awk '{ print $1 }'; }
else
    die "neither shasum nor sha256sum found; one is needed to name the output"
fi

size_of() { wc -c <"$1" | tr -d ' '; }

# --- build -------------------------------------------------------------------
#
# Release, always. The debug profile produces a module several times the size
# for a solver that is then too slow to be worth loading, so there is no
# reasonable debug build of this thing to offer a flag for.

printf 'build.sh: cargo build --release --target %s -p %s\n' "$target" "$crate"
cargo build --release --target "$target" -p "$crate"

[ -f "$artifact" ] || die "cargo reported success but $artifact does not exist"

hash="$(sha256 "$artifact")"
short="${hash:0:8}"
name="gridwright_worker_$short"

rm -rf "$pkg"
mkdir -p "$pkg"

printf 'build.sh: wasm-bindgen --target web --out-name %s\n' "$name"
wasm-bindgen --target web --out-dir "$pkg" --out-name "$name" "$artifact"

js="$pkg/$name.js"
wasm="$pkg/${name}_bg.wasm"

# wasm-bindgen exits 0 on paths that produce nothing usable, so the two files
# the worker actually imports are checked by name rather than assumed.
[ -f "$js" ] || die "wasm-bindgen produced no $js"
[ -f "$wasm" ] || die "wasm-bindgen produced no $wasm"

# The worker reads this to learn what the current build is called. It is the
# only indirection in the whole scheme and it exists so that everything else can
# be cached forever.
cat >"$pkg/manifest.json" <<EOF
{
  "module": "$name.js",
  "wasm": "${name}_bg.wasm",
  "hash": "$hash",
  "built": "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
}
EOF

# --- report ------------------------------------------------------------------

wasm_bytes="$(size_of "$wasm")"
js_bytes="$(size_of "$js")"

printf '\nbuild.sh: wrote %s\n' "$pkg"
printf '  %-40s %9s bytes\n' "${name}_bg.wasm" "$wasm_bytes"
printf '  %-40s %9s bytes\n' "$name.js" "$js_bytes"

# Transfer size is what a visitor pays, and it is a different number entirely.
# Reported only when a brotli encoder is on hand, since one is not standard on
# macOS and its absence is not a build failure.
if command -v brotli >/dev/null 2>&1; then
    br_bytes="$(brotli -c -q 11 "$wasm" | wc -c | tr -d ' ')"
    printf '  %-40s %9s bytes (what the CDN sends)\n' "${name}_bg.wasm, brotli -q 11" "$br_bytes"
fi

printf '\nbuild.sh: ok. Serve this directory over HTTP:\n'
printf '  python3 -m http.server 8080 --directory %s\n' "$here"
