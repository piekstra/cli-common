#!/bin/sh
# Measure the cost of a tools/call inside workerd (`wrangler dev --local`),
# against the localhost mock. The local runtime reports no per-request CPU
# time, so this samples the workerd process's CPU time around N requests of
# each kind and divides. tools/list does the same OAuth token check, KV read
# and MCP framing as tools/call, so the difference between the two is the
# WebAssembly tool itself plus its fetches to the mock.
#
#   sh scripts/bench-workerd.sh <access_token> [N] [base]
set -eu
ACCESS="$1"
N="${2:-200}"
BASE="${3:-http://127.0.0.1:8787}"
# This checkout's workerd only (other projects may run their own), summed
# over its processes.
PIDS="$(pgrep -f "$(cd "$(dirname "$0")/.." && pwd)/node_modules/@cloudflare/workerd" | paste -sd, -)"
[ -n "$PIDS" ] || { echo "this checkout's workerd is not running" >&2; exit 1; }

cpu_ms() { ps -o time= -p "$PIDS" | awk -F'[:.]' '{ if (NF==3) t+=($1*60+$2)*1000+$3*10; else t+=(($1*60+$2)*60+$3)*1000+$4*10 } END { print t }'; }

run() {
  body="$1"
  start=$(cpu_ms)
  i=0
  while [ $i -lt "$N" ]; do
    curl -s -o /dev/null "$BASE/mcp" -H "authorization: Bearer $ACCESS" -H 'content-type: application/json' \
      -H 'accept: application/json, text/event-stream' -H 'mcp-protocol-version: 2025-11-25' -d "$body"
    i=$((i + 1))
  done
  end=$(cpu_ms)
  echo "$(( end - start ))"
}

run '{"jsonrpc":"2.0","id":1,"method":"tools/list"}' >/dev/null # warm-up
LIST=$(run '{"jsonrpc":"2.0","id":1,"method":"tools/list"}')
CALL=$(run '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"coinbase_accounts","arguments":{}}}')
awk -v n="$N" -v l="$LIST" -v c="$CALL" 'BEGIN {
  printf "workerd process CPU over %d requests each (ps, 10 ms resolution)\n", n
  printf "  tools/list:                      %6.3f ms/request\n", l / n
  printf "  tools/call coinbase_accounts (2 pages, 2 JWTs): %6.3f ms/request\n", c / n
  printf "  difference (the WASM tool):      %6.3f ms/request\n", (c - l) / n
}'
