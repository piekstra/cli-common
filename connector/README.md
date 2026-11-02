# Your own Claude connector for your CLI data

A Cloudflare Worker that you deploy into **your own** Cloudflare account and
add to Claude (web, mobile and desktop) as a custom connector. It gives Claude
read-only tools over your own accounts, built from the same open-source code
as the family's command-line tools. There is no service in between: your API
keys are secrets in your Worker, and only someone with your passphrase can
connect a client to it.

[![Deploy to Cloudflare](https://deploy.workers.cloudflare.com/button)](https://deploy.workers.cloudflare.com/?url=https://github.com/piekstra/cli-common/tree/main/connector)

Tools it serves:

| Tools | From | Answers |
|---|---|---|
| `coinbase_accounts`, `coinbase_transactions` | [coinbase-cli](https://github.com/piekstra/coinbase-cli) | What do I hold on Coinbase? What happened in a wallet, over a period? |

## What you need

- A Cloudflare account. The free plan is enough: it allows 100,000 requests
  a day and 10 ms of CPU per request, and a tool call uses 4 to 6 ms
  (measured below).
- A GitHub or GitLab account. The Deploy button copies this folder into a new
  repository there, and Cloudflare redeploys from it whenever it changes.
- A Claude plan that includes custom connectors.
- A Coinbase account.

## 1. Create a view-only Coinbase API key

1. Open the [Coinbase Developer Platform](https://portal.cdp.coinbase.com/projects/api-keys)
   and sign in with your Coinbase account.
2. Create a **Secret API key**. Name it after this connector.
3. Under permissions, give it **View** only. Leave Trade and Transfer off.
4. Under the advanced settings, choose the **ECDSA** signature algorithm.
   Coinbase's App API, which these tools read, documents Ed25519 keys as
   unsupported.
5. Leave the IP allowlist empty: Cloudflare Workers do not call out from
   fixed addresses.
6. Create the key and keep the key file Coinbase offers you. It holds the two
   values you need next: `name` (`organizations/…/apiKeys/…`) and
   `privateKey` (a PEM block from `-----BEGIN EC PRIVATE KEY-----` to
   `-----END EC PRIVATE KEY-----`).

## 2. Deploy the Worker

1. Make a passphrase of 20 characters or more and save it in your password
   manager, for example the output of `openssl rand -base64 24`. You type it
   each time you connect a client.
2. Click **Deploy to Cloudflare** above. Sign in to Cloudflare and connect your
   GitHub or GitLab account when asked.
3. Fill in the form:

   | Field | Value |
   |---|---|
   | Project or Worker name | anything; it becomes part of the URL |
   | `OAUTH_KV` | create a new namespace |
   | `OWNER_PASSPHRASE` | the passphrase from step 1 |
   | `CBASE_API_KEY_NAME` | the key file's `name` |
   | `CBASE_API_PRIVATE_KEY` | the key file's `privateKey`, the whole PEM; pasting it on one line is fine |

4. Click **Create and deploy**. The build downloads the Coinbase module from
   coinbase-cli's release and refuses to deploy unless its SHA-256 matches
   the pin in `modules.json`.
5. When it finishes, open the Worker's URL
   (`https://<worker-name>.<your-subdomain>.workers.dev`). It says "This is a
   personal MCP connector".

## 3. Connect Claude

1. In Claude, open **Settings → Connectors** and choose **Add custom
   connector**.
2. Name it (for example "Coinbase") and enter your Worker's URL with `/mcp`
   on the end: `https://<worker-name>.<your-subdomain>.workers.dev/mcp`.
   Leave the advanced OAuth settings empty.
3. Click **Connect**. Your Worker's own page opens and asks for the
   passphrase. Enter it and click **Connect**.
4. Back in Claude, the connector lists two tools. Ask "What do I hold on
   Coinbase?" to try it. Connectors you add on claude.ai also appear in the
   Claude mobile apps under the same account.

A connection stays signed in while you use it. After 30 days without use,
Claude asks you to connect again.

## Updating, rotating and removing

**A new coinbase-cli release.** The repository the Deploy button made for you
holds a copy of `modules.json`. Change the coinbase entry's `url` to the new
release's `cbase-mcp-<version>.wasm` and its `sha256` to the value in the
release's `cbase-mcp-<version>.wasm.sha256`, then commit. Cloudflare rebuilds
and redeploys.

**A new version of this connector.** Your repository is a copy of this
folder, at its root, with no shared history, so an update is a copy rather
than a merge. From a clone of your repository:

```sh
git clone --depth 1 https://github.com/piekstra/cli-common /tmp/cli-common
rsync -a --delete --exclude .git --exclude modules.json /tmp/cli-common/connector/ ./
git add -A && git commit -m "connector: update from cli-common"
git push
```

That makes your repository match this folder except for `modules.json`,
which keeps your pins: compare it with
`/tmp/cli-common/connector/modules.json` and take the newer pins.

**A new Coinbase key.** Create the new key (step 1), then in the Cloudflare
dashboard open the Worker's **Settings → Variables and Secrets** and replace
`CBASE_API_KEY_NAME` and `CBASE_API_PRIVATE_KEY`. Delete the old key in the
Coinbase Developer Platform.

**A new passphrase.** Replace `OWNER_PASSPHRASE` the same way. Every
connection made under the old one ends at once: each grant records which
passphrase approved it, and the Worker refuses its tokens once that has
changed. Connect Claude again with the new passphrase. To disconnect every
client without changing the passphrase, open `https://<your-worker>/revoke`
and enter it.

**Removing it.** Remove the connector in Claude's settings, delete the Worker
in Cloudflare, and delete the API key in the Coinbase Developer Platform.

## Threat model

Who holds what:

| Party | Holds | Can |
|---|---|---|
| You | your Cloudflare account, the passphrase | everything below |
| Your Worker, in your Cloudflare account | the Coinbase key and the passphrase as Worker secrets (encrypted, never shown again in the dashboard); OAuth clients and grants in KV, tokens stored only as hashes | read your Coinbase data for a connected client |
| Cloudflare | runs your Worker | the access any hosting provider has to what it runs |
| Claude (Anthropic) | an access token (1 hour) and a refresh token for your Worker | call the read-only tools while connected; sees the results in your conversation |
| Coinbase | the key's public half | check the Worker's signed requests |
| The authors of this code | nothing: no service, no telemetry | publish new versions, which reach your Worker only when you update `modules.json` or merge them |
| Anyone else | your Worker's URL | open the passphrase page; ten wrong passphrases in an hour lock it and `/revoke` until the hour ends |

**The key is read-only twice over.** Coinbase refuses any trade or transfer
made with a View-only key, and the module never asks: its client sends `GET`
requests only, enforced in coinbase-cli's code. Nothing reachable through
this connector can move money.

**Codes go only to Claude or your own computer.** The authorization code is
only ever sent to Claude's callback (`https://claude.ai/api/mcp/auth_callback`,
or its `claude.com` twin) or to a loopback address, for clients such as
Claude Code and the MCP Inspector. Clients may register themselves, but only
with those callbacks. To allow another client, set `EXTRA_REDIRECT_URIS`
(comma-separated, matched on origin and path) in the Worker's variables.

**A leaked passphrase** lets whoever has it connect a client of their own,
from their own computer, and read what the tools return: your Coinbase
balances and transaction history. It does not let them trade, move funds or
see the key. Change the passphrase in the Cloudflare dashboard (above): that
ends every connection at once, theirs included, and needs no page that a
guesser could lock.

**A guesser can lock the pages, not your Worker.** Ten wrong passphrases in a
clock hour close `/authorize` and `/revoke` until the hour ends. Connections
that already exist keep working. Changing the passphrase still ends them all.
To reopen the pages at once, delete the `owner-auth-failures:…` key in the
Worker's `OAUTH_KV` namespace (Cloudflare dashboard, **Storage & databases →
Workers KV**).

**A leaked Coinbase key** lets whoever has it read your Coinbase account
directly, without this Worker. Delete the key in the Coinbase Developer
Platform and make a new one.

**The modules are pinned.** The build downloads each tool module from its
CLI's GitHub release and checks it against the SHA-256 in `modules.json`; a
file that does not match fails the build. No binary is committed here. The
host refuses a module that imports anything but its five host functions, so a
module reaches the network only through the Worker's `fetch`, and never
follows a redirect.

**Logs stay in your account.** Workers Logs record each request and any error
the Worker logs. Tool results are not logged.

## How it works

```
Claude ──Streamable HTTP + OAuth──▶ your Worker (src/index.ts)
                                      ├─ workers-oauth-provider: registration, PKCE, tokens in KV
                                      ├─ /authorize, /revoke: the passphrase pages
                                      └─ /mcp: MCP server ─▶ src/pk-host.ts ─▶ cbase-mcp-<version>.wasm
                                                                                  └─ fetch ─▶ api.coinbase.com
```

Each CLI's MCP tools are its own Rust code compiled to one WebAssembly file
(pk-cli-mcp's `export_wasm!`) and attached to that CLI's GitHub release. The
Worker never interprets them: `tools/list` is what the modules describe, a
`tools/call` goes to the module that owns the tool with the Worker secrets it
reads, and the result comes back unchanged. `src/pk-host.ts` runs the
modules; it is an exact copy of cli-common's
`crates/pk-cli-mcp/js/pk-host.ts`, checked identical in CI.

| Path | What |
|---|---|
| `src/index.ts` | The Worker: OAuth provider, MCP server, the pages |
| `src/modules.ts` | The tool modules and the secrets each one reads |
| `modules.json` | Each module's release URL and SHA-256 |
| `scripts/fetch-modules.mjs` | The build step: download and verify the modules |
| `src/pk-host.ts` | Runs a module (copy of cli-common's reference host) |
| `src/owner.ts`, `src/pages.ts` | Passphrase check and lockout, the callback allowlist, the HTML pages |
| `mock/` | A localhost Coinbase that verifies each request's JWT, with invented data |
| `scripts/e2e.sh`, `scripts/smoke.sh` | The end-to-end check CI runs |
| `test/` | Unit tests for the owner gate and the pages' policy |

### Adding another CLI

A CLI qualifies when it reads an official API with a key its user can create,
and builds its tools with pk-cli-mcp's `wasm` feature (coinbase-cli's
`src/worker.rs` is the example). Then:

1. Add its release asset to `modules.json` (`name`, `file`, `url`, `sha256`).
2. Import the file in `src/modules.ts` and list the Worker secrets it reads.
3. Add those secrets to `.dev.vars.example` and describe them under
   `cloudflare.bindings` in `package.json`, where the Deploy button reads
   them.
4. Extend the mock and `scripts/smoke.sh` to cover its tools.

## Developing

Needs Node 22.18 or later (it runs the TypeScript tests directly), `curl`,
`jq` and `openssl`. No Cloudflare account and no login: everything runs on
`127.0.0.1`.

```sh
npm ci
npm run typecheck
npm test               # the owner gate and the pages' policy
npm run e2e            # dummy key, mock, `wrangler dev --local`, then the smoke test
```

`npm run e2e` walks the whole flow the way Claude does: discovery, dynamic
registration (a foreign callback refused), the passphrase page (a wrong
passphrase refused), PKCE token exchange, `initialize`, `tools/list`, both
tools against the mock, the error paths, refresh-token rotation and
`/revoke`. It then restarts the Worker under a new passphrase and checks that
the earlier connection's tokens are refused and the new passphrase connects.
Each run starts from empty local storage.

To try a module that is not released yet, point the build at a local file:
`CONNECTOR_MODULE_COINBASE=/path/to/cbase.wasm npm run e2e` (the build log
says it is unverified). For a session by hand: `npm run keys`, `npm run mock`,
then `npm run dev`; the MCP Inspector can connect to
`http://127.0.0.1:8787/mcp` with the passphrase from `.dev.vars`.

Measured with `cbase-mcp-0.3.0.wasm` in `wrangler dev --local` (workerd's
process CPU over 300 requests of each kind, `scripts/bench-workerd.sh`, three
runs):

| Request | CPU per request |
|---|---|
| `tools/list` (token check, KV read, MCP framing) | 2.4 to 3.6 ms |
| `coinbase_accounts`, two pages: two requests, each JWT signed by WebCrypto | 3.6 to 5.6 ms |
| the difference: the module and its two fetches | 1.2 to 2.0 ms |

The upload is 1,429 KiB, 401 KiB gzipped, against the free plan's 3 MiB.
