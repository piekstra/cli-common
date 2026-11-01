# connector (spike)

A Cloudflare Worker that each user deploys into their own Cloudflare account
and adds to Claude (web, desktop, mobile) as a custom connector. It serves
piekstra-family CLI tools over MCP. No shared service holds anyone's
credentials: the provider key lives in the deployer's Worker secrets, and only
the deployer can approve a connection.

**Status: localhost prototype, not for merge.** Nothing here has been
deployed. The first tool set is coinbase-cli's two reads.

```
claude.ai ──Streamable HTTP + OAuth──▶ Worker (TypeScript shell)
                                         ├─ workers-oauth-provider: DCR, CIMD, PKCE, tokens in KV
                                         ├─ /authorize: owner passphrase page
                                         └─ /mcp: agents createMcpHandler + MCP SDK v2 Server
                                               └─ tools() / call(name, args)  ◀── JSON boundary
                                                     cbase_wasm (Rust → wasm32)
                                                       └─ coinbase-cli core: JWT, client, parsers
                                                             └─ fetch ──▶ api.coinbase.com
```

## Layout

| Path | What |
|---|---|
| `src/index.ts` | Worker entry: OAuth provider, MCP handler, `/authorize` |
| `src/owner.ts` | Passphrase check, failure lockout, redirect URI allowlist |
| `src/pages.ts` | The approval page and message pages |
| `cbase-wasm/` | wasm-bindgen glue: `tools()`, `call()`, fetch transport, WebCrypto ES256 |
| `coinbase-cli.patch` | The coinbase-cli change this needs, against its `v0.1.1` tag |
| `mock/` | A localhost Coinbase stand-in that verifies the CDP JWTs, with invented fixtures |
| `scripts/` | Build, dummy keys, smoke test, benchmarks |

## Run it locally

Needs Node 20+, Rust with `wasm32-unknown-unknown`, `wasm-bindgen-cli` at the
version in `cbase-wasm/Cargo.lock`, and optionally `wasm-opt`. No Cloudflare
login.

```sh
npm install
npm run build:wasm      # clones coinbase-cli v0.1.1, applies the patch, builds pkg/
npm run keys            # throwaway EC key for the mock + .dev.vars (gitignored)
npm run mock &          # http://127.0.0.1:8788
npm run dev &           # http://127.0.0.1:8787
npm run smoke           # discovery → DCR → passphrase → PKCE token → MCP list/call → refresh
```

The MCP Inspector can connect to `http://127.0.0.1:8787/mcp` the same way;
the passphrase is `OWNER_PASSPHRASE` in `.dev.vars`.

## Security model

- `/authorize` requires `OWNER_PASSPHRASE` (a Worker secret, 20+ characters,
  compared in constant time). Ten wrong attempts in a clock hour lock it until
  the hour ends.
- Codes go only to Claude's callback (`https://claude.ai/api/mcp/auth_callback`)
  or a loopback client. Dynamic registration refuses other redirect URIs, and
  `/authorize` re-checks every request's.
- The approval page shows the client's name and where access is sent, warns on
  loopback, cannot be framed, and posts a single-use handle bound to the
  browser by a `__Host-` cookie (`beginConsent`).
- Tokens, codes and client secrets are stored in KV only as hashes; grant
  props are encrypted (workers-oauth-provider).
- Every tool is read-only and says so in its annotations.

## Deploy (owner only, not done in this spike)

The Deploy to Cloudflare button would point at this directory; it provisions
the `OAUTH_KV` namespace and prompts for the two secrets in
`.dev.vars.example`. The wasm must be prebuilt for that (see the PR).
