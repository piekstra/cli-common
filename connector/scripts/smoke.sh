#!/bin/sh
# End-to-end check of the connector on localhost, with curl, the way claude.ai
# drives it: discovery, dynamic registration, PKCE authorization through the
# owner passphrase page, token exchange, then MCP JSON-RPC over Streamable
# HTTP, and a refresh-token rotation.
#
# Prereqs (three terminals, or background them):
#   node scripts/gen-dummy-key.mjs      # once: dummy key + .dev.vars
#   node mock/coinbase-mock.mjs         # localhost Coinbase stand-in, :8788
#   npx wrangler dev --local --port 8787
#   sh scripts/smoke.sh [http://127.0.0.1:8787]
#
# Needs curl, jq, openssl.
set -eu
cd "$(dirname "$0")/.."
BASE="${1:-http://127.0.0.1:8787}"
REDIRECT="http://127.0.0.1:9/callback"
JAR="$(mktemp)"
trap 'rm -f "$JAR"' EXIT
PASSPHRASE="$(sed -n 's/^OWNER_PASSPHRASE=//p' .dev.vars)"

step() { printf '\n== %s\n' "$*"; }
fail() { echo "FAIL: $*" >&2; exit 1; }
b64url() { openssl base64 -A | tr '+/' '-_' | tr -d '='; }

step "unauthenticated /mcp is challenged"
curl -s -o /dev/null -D - -X POST "$BASE/mcp" -H 'content-type: application/json' -d '{}' \
  | grep -i '^www-authenticate' || fail "no WWW-Authenticate challenge"

step "discovery"
curl -sf "$BASE/.well-known/oauth-protected-resource/mcp" | jq -c '{resource, authorization_servers, scopes_supported}'
AS="$(curl -sf "$BASE/.well-known/oauth-authorization-server")"
echo "$AS" | jq -c '{issuer, authorization_endpoint, token_endpoint, registration_endpoint, code_challenge_methods_supported, token_endpoint_auth_methods_supported}'

step "dynamic registration refuses a foreign redirect URI"
code=$(curl -s -o /dev/null -w '%{http_code}' "$BASE/oauth/register" -H 'content-type: application/json' \
  -d '{"client_name":"evil","redirect_uris":["https://evil.example/cb"],"token_endpoint_auth_method":"none"}')
[ "$code" = 400 ] || fail "expected 400, got $code"
echo "400 as expected"

step "dynamic registration (public client, loopback redirect)"
CLIENT_ID="$(curl -sf "$BASE/oauth/register" -H 'content-type: application/json' \
  -d "{\"client_name\":\"smoke test\",\"redirect_uris\":[\"$REDIRECT\"],\"token_endpoint_auth_method\":\"none\",\"grant_types\":[\"authorization_code\",\"refresh_token\"],\"response_types\":[\"code\"]}" \
  | jq -r .client_id)"
echo "client_id=$CLIENT_ID"

VERIFIER="$(openssl rand 32 | b64url)"
CHALLENGE="$(printf %s "$VERIFIER" | openssl dgst -sha256 -binary | b64url)"
STATE="$(openssl rand -hex 8)"
AUTHZ="$BASE/authorize?response_type=code&client_id=$CLIENT_ID&redirect_uri=$(jq -rn --arg u "$REDIRECT" '$u|@uri')&code_challenge=$CHALLENGE&code_challenge_method=S256&state=$STATE&scope=mcp:read&resource=$(jq -rn --arg u "$BASE/mcp" '$u|@uri')"

step "GET /authorize renders the passphrase page"
PAGE="$(curl -sf -c "$JAR" -b "$JAR" "$AUTHZ")"
HANDLE="$(printf %s "$PAGE" | sed -n 's/.*name="handle" value="\([^"]*\)".*/\1/p')"
[ -n "$HANDLE" ] || fail "no consent handle in page"
printf %s "$PAGE" | grep -o '<h1>[^<]*</h1>'

step "a wrong passphrase is refused"
code=$(curl -s -o /dev/null -w '%{http_code}' -c "$JAR" -b "$JAR" "$BASE/authorize" \
  --data-urlencode "handle=$HANDLE" --data-urlencode "passphrase=not-the-passphrase" -d decision=approve)
[ "$code" = 403 ] || fail "expected 403, got $code"
echo "403 as expected"

step "the right passphrase redirects with a code"
LOCATION="$(curl -s -o /dev/null -w '%{redirect_url}' -c "$JAR" -b "$JAR" "$BASE/authorize" \
  --data-urlencode "handle=$HANDLE" --data-urlencode "passphrase=$PASSPHRASE" -d decision=approve)"
echo "$LOCATION" | sed 's/code=[^&]*/code=<redacted>/'
CODE="$(printf %s "$LOCATION" | sed -n 's/.*[?&]code=\([^&]*\).*/\1/p')"
printf %s "$LOCATION" | grep -q "state=$STATE" || fail "state not echoed"
[ -n "$CODE" ] || fail "no code"

step "token exchange (PKCE)"
TOKENS="$(curl -sf "$BASE/oauth/token" -d grant_type=authorization_code -d "code=$CODE" \
  --data-urlencode "redirect_uri=$REDIRECT" -d "client_id=$CLIENT_ID" -d "code_verifier=$VERIFIER")"
echo "$TOKENS" | jq -c '{token_type, expires_in, scope, has_refresh: (.refresh_token != null)}'
ACCESS="$(echo "$TOKENS" | jq -r .access_token)"
REFRESH="$(echo "$TOKENS" | jq -r .refresh_token)"

# One JSON-RPC call. Streamable HTTP may answer as JSON or as an SSE stream;
# take the JSON either way. The code stays percent-encoded as the redirect
# carried it, so it goes back with -d, not --data-urlencode.
rpc() {
  curl -sf "$BASE/mcp" -H "authorization: Bearer $ACCESS" -H 'content-type: application/json' \
    -H 'accept: application/json, text/event-stream' -H 'mcp-protocol-version: 2025-11-25' -d "$1" \
    | sed -n -e 's/^data: //p' -e t -e '/^{/p'
}

step "initialize"
rpc '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"smoke","version":"0"}}}' \
  | jq -c '.result | {protocolVersion, serverInfo, capabilities}'

step "tools/list"
rpc '{"jsonrpc":"2.0","id":2,"method":"tools/list"}' | jq -c '.result.tools[] | {name, readOnly: .annotations.readOnlyHint}'

step "tools/call coinbase_accounts_list"
rpc '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"coinbase_accounts_list","arguments":{}}}' \
  | jq -c '.result | {isError, items: [.structuredContent.items[] | {name, balance: .balance.amount, currency}]}'

step "tools/call coinbase_transactions_list (since 2026-01-01)"
rpc '{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"coinbase_transactions_list","arguments":{"account":"11111111-1111-4111-8111-111111111111","since":"2026-01-01"}}}' \
  | jq -c '.result | {isError, items: [.structuredContent.items[] | {date, kind, amount: .amount.amount}]}'

step "tools/call with a bad argument is a tool error, not a crash"
rpc '{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"coinbase_transactions_list","arguments":{"account":"x","since":"yesterday"}}}' \
  | jq -c '.result | {isError, text: .content[0].text}'

step "refresh token rotates"
NEW="$(curl -sf "$BASE/oauth/token" -d grant_type=refresh_token --data-urlencode "refresh_token=$REFRESH" -d "client_id=$CLIENT_ID")"
[ "$(echo "$NEW" | jq -r .refresh_token)" != "$REFRESH" ] || fail "refresh token did not rotate"
ACCESS="$(echo "$NEW" | jq -r .access_token)"
rpc '{"jsonrpc":"2.0","id":6,"method":"tools/list"}' | jq -c '{tools_after_refresh: (.result.tools | length)}'

# For scripts/bench-workerd.sh: hand the live access token on, if asked.
if [ -n "${SMOKE_TOKEN_OUT:-}" ]; then
  (umask 077; printf %s "$ACCESS" > "$SMOKE_TOKEN_OUT")
fi
printf '\nOK\n'
