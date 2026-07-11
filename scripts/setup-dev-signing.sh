#!/usr/bin/env bash
#
# ONE-TIME, RUN BY THE OWNER (macOS): create the stable self-signed
# "pk-cli-codesign" identity so keychain "Always Allow" decisions stick across
# rebuilds of the family CLIs (fpl, xfin, lrfl, tojfl, …).
#
# Plain `cargo build` ad-hoc signs, which gives every build a DIFFERENT code
# signature — the keychain ACL treats each rebuild as a brand-new app and
# re-prompts. Signing dev builds with one named identity (same pattern as the
# tosui-codesign / optio-codesign identities) gives every build the same
# designated requirement, so one "Always Allow" holds forever.
#
# macOS may prompt for your login password once (trust settings). After the
# first RE-SIGNED build touches the keychain, expect ONE final prompt per CLI —
# answer "Always Allow" and that's the last one. Re-sign each build with
# scripts/dev-sign.sh (or a `make dev` target that calls it).
set -euo pipefail

ID="pk-cli-codesign"
KEYCHAIN="$HOME/Library/Keychains/login.keychain-db"

if security find-identity -v -p codesigning | grep -q "$ID"; then
  echo "[setup-dev-signing] $ID already exists — nothing to do"
  exit 0
fi

TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

# System LibreSSL, NOT homebrew OpenSSL 3: the 3.x default p12 encryption
# (AES/PBKDF2) fails `security import` with "MAC verification failed"; the
# system openssl emits the legacy algorithms macOS expects.
OPENSSL=/usr/bin/openssl

echo "[setup-dev-signing] generating self-signed code-signing certificate ($ID, 10y)…"
"$OPENSSL" req -x509 -newkey rsa:2048 -keyout "$TMP/key.pem" -out "$TMP/cert.pem" \
  -days 3650 -nodes -subj "/CN=$ID" \
  -addext "keyUsage=critical,digitalSignature" \
  -addext "extendedKeyUsage=critical,codeSigning" \
  -addext "basicConstraints=critical,CA:false" >/dev/null 2>&1

"$OPENSSL" pkcs12 -export -out "$TMP/$ID.p12" -inkey "$TMP/key.pem" -in "$TMP/cert.pem" \
  -passout pass:pk-cli-local -name "$ID"

echo "[setup-dev-signing] importing into the login keychain (codesign pre-authorized)…"
security import "$TMP/$ID.p12" -k "$KEYCHAIN" -P pk-cli-local -T /usr/bin/codesign

echo "[setup-dev-signing] trusting for code signing (macOS may ask for your password)…"
security add-trusted-cert -r trustRoot -p codeSign -k "$KEYCHAIN" "$TMP/cert.pem"

echo "[setup-dev-signing] verifying…"
if security find-identity -v -p codesigning | grep -q "$ID"; then
  echo "[setup-dev-signing] OK — $ID is ready. Sign builds with scripts/dev-sign.sh."
else
  echo "[setup-dev-signing] FAILED — $ID not valid for codesigning; see output above" >&2
  exit 1
fi
