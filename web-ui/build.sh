#!/usr/bin/env bash
# Build the web UI to WebAssembly and place it where `ano web` embeds it:
# src/interface/web/assets/pkg/. Commit the result, so building ano itself
# needs no wasm toolchain.
#
#   web-ui/build.sh
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root="$(cd "$here/.." && pwd)"
out="$root/src/interface/web/assets/pkg"

die() { echo "error: $*" >&2; exit 1; }

if ! rustup target list --installed 2>/dev/null | grep -qx wasm32-unknown-unknown; then
  echo "Adding the wasm32-unknown-unknown target..."
  rustup target add wasm32-unknown-unknown
fi

want="$(awk '/^name = "wasm-bindgen"$/{getline; gsub(/[",]/,""); print $3; exit}' "$root/Cargo.lock")"
[ -n "$want" ] || die "cannot read the wasm-bindgen version from Cargo.lock"
command -v wasm-bindgen >/dev/null 2>&1 || die "wasm-bindgen is not installed; run:
  cargo install wasm-bindgen-cli --version $want"
have="$(wasm-bindgen --version | awk '{print $2}')"
[ "$have" = "$want" ] || die "wasm-bindgen CLI $have does not match the crate's $want; run:
  cargo install wasm-bindgen-cli --version $want"

cd "$root"
cargo build -p ano-web-ui --release --target wasm32-unknown-unknown
rm -rf "$out"
wasm-bindgen --target web --no-typescript --remove-name-section --remove-producers-section \
  --out-dir "$out" \
  "$root/target/wasm32-unknown-unknown/release/ano_web_ui.wasm"
if command -v wasm-opt >/dev/null 2>&1; then
  wasm-opt -Oz --enable-bulk-memory --enable-nontrapping-float-to-int \
    -o "$out/ano_web_ui_bg.wasm" "$out/ano_web_ui_bg.wasm"
fi
ls -l "$out"
