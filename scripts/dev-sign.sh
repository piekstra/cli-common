#!/usr/bin/env bash
#
# Re-sign freshly built dev binaries with the stable "pk-cli-codesign"
# identity (created once by setup-dev-signing.sh) so macOS keychain
# "Always Allow" grants survive rebuilds.
#
#   cargo build && /path/to/cli-common/scripts/dev-sign.sh target/debug/fpl
#
# No-ops with a warning when the identity doesn't exist (e.g. CI, Linux).
set -euo pipefail

ID="pk-cli-codesign"

if [[ "$(uname)" != "Darwin" ]]; then
  exit 0
fi
if [[ $# -eq 0 ]]; then
  echo "usage: dev-sign.sh <binary> [binary…]" >&2
  exit 2
fi
if ! security find-identity -v -p codesigning 2>/dev/null | grep -q "$ID"; then
  echo "[dev-sign] $ID identity not found — run cli-common/scripts/setup-dev-signing.sh once" >&2
  exit 0
fi

for bin in "$@"; do
  codesign --force --sign "$ID" "$bin"
  echo "[dev-sign] signed $bin with $ID"
done
