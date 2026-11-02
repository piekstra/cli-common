#!/bin/sh
# The whole local check, as CI runs it: a dummy key, the Coinbase mock, the
# Worker under `wrangler dev --local` (which fetches and verifies the pinned
# modules first), scripts/smoke.sh, then a passphrase change that must end
# the connections made before it. No Cloudflare account and no deployment;
# everything listens on 127.0.0.1 and stops on exit.
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
BASE=http://127.0.0.1:8787
DEV=
trap 'kill $MOCK $DEV 2>/dev/null || true' EXIT INT TERM

# The base URL is passed here as well as in .dev.vars, so no edit there can
# send test traffic to the real API. Local KV persists in .e2e/state, so a
# restart keeps the grants.
start_worker() {
  WRANGLER_SEND_METRICS=false npx wrangler dev --local --ip 127.0.0.1 --port 8787 \
    --persist-to .e2e/state --var CBASE_BASE_URL:http://127.0.0.1:8788 >>.e2e/dev.log 2>&1 &
  DEV=$!
  i=0
  until curl -sf -o /dev/null "$BASE/"; do
    i=$((i + 1))
    if [ $i -gt 120 ] || ! kill -0 "$DEV" 2>/dev/null; then
      echo "the Worker did not come up; its log:" >&2
      cat .e2e/dev.log >&2
      exit 1
    fi
    sleep 1
  done
}

stop_worker() {
  kill "$DEV" 2>/dev/null || true
  wait "$DEV" 2>/dev/null || true
  # workerd can outlive wrangler for a moment; wait for the port to close.
  i=0
  while curl -s -o /dev/null "$BASE/" && [ $i -lt 30 ]; do i=$((i + 1)); sleep 1; done
}

failed() {
  echo "FAIL: $*" >&2
  echo "--- Worker log" >&2; cat .e2e/dev.log >&2
  echo "--- mock log" >&2; cat .e2e/mock.log >&2
  exit 1
}

start_worker
sh scripts/smoke.sh "$BASE" || failed "smoke"

# Changing the passphrase ends every connection: connect once more and keep
# the tokens, restart the Worker under a new passphrase, and both tokens must
# be refused, while the new passphrase connects.
printf '\n== a new passphrase ends every connection\n'
SMOKE_TOKEN_OUT=.e2e/tokens.json sh scripts/smoke.sh "$BASE" >.e2e/smoke-before.log 2>&1 \
  || { cat .e2e/smoke-before.log >&2; failed "connecting before the change"; }
stop_worker
sed -i.bak "s/^OWNER_PASSPHRASE=.*/OWNER_PASSPHRASE=$(openssl rand -hex 16)/" .dev.vars && rm -f .dev.vars.bak
start_worker
code=$(curl -s -o /dev/null -w '%{http_code}' "$BASE/mcp" -H "authorization: Bearer $(jq -r .access .e2e/tokens.json)" \
  -H 'content-type: application/json' -H 'accept: application/json, text/event-stream' \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/list"}')
[ "$code" = 401 ] || failed "the access token still works under the new passphrase ($code)"
answer=$(curl -s "$BASE/oauth/token" -d grant_type=refresh_token \
  --data-urlencode "refresh_token=$(jq -r .refresh .e2e/tokens.json)" -d "client_id=$(jq -r .client_id .e2e/tokens.json)")
[ "$(echo "$answer" | jq -r .error)" = invalid_grant ] || failed "the refresh token still works under the new passphrase: $answer"
echo "access token 401, refresh token invalid_grant"
sh scripts/smoke.sh "$BASE" >.e2e/smoke-after.log 2>&1 \
  || { cat .e2e/smoke-after.log >&2; failed "connecting with the new passphrase"; }
echo "the new passphrase connects"
printf '\nOK\n'
