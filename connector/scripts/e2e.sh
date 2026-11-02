#!/bin/sh
# The whole local check, as CI runs it: a dummy key, the Coinbase mock, the
# Worker under `wrangler dev --local` (which fetches and verifies the pinned
# modules first), then scripts/smoke.sh. No Cloudflare account and no
# deployment; everything listens on 127.0.0.1 and stops on exit.
#
#   sh scripts/e2e.sh        (npm run e2e)
set -eu
cd "$(dirname "$0")/.."
# Fresh local KV every run: OAuth state and the passphrase lockout counter
# from an earlier run would change what this one sees.
rm -rf .e2e
mkdir -p .e2e
node scripts/gen-dummy-key.mjs
node mock/coinbase-mock.mjs 8788 >.e2e/mock.log 2>&1 &
MOCK=$!
# The base URL is passed here as well as in .dev.vars, so no edit there can
# send test traffic to the real API.
WRANGLER_SEND_METRICS=false npx wrangler dev --local --ip 127.0.0.1 --port 8787 \
  --persist-to .e2e/state --var CBASE_BASE_URL:http://127.0.0.1:8788 >.e2e/dev.log 2>&1 &
DEV=$!
trap 'kill $MOCK $DEV 2>/dev/null || true' EXIT INT TERM

i=0
until curl -sf -o /dev/null http://127.0.0.1:8787/; do
  i=$((i + 1))
  if [ $i -gt 120 ] || ! kill -0 $DEV 2>/dev/null; then
    echo "the Worker did not come up; its log:" >&2
    cat .e2e/dev.log >&2
    exit 1
  fi
  sleep 1
done

if ! sh scripts/smoke.sh http://127.0.0.1:8787; then
  echo "--- Worker log" >&2; cat .e2e/dev.log >&2
  echo "--- mock log" >&2; cat .e2e/mock.log >&2
  exit 1
fi
