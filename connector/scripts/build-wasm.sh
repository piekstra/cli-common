#!/bin/sh
# Build cbase-wasm into pkg/ for the Worker.
#
# Needs: rustup target wasm32-unknown-unknown, and wasm-bindgen-cli at the
# version in cbase-wasm/Cargo.lock (`cargo install wasm-bindgen-cli --version
# <v>`, or set WASM_BINDGEN). wasm-opt (binaryen) is used when found
# (WASM_OPT or PATH) and skipped otherwise.
#
# coinbase-cli has no wasm-ready release yet, so this checks out its v0.1.1 tag
# into .cache/ and applies coinbase-cli.patch, the change this spike proposes
# upstream (a `native` feature, a Transport seam, shared queries and an MCP
# tool table). Once coinbase-cli ships that change, the path dependency in
# cbase-wasm/Cargo.toml becomes a git or crates.io dependency and this step
# goes away.
set -eu
cd "$(dirname "$0")/.."

if [ ! -e .cache/coinbase-cli ]; then
  mkdir -p .cache
  git -c advice.detachedHead=false clone --quiet --branch v0.1.1 \
    https://github.com/piekstra/coinbase-cli .cache/coinbase-cli
  git -C .cache/coinbase-cli apply "$PWD/coinbase-cli.patch"
fi

TARGET_DIR="${CARGO_TARGET_DIR:-$PWD/cbase-wasm/target}"
cargo build --manifest-path cbase-wasm/Cargo.toml --release --target wasm32-unknown-unknown
"${WASM_BINDGEN:-wasm-bindgen}" --target web --out-dir pkg \
  "$TARGET_DIR/wasm32-unknown-unknown/release/cbase_wasm.wasm"

WASM_OPT="${WASM_OPT:-$(command -v wasm-opt || true)}"
if [ -n "$WASM_OPT" ]; then
  "$WASM_OPT" -Oz --enable-bulk-memory --enable-reference-types --enable-multivalue --enable-sign-ext --enable-nontrapping-float-to-int --enable-mutable-globals --strip-debug --strip-producers \
    pkg/cbase_wasm_bg.wasm -o pkg/cbase_wasm_bg.wasm
else
  echo "wasm-opt not found; pkg/ is unoptimized" >&2
fi

wc -c < pkg/cbase_wasm_bg.wasm | awk '{print "wasm:", $1, "bytes"}'
gzip -9 -c pkg/cbase_wasm_bg.wasm | wc -c | awk '{print "gzip -9:", $1, "bytes"}'
