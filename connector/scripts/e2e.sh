#!/bin/sh
# The whole local check, as CI runs it: dummy keys, the Coinbase and Alpaca
# mocks, the Worker under `wrangler dev --local` (with the pinned modules
# fetched and verified first), scripts/smoke.sh, then a passphrase change
# that must end the connections made before it, and each module turned off
# in turn. No Cloudflare account and no deployment; everything listens on
# 127.0.0.1 and stops on exit.
#
#   sh scripts/e2e.sh        (npm run e2e)
#
# With E2E_HOME_BIN set to a family CLI that has `mcp --http` (the Makefile
# passes cli-common's example-cli), it also runs a home module: that CLI on
# 127.0.0.1:8790 with a fresh bearer token, reached through
# mock/vpc-standin.ts in place of a Workers VPC binding, then stopped, so
# its tools must answer `offline:`. The CLI runs with this environment: for
# one that reads the keychain on a call, point it at an empty keychain
# service first (m1f: M1F_KEYCHAIN_SERVICE).
set -eu
cd "$(dirname "$0")/.."
# Fresh local KV every run: OAuth state and the passphrase lockout counter
# from an earlier run would change what this one sees.
rm -rf .e2e
mkdir -p .e2e
node scripts/fetch-modules.mjs
node scripts/gen-dummy-key.mjs
node mock/coinbase-mock.mjs 8788 >.e2e/mock.log 2>&1 &
MOCK=$!
node mock/alpaca-mock.mjs 8789 >.e2e/alpaca.log 2>&1 &
ALPACA=$!
BASE=http://127.0.0.1:8787
DEV=
HOME_SERVER=
# A process and everything under it, children first: killing npx alone can
# leave wrangler's workerd serving the port (it does on Linux).
kill_tree() {
  for child in $(pgrep -P "$1" 2>/dev/null); do kill_tree "$child"; done
  kill "$1" 2>/dev/null || true
}
trap 'kill_tree $MOCK; kill_tree $ALPACA; [ -z "$DEV" ] || kill_tree $DEV; [ -z "$HOME_SERVER" ] || kill_tree $HOME_SERVER' EXIT INT TERM

failed() {
  echo "FAIL: $*" >&2
  echo "--- Worker log" >&2; cat .e2e/dev.log >&2
  echo "--- Coinbase mock log" >&2; cat .e2e/mock.log >&2
  echo "--- Alpaca mock log" >&2; cat .e2e/alpaca.log >&2
  [ ! -f .e2e/home.log ] || { echo "--- home server log" >&2; cat .e2e/home.log >&2; }
  exit 1
}

EXPECT='["coinbase_accounts","coinbase_transactions","alpaca_portfolio","alpaca_activities"]'
CONFIG=
if [ -n "${E2E_HOME_BIN:-}" ]; then
  HOME_PORT=8790
  "$E2E_HOME_BIN" mcp --list-tools >.e2e/home-tools.json
  # The module's name is the tools' prefix (m1_portfolio: HOME_M1).
  HOME_NAME="$(jq -r '.tools[0].name | split("_")[0] | ascii_upcase' .e2e/home-tools.json)"
  HOME_TOOLS="$(jq -c '[.tools[] | select(.annotations.readOnlyHint) | .name]' .e2e/home-tools.json)"
  HOME_WRITE="$(jq -r '[.tools[] | select(.annotations.readOnlyHint | not) | .name][0] // empty' .e2e/home-tools.json)"
  TOKEN_VAR="$(basename "$E2E_HOME_BIN" | tr 'a-z' 'A-Z' | tr -c 'A-Z0-9\n' '_')_MCP_TOKEN"
  HOME_TOKEN="$(openssl rand -hex 32)"
  env "$TOKEN_VAR=$HOME_TOKEN" "$E2E_HOME_BIN" mcp --http --port $HOME_PORT >.e2e/home.log 2>&1 &
  HOME_SERVER=$!
  i=0
  until [ "$(curl -s -o /dev/null -w '%{http_code}' -X POST "http://127.0.0.1:$HOME_PORT/mcp")" = 401 ]; do
    i=$((i + 1))
    { [ $i -lt 60 ] && kill -0 "$HOME_SERVER" 2>/dev/null; } || failed "the home server did not come up"
    sleep 1
  done
  echo "HOME_${HOME_NAME}_TOKEN=$HOME_TOKEN" >>.dev.vars
  node scripts/e2e-home-config.mjs "$HOME_NAME" "$HOME_PORT"
  CONFIG="-c wrangler.e2e.jsonc -c .e2e/vpc-standin.jsonc"
  EXPECT="$(jq -cn --argjson a "$EXPECT" --argjson b "$HOME_TOOLS" '$a + $b')"
fi
export SMOKE_EXPECT_TOOLS="$EXPECT"

# The base URLs are passed here as well as in .dev.vars, so no edit there
# can send test traffic to the real APIs. Local KV persists in .e2e/state, so
# a restart keeps the grants.
start_worker() {
  # shellcheck disable=SC2086 # CONFIG is two flags or none
  WRANGLER_SEND_METRICS=false npx wrangler dev $CONFIG --local --ip 127.0.0.1 --port 8787 \
    --persist-to .e2e/state --var CBASE_BASE_URL:http://127.0.0.1:8788 \
    --var ALP_BASE_URL:http://127.0.0.1:8789 >>.e2e/dev.log 2>&1 &
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
  kill_tree "$DEV"
  wait "$DEV" 2>/dev/null || true
  DEV=
  # Until the port closes: a Worker still answering there would be the old
  # one, and the checks after a restart would test nothing.
  i=0
  while curl -s -o /dev/null --max-time 2 "$BASE/"; do
    i=$((i + 1))
    [ $i -lt 30 ] || failed "the Worker still answers on $BASE after it was stopped"
    sleep 1
  done
}

# One JSON-RPC call to the Worker, with the access token from a file
# scripts/smoke.sh wrote (SMOKE_TOKEN_OUT).
rpc_with() { # <tokens file> <body>
  curl -s "$BASE/mcp" -H "authorization: Bearer $(jq -r .access "$1")" -H 'content-type: application/json' \
    -H 'accept: application/json, text/event-stream' -H 'mcp-protocol-version: 2025-11-25' -d "$2" \
    | sed -n -e 's/^data: //p' -e t -e '/^{/p'
}

call_json() { # <id> <tool>
  printf '{"jsonrpc":"2.0","id":%s,"method":"tools/call","params":{"name":"%s","arguments":{}}}' "$1" "$2"
}

start_worker
sh scripts/smoke.sh "$BASE" || failed "smoke"

# Changing the passphrase ends every connection: connect once more and keep
# the tokens, restart the Worker under a new passphrase, and both tokens must
# be refused, while the new passphrase connects.
SMOKE_TOKEN_OUT=.e2e/tokens.json sh scripts/smoke.sh "$BASE" >.e2e/smoke-before.log 2>&1 \
  || { cat .e2e/smoke-before.log >&2; failed "connecting before the change"; }

if [ -n "$HOME_SERVER" ]; then
  # Through the Worker and the stand-in to the CLI on 127.0.0.1, and back.
  printf '\n== the home module: HOME_%s, through the tunnel stand-in\n' "$HOME_NAME"
  first="$(echo "$HOME_TOOLS" | jq -r '.[0]')"
  out="$(rpc_with .e2e/tokens.json "$(call_json 1 "$first")")"
  echo "$out" | jq -c '.result | {isError, text: (.content[0].text | .[0:120])}'
  # The CLI's own answer, whatever it is: data, or (with no sign-in on this
  # machine) its auth error naming its login command.
  echo "$out" | jq -e '.result.isError == false or (.result.content[0].text | startswith("auth:"))' >/dev/null \
    || failed "the home tool's answer did not come back: $out"
  if [ -n "$HOME_WRITE" ]; then
    out="$(rpc_with .e2e/tokens.json "$(call_json 2 "$HOME_WRITE")")"
    echo "$out" | jq -c '.error'
    echo "$out" | jq -e '.error.code == -32602' >/dev/null || failed "the write tool $HOME_WRITE was served: $out"
  fi
  kill_tree "$HOME_SERVER"
  wait "$HOME_SERVER" 2>/dev/null || true
  HOME_SERVER=
  start=$(date +%s)
  out="$(rpc_with .e2e/tokens.json "$(call_json 3 "$first")")"
  took=$(($(date +%s) - start))
  echo "$out" | jq -c '.result | {isError, text: (.content[0].text | .[0:80])}'
  echo "$out" | jq -e '.result.isError and (.result.content[0].text | startswith("offline:"))' >/dev/null \
    || failed "a stopped home server was not reported offline: $out"
  [ "$took" -le 10 ] || failed "the offline answer took $took s"
  out="$(rpc_with .e2e/tokens.json '{"jsonrpc":"2.0","id":4,"method":"tools/list"}')"
  echo "$out" | jq -e --argjson want "$EXPECT" '([.result.tools[].name] | sort) == ($want | sort)' >/dev/null \
    || failed "the home tools left the list while the computer is offline: $out"
  echo "offline answered in ${took} s; the tools stay listed from the last list"
fi

printf '\n== a new passphrase ends every connection\n'
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
# With the home server stopped, its tools still list: the last list is kept
# in KV, across the restart.
SMOKE_TOKEN_OUT=.e2e/tokens-after.json sh scripts/smoke.sh "$BASE" >.e2e/smoke-after.log 2>&1 \
  || { cat .e2e/smoke-after.log >&2; failed "connecting with the new passphrase"; }
echo "the new passphrase connects"

# Every module is optional: its secrets left empty or `none` (the Deploy
# form's default) turn it off, and its tools go with it.
printf '\n== each module is optional\n'
cp .dev.vars .e2e/dev.vars.full
only() { # <label> <tools JSON> <sed script for .dev.vars>
  stop_worker
  sed "$3" .e2e/dev.vars.full >.dev.vars
  start_worker
  out="$(rpc_with .e2e/tokens-after.json '{"jsonrpc":"2.0","id":1,"method":"tools/list"}')"
  echo "$out" | jq -e --argjson want "$2" '([.result.tools[].name] | sort) == ($want | sort)' >/dev/null \
    || failed "$1: unexpected tools: $out"
  printf '%s: %s\n' "$1" "$(echo "$out" | jq -c '[.result.tools[].name]')"
}
only "Coinbase only" '["coinbase_accounts","coinbase_transactions"]' \
  's/^ALP_API_KEY_ID=.*/ALP_API_KEY_ID=none/; s/^ALP_API_SECRET_KEY=.*/ALP_API_SECRET_KEY=none/; /^HOME_/d'
only "Alpaca only" '["alpaca_portfolio","alpaca_activities"]' \
  's/^CBASE_API_KEY_NAME=.*/CBASE_API_KEY_NAME=none/; s/^CBASE_API_PRIVATE_KEY=.*/CBASE_API_PRIVATE_KEY=/; /^HOME_/d'
only "nothing set up" '[]' \
  's/^CBASE_API_KEY_NAME=.*/CBASE_API_KEY_NAME=none/; s/^CBASE_API_PRIVATE_KEY=.*/CBASE_API_PRIVATE_KEY=none/; s/^ALP_API_KEY_ID=.*/ALP_API_KEY_ID=/; s/^ALP_API_SECRET_KEY=.*/ALP_API_SECRET_KEY=none/; /^HOME_/d'
out="$(rpc_with .e2e/tokens-after.json '{"jsonrpc":"2.0","id":2,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"e2e","version":"0"}}}')"
echo "$out" | jq -e '.result.instructions | startswith("No tools are set up")' >/dev/null \
  || failed "with nothing set up, the instructions do not say so: $out"
out="$(rpc_with .e2e/tokens-after.json "$(call_json 3 coinbase_accounts)")"
echo "$out" | jq -e '.error.code == -32602' >/dev/null || failed "a module that is off still answered: $out"
echo "a module that is off is unknown, and the instructions say nothing is set up"
cp .e2e/dev.vars.full .dev.vars
printf '\nOK\n'
