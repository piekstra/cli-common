# Your own Claude connector for your CLI data

A Cloudflare Worker that you deploy into **your own** Cloudflare account and
add to Claude (web, mobile and desktop) as a custom connector. It gives Claude
read-only tools over your own accounts, built from the same open-source code
as the family's command-line tools. There is no service in between: your API
keys are secrets in your Worker, and only someone with your passphrase can
connect a client to it.

[![Deploy to Cloudflare](https://deploy.workers.cloudflare.com/button)](https://deploy.workers.cloudflare.com/?url=https://github.com/piekstra/cli-common/tree/main/connector)

Tools it can serve. Every module is optional: you turn on the ones you use.

| Tools | From | Runs | Answers |
|---|---|---|---|
| `coinbase_accounts`, `coinbase_transactions` | [coinbase-cli](https://github.com/piekstra/coinbase-cli) | in the Worker | What do I hold on Coinbase? What happened in a wallet, over a period? |
| `alpaca_portfolio`, `alpaca_activities` | [alpaca-cli](https://github.com/piekstra/alpaca-cli) | in the Worker | What is my Alpaca account worth, and what do I hold? Which fills, dividends and fees, over a period? |
| `m1_…`, `robinhood_…` and other portal CLIs' tools | [m1-cli](https://github.com/piekstra/m1-cli), [robinhood-cli](https://github.com/piekstra/robinhood-cli) | on your computer, through your tunnel ([home modules](#tools-from-your-computer-home-modules)) | whatever that CLI's `mcp` tools answer |
| `fpl_…`, `jupiter_…`, `loxahatchee_…`, `xfinity_…` | [fpl-cli](https://github.com/piekstra/fpl-cli), [town-of-jupiter-fl-cli](https://github.com/piekstra/town-of-jupiter-fl-cli), [loxahatchee-river-fl-cli](https://github.com/piekstra/loxahatchee-river-fl-cli), [xfinity-cli](https://github.com/piekstra/xfinity-cli) | on your computer, through your tunnel ([home modules](#tools-from-your-computer-home-modules)) | What do I owe, and when is it due? What did past bills come to? Which payments posted? How much did I use? |
| `schwab_…`, `nelnet_…`, `chase_…`, `lofty_…` | [schwab-cli](https://github.com/piekstra/schwab-cli), [nelnet-cli](https://github.com/piekstra/nelnet-cli), [chase-cli](https://github.com/piekstra/chase-cli), [lofty-cli](https://github.com/piekstra/lofty-cli) | on your computer, through your tunnel ([home modules](#tools-from-your-computer-home-modules)) | What do I hold, and what happened over a period? What do I owe on my loans? What did I spend, in the Chase activity I downloaded? |
| `campbell_…`, `rpm_…`, `loftypm_…`, `kin_…` | [campbell-property-management-hoa-cli](https://github.com/piekstra/campbell-property-management-hoa-cli), [rpm-fl-cli](https://github.com/piekstra/rpm-fl-cli), [lofty-pm-cli](https://github.com/piekstra/lofty-pm-cli), [kin-cli](https://github.com/piekstra/kin-cli) | on your computer, through your tunnel ([home modules](#tools-from-your-computer-home-modules)) | What do I owe my HOA? How did my rentals do? Which Lofty votes wait on me? What does my home insurance cover? |

The Coinbase and Alpaca modules call official APIs with keys you create, so
they run inside the Worker. Portal CLIs (M1, Robinhood) sign in through a
browser, keep the session on your computer, and are refused by their
providers when called from a datacenter, so they stay on your computer: the
Worker reaches them through a Cloudflare Tunnel in your account.

## What you need

- A Cloudflare account. The free plan is enough: it allows 100,000 requests
  a day and 10 ms of CPU per request, and a tool call used 4 to 5 ms on
  an idle laptop ([measured below](#developing)).
- A GitHub or GitLab account. The Deploy button copies this folder into a new
  repository there, and Cloudflare redeploys from it whenever it changes.
- A Claude plan that includes custom connectors.
- An account with at least one of the services above.

## 1. Create the keys for the services you use

### Coinbase: a view-only API key

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

### Alpaca: an API key pair

1. Sign in to the [Alpaca dashboard](https://app.alpaca.markets) and switch
   to the account you want Claude to read: live, or paper.
2. Under **API Keys**, generate a key. Alpaca shows the **Key ID** and the
   **Secret Key**; the secret only once, so copy both now.
3. Alpaca keys cannot be limited to reading: the same pair can place
   orders. The connector's Alpaca code has no write path (it sends `GET`
   requests only), but treat the pair like a password (see the
   [threat model](#threat-model)).

## 2. Deploy the Worker

1. Make a passphrase of 20 characters or more and save it in your password
   manager, for example the output of `openssl rand -base64 24`. You type it
   each time you connect a client.
2. Click **Deploy to Cloudflare** above. Sign in to Cloudflare and connect your
   GitHub or GitLab account when asked.
3. Fill in the form. Each module's secrets come pre-filled with `none`;
   leave `none` for a service you do not use, and its tools stay off.

   | Field | Value |
   |---|---|
   | Project or Worker name | anything; it becomes part of the URL |
   | `OAUTH_KV` | create a new namespace |
   | `OWNER_PASSPHRASE` | the passphrase from step 1 (required) |
   | `CBASE_API_KEY_NAME` | Coinbase: the key file's `name`, or `none` |
   | `CBASE_API_PRIVATE_KEY` | Coinbase: the key file's `privateKey`, the whole PEM (one line is fine), or `none` |
   | `ALP_API_KEY_ID` | Alpaca: the Key ID, or `none` |
   | `ALP_API_SECRET_KEY` | Alpaca: the Secret Key, or `none` |

4. Click **Create and deploy**. The build downloads each module from its
   CLI's release and refuses to deploy unless its SHA-256 matches the pin in
   `modules.json`.
5. For Alpaca **paper** keys only: in the Worker's **Settings → Variables and
   Secrets**, add a variable `ALP_ENV` with the value `paper`.
6. Open the Worker's URL (`https://<worker-name>.<your-subdomain>.workers.dev`).
   It says "This is a personal MCP connector".

## 3. Connect Claude

1. In Claude, open **Settings → Connectors** and choose **Add custom
   connector**.
2. Name it (for example "My accounts") and enter your Worker's URL with
   `/mcp` on the end: `https://<worker-name>.<your-subdomain>.workers.dev/mcp`.
   Leave the advanced OAuth settings empty.
3. Click **Connect**. Your Worker's own page opens and asks for the
   passphrase. Enter it and click **Connect**.
4. Back in Claude, the connector lists the tools of the modules you turned
   on. Ask "What do I hold on Coinbase?" or "What is my Alpaca account
   worth?" to try it. Connectors you add on claude.ai also appear in the
   Claude mobile apps under the same account.

A connection stays signed in while you use it. After 30 days without use,
Claude asks you to connect again.

## Turning a module on or off later

A module is on when any of its secrets is set to something other than
`none` or empty. In the Cloudflare dashboard, open the Worker's **Settings →
Variables and Secrets**, and set (or add) its secrets; each save redeploys.
Set them back to `none` to turn the module off. In Claude, open the
connector's settings and refresh its tools if the list did not change.

### Adding Alpaca to a deployment you already have

A deployment made before Alpaca was added has neither its code nor its
secrets. Your repository needs this version of the connector first:

1. Create the Alpaca key pair ([above](#alpaca-an-api-key-pair)).
2. Bring your repository up to date ([A new version of this
   connector](#updating-rotating-and-removing) below), and in your
   `modules.json` add the `alpaca` entry from this folder's
   `modules.json`, keeping your other pins.
3. Commit and push. Cloudflare rebuilds and redeploys; the build log shows
   `alpaca: fetched … sha256 verified`.
4. In the Worker's **Settings → Variables and Secrets**, add two secrets
   (type **Secret**): `ALP_API_KEY_ID` and `ALP_API_SECRET_KEY`. For paper
   keys, also add the variable `ALP_ENV` = `paper`.
5. In Claude, refresh the connector's tools (or remove and add it again).
   `alpaca_portfolio` and `alpaca_activities` appear beside the others.

## Tools from your computer (home modules)

A portal CLI such as `m1f` (M1 Finance) or `rhd` (Robinhood) signs in
through a browser and keeps that session in your computer's keychain. Its
provider refuses requests from datacenter addresses, so its tools cannot
run in the Worker. Instead the CLI serves them on your computer, on
`127.0.0.1` only, behind a bearer token, and the Worker reaches that port
through a **Cloudflare Tunnel** in your account (Workers VPC, free while in
beta). The session never leaves your computer; the Worker holds only the
bearer token.

The utility CLIs keep their credentials on your computer and run the same way
(`lrfl` needs none, since the district's lookup is anonymous). So do `schw`, `nlnt` and
`lofty`, whose session or API key stays on your computer, and `chse`, which has no
credential and reads the CSV files in a folder you name. So do `cpmfl`, `rpmfl` and `kin`,
whose portal session stays on your computer, and `lofty-pm`, which reads the local store
its `scan` fills from your email. Each README's "Use with Claude"
section names its port and connector secret, for the steps below:

| CLI | Tools | Port | Connector secret |
|---|---|---|---|
| `fpl` | `fpl_…` | 8732 | `HOME_FPL_TOKEN` |
| `xfin` | `xfinity_…` | 8733 | `HOME_XFINITY_TOKEN` |
| `lrfl` | `loxahatchee_…` | 8734 | `HOME_LOXAHATCHEE_TOKEN` |
| `tojfl` | `jupiter_…` | 8735 | `HOME_JUPITER_TOKEN` |
| `schw` | `schwab_…` | 8736 | `HOME_SCHWAB_TOKEN` |
| `nlnt` | `nelnet_…` | 8737 | `HOME_NELNET_TOKEN` |
| `chse` | `chase_…` | 8738 | `HOME_CHASE_TOKEN` |
| `lofty` | `lofty_…` | 8739 | `HOME_LOFTY_TOKEN` |
| `cpmfl` | `campbell_…` | 8750 | `HOME_CAMPBELL_TOKEN` |
| `rpmfl` | `rpm_…` | 8751 | `HOME_RPM_TOKEN` |
| `lofty-pm` | `loftypm_…` | 8752 | `HOME_LOFTYPM_TOKEN` |
| `kin` | `kin_…` | 8753 | `HOME_KIN_TOKEN` |

The tools work while the computer is awake, signed in to the CLI, and
running both the CLI's server and `cloudflared`. When it is not, those tools
answer `offline:` at once, and the other tools keep working.

The steps for `m1f` follow; for `rhd`, put `rhd` for `m1f` and `ROBINHOOD`
for `M1` (the module's name is its tools' prefix: `m1_…` is `M1`,
`robinhood_…` is `ROBINHOOD`). Use a different port for each CLI.

1. **Sign in to the CLI** on the computer, if you have not: `m1f auth login`.
   You need m1-cli 0.5.0 or later (robinhood-cli 0.4.0); `m1f mcp --help`
   lists `--http`.
2. **Make the bearer token:** `m1f mcp --new-token`. It saves a random
   token in your keychain (`piekstra.m1f`, item `mcp-http-token`) and prints
   it once. Keep it for step 7. To replace it later:
   `m1f mcp --new-token --overwrite`.
3. **Run the server** and leave it running: `m1f mcp --http --port 8731`.
   It prints one line when it is serving, naming its version, and refuses to
   start without the token. To start it at login on macOS, save this as
   `~/Library/LaunchAgents/com.piekstra.m1f-mcp-http.plist` (put the output
   of `which m1f` in the first string, and your home folder in the log path):

   ```xml
   <?xml version="1.0" encoding="UTF-8"?>
   <!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
   <plist version="1.0">
   <dict>
     <key>Label</key>
     <string>com.piekstra.m1f-mcp-http</string>
     <key>ProgramArguments</key>
     <array>
       <string>/Users/you/.cargo/bin/m1f</string>
       <string>mcp</string>
       <string>--http</string>
       <string>--port</string>
       <string>8731</string>
     </array>
     <key>RunAtLoad</key>
     <true/>
     <key>KeepAlive</key>
     <true/>
     <key>StandardErrorPath</key>
     <string>/Users/you/Library/Logs/m1f-mcp-http.log</string>
   </dict>
   </plist>
   ```

   Then `launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/com.piekstra.m1f-mcp-http.plist`.
   The first start may ask once to allow `m1f` to read its keychain items;
   choose **Always Allow**. The token stays in the keychain, never in the
   plist.
4. **Create the tunnel.** In the Cloudflare dashboard, open **Workers VPC →
   Tunnels**, select **Create**, name it, and choose your operating system.
   Install `cloudflared` as shown (on macOS: `brew install cloudflared`;
   version 2025.7.0 or later) and run the service-install command the
   dashboard gives you, with its token. The tunnel connects outward only
   (QUIC, UDP port 7844); no port on your computer opens to the internet.
   The dashboard shows when it is connected.
5. **Create the VPC service.** In **Workers VPC → VPC Services**, select
   **Create**: name it (`m1f`), choose the tunnel, enter `127.0.0.1` as the
   host, choose **Provide port values** with HTTP port `8731`, and keep the
   tunnel as resolver. Copy the **Service ID** it shows. (Or:
   `npx wrangler vpc service create m1f --type http --tunnel-id <TUNNEL_ID> --ipv4 127.0.0.1 --http-port 8731`.)
6. **Bind it to the Worker.** In your repository's `wrangler.jsonc`, add:

   ```jsonc
   "vpc_services": [
     { "binding": "HOME_M1", "service_id": "<SERVICE_ID>" }
   ]
   ```

   Commit and push; Cloudflare redeploys.
7. **Give the Worker the token.** In the Worker's **Settings → Variables
   and Secrets**, add a secret `HOME_M1_TOKEN` with the token from step 2.
8. **Try it.** In Claude, refresh the connector's tools: the `m1_…` tools
   appear, read-only. Ask "What do I hold on M1?".

The server serves only the CLI's read-only tools. `--allow-writes` would add
its write tools (each still needs your approval in Claude); leave it off
unless you want Claude to change something there.

**Without Workers VPC.** If you have a domain on Cloudflare, a tunnel route
works too: in **Zero Trust → Networks → Tunnels**, give the tunnel a public
hostname (`m1f.<your-domain>`) for `http://127.0.0.1:8731`, with
**HTTP Host Header** set to `localhost` (the server refuses any other
`Host`). Put it behind **Access** with a self-hosted application whose only
policy is **Service Auth**, and create a service token. Then set the
Worker's `HOME_M1_URL` to `https://m1f.<your-domain>/mcp`, and
`HOME_M1_ACCESS_CLIENT_ID` and `HOME_M1_ACCESS_CLIENT_SECRET` to the service
token, instead of step 6.

**When it says offline.** The Worker could not reach the server: check that
the computer is awake, that `curl -s -o /dev/null -w '%{http_code}\n' -X POST http://127.0.0.1:8731/mcp`
prints `401` (the server is up and wants its token), and that the tunnel
shows as connected in the dashboard. An `auth:` answer naming
`HOME_M1_TOKEN` means the Worker's token and the server's differ: set them
again (steps 2 and 7), then restart the server.

## Updating, rotating and removing

**A new release of a CLI with a module.** The repository the Deploy button
made for you holds a copy of `modules.json`. Change that module's `url` to the
new release's `<bin>-mcp-<version>.wasm` and its `sha256` to the value in the
release's `<bin>-mcp-<version>.wasm.sha256`, then commit. Cloudflare rebuilds
and redeploys.

**A new release of a CLI with a home module.** `m1f self-update` replaces
the binary on disk, but a running server keeps the old code, and launchd
restarts it only when it exits. Restart it:
`launchctl kickstart -k gui/$(id -u)/com.piekstra.m1f-mcp-http`. The new
server's line in `~/Library/Logs/m1f-mcp-http.log` names its version. A new
binary may ask once more to read its keychain items; choose **Always Allow**.
A Claude Desktop extension for the same CLI runs its own copy: install the new
release's `.mcpb` to replace it.

**A new version of this connector.** Your repository is a copy of this
folder, at its root, with no shared history, so an update is a copy rather
than a merge. From a clone of your repository:

```sh
git clone --depth 1 https://github.com/piekstra/cli-common /tmp/cli-common
rsync -a --delete --exclude .git --exclude modules.json --exclude wrangler.jsonc \
  /tmp/cli-common/connector/ ./
git add -A && git commit -m "connector: update from cli-common"
git push
```

That makes your repository match this folder except for `modules.json` and
`wrangler.jsonc`, which keep your pins and your bindings: compare each with
its copy in `/tmp/cli-common/connector/`, and take the newer pins and any
new settings.

**A new Coinbase or Alpaca key.** Create the new key (step 1), then in the
Cloudflare dashboard open the Worker's **Settings → Variables and Secrets**
and replace its secrets. Then delete the old key with the service.

**A new home token.** Run `m1f mcp --new-token --overwrite`, put the new
value in `HOME_M1_TOKEN`, and restart the server
(`launchctl kickstart -k gui/$(id -u)/com.piekstra.m1f-mcp-http`).

**A new passphrase.** Replace `OWNER_PASSPHRASE` the same way. Every
connection made under the old one ends at once: each grant records which
passphrase approved it, and the Worker refuses its tokens once that has
changed. Connect Claude again with the new passphrase. To disconnect every
client without changing the passphrase, open `https://<your-worker>/revoke`
and enter it.

**Removing it.** Remove the connector in Claude's settings, delete the Worker
in Cloudflare, and delete the API keys with each service. For home modules,
also delete the tunnel and VPC services (**Workers VPC**), uninstall the
`cloudflared` service, and `launchctl bootout gui/$(id -u)/com.piekstra.m1f-mcp-http`.

## Threat model

Who holds what:

| Party | Holds | Can |
|---|---|---|
| You | your Cloudflare account, the passphrase | everything below |
| Your Worker, in your Cloudflare account | the API keys, the home tokens and the passphrase as Worker secrets (encrypted, never shown again in the dashboard); OAuth clients and grants in KV, tokens stored only as hashes; the last tool list of each home module | read your data for a connected client |
| Your computer (home modules) | the portal CLIs' sessions, in its keychain; each server's bearer token | serve that CLI's read-only tools on 127.0.0.1 to holders of the token |
| Cloudflare | runs your Worker and your tunnel | the access any hosting provider has to what it runs and carries |
| Claude (Anthropic) | an access token (1 hour) and a refresh token for your Worker | call the tools while connected; sees the results in your conversation |
| Coinbase, Alpaca | the keys you created there | answer the Worker's requests |
| The authors of this code | nothing: no service, no telemetry | publish new versions, which reach your Worker only when you update `modules.json` or copy them in |
| Anyone else | your Worker's URL | open the passphrase page; ten wrong passphrases in an hour lock it and `/revoke` until the hour ends |

**The Coinbase key is read-only twice over.** Coinbase refuses any trade or
transfer made with a View-only key, and the module never asks: its client
sends `GET` requests only, enforced in coinbase-cli's code.

**The Alpaca key is read-only once.** Alpaca has no read-only keys: the pair
in your Worker could place orders if someone used it directly. The module
sends `GET` requests only (alpaca-cli's client has no write method), so
nothing reachable through this connector trades. A leaked Alpaca pair is a
trading credential: regenerate it in the Alpaca dashboard at once.

**Codes go only to Claude or your own computer.** The authorization code is
only ever sent to Claude's callback (`https://claude.ai/api/mcp/auth_callback`,
or its `claude.com` twin) or to a loopback address, for clients such as
Claude Code and the MCP Inspector. Clients may register themselves, but only
with those callbacks. To allow another client, set `EXTRA_REDIRECT_URIS`
(comma-separated, matched on origin and path) in the Worker's variables.

**A leaked passphrase** lets whoever has it connect a client of their own,
from their own computer, and read what the tools return: balances and
history from every module you turned on, home modules included while your
computer is on. It does not let them trade, move funds or see a key. Change
the passphrase in the Cloudflare dashboard (above): that ends every
connection at once, theirs included, and needs no page that a guesser could
lock.

**A guesser can lock the pages, not your Worker.** Ten wrong passphrases in a
clock hour close `/authorize` and `/revoke` until the hour ends. Connections
that already exist keep working. Changing the passphrase still ends them all.
To reopen the pages at once, delete the `owner-auth-failures:…` key in the
Worker's `OAUTH_KV` namespace (Cloudflare dashboard, **Storage & databases →
Workers KV**).

**A leaked Coinbase key** lets whoever has it read your Coinbase account
directly, without this Worker. Delete the key in the Coinbase Developer
Platform and make a new one.

### The home path

**The session cookies never leave your computer.** The CLI signs in and
calls its provider from your computer, as it does in a terminal. The Worker
sends it tool calls and gets back tool results; it never sees a cookie, a
password or a one-time code, and holds nothing that signs in to M1 or
Robinhood.

**Nothing listens beyond loopback.** `<bin> mcp --http` binds `127.0.0.1`
only, with no flag for another address. `cloudflared` makes outbound
connections only. Your router, your network and the internet see no new
open port.

**A leaked bearer token** (`HOME_M1_TOKEN`) alone reaches nothing: the
server answers only on `127.0.0.1`, so a request needs a way onto your
computer as well, either your tunnel (only your Worker, through a VPC
service in your account; or, on the hostname route, Cloudflare Access's
service token too) or a program already running on your computer. Whoever
has both can call the read-only tools and read what they return. They
cannot call a write tool (none is served without `--allow-writes`), sign in,
or read the session. Make a new token (`--new-token --overwrite`), set it in
the Worker, and restart the server.

**Read-only by default.** Without `--allow-writes`, the server lists and
answers only the tools the CLI marks read-only; a write tool is unknown to
it, not merely hidden.

**The server checks every request.** A request without the token gets 401.
One whose `Host` is not `127.0.0.1` or `localhost`, or that carries an
`Origin` header, gets 403, so a web page you visit cannot reach it by DNS
rebinding; it sends no CORS headers and answers `POST /mcp` only.

**Your computer must be awake.** When it sleeps, the tunnel has no connector
and the Worker answers `offline:` at once; nothing is queued or retried
later.

**Your Cloudflare account is the key to the tunnel.** A tunnel reaches
whatever a VPC service in your account names, from your computer: whoever
controls your Cloudflare account could add a VPC service for another address
on your network. Protect the account with two-factor authentication, and
delete the tunnel when you stop using it.

### Everything else

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
                                      └─ /mcp: MCP server
                                           ├─ src/pk-host.ts ─▶ cbase-mcp-<v>.wasm ─fetch─▶ api.coinbase.com
                                           ├─ src/pk-host.ts ─▶ alp-mcp-<v>.wasm ───fetch─▶ api.alpaca.markets
                                           └─ src/home.ts ──VPC binding──▶ your tunnel ──▶ 127.0.0.1:8731
                                                                                    m1f mcp --http ─▶ M1
```

Each official-API CLI's MCP tools are its own Rust code compiled to one
WebAssembly file (pk-cli-mcp's `export_wasm!`) and attached to that CLI's
GitHub release. The Worker never interprets a tool: `tools/list` is what the
modules describe, a `tools/call` goes to the module that owns the tool (with
the Worker secrets it reads, or through the tunnel), and the result comes back
unchanged. An in-Worker module that is on keeps its tools' names: a home
server cannot list or answer them. `src/pk-host.ts` runs the WebAssembly modules; it is an exact copy
of cli-common's `crates/pk-cli-mcp/js/pk-host.ts`, checked identical in CI.

A home module's tool list is read from the computer at most every five
minutes while it answers, and kept in memory and in KV (written only when it
changes), so its tools stay listed while the computer sleeps. A call pings
the server first (4 s), so a sleeping computer is reported at once rather
than after the call's 150 s limit.

| Path | What |
|---|---|
| `src/index.ts` | The Worker: OAuth provider, MCP server, the pages |
| `src/modules.ts` | The in-Worker modules and the secrets each one reads |
| `src/home.ts` | Home modules: settings, the tunnel, the cached list, offline answers |
| `src/routing.ts` | Which module answers a tool, and the merged tool list |
| `src/settings.ts` | What counts as set (`none` and empty do not) |
| `src/version.ts` | The version MCP clients see; a test holds it to `package.json` |
| `modules.json` | Each module's release URL and SHA-256 |
| `scripts/fetch-modules.mjs` | The build step: download and verify the modules |
| `src/pk-host.ts` | Runs a module (copy of cli-common's reference host) |
| `src/owner.ts`, `src/pages.ts` | Passphrase check and lockout, the callback allowlist, the HTML pages |
| `mock/` | Localhost Coinbase (checks each JWT) and Alpaca (checks the key headers) with invented data, and the stand-in for a VPC binding |
| `scripts/e2e.sh`, `scripts/smoke.sh` | The end-to-end check CI runs |
| `test/` | Unit tests for the owner gate, the pages' policy, home modules and routing |

### Adding another CLI

**In the Worker.** A CLI qualifies when it reads an official API with a key
its user can create, and builds its tools with pk-cli-mcp's `wasm` feature
(coinbase-cli's and alpaca-cli's `src/worker.rs` are the examples). Then:

1. Add its release asset to `modules.json` (`name`, `file`, `url`, `sha256`).
2. Import the file in `src/modules.ts`; list the secrets and vars it reads
   (`config`) and the secrets that turn it on (`secrets`).
3. Add those secrets to `.dev.vars.example` with the value `none`, and
   describe them under `cloudflare.bindings` in `package.json`, where the
   Deploy button reads them.
4. Add a mock and extend `scripts/smoke.sh` and `scripts/e2e.sh` to cover
   its tools.

**On the computer.** A CLI that pins cli-common 0.15.0 or later and turns
on pk-cli-mcp's `http` feature has `<bin> mcp --http`. Nothing in this folder
changes: its owner follows the [home module steps](#tools-from-your-computer-home-modules)
with the CLI's own prefix.

## Developing

Needs Node 22.18 or later (it runs the TypeScript tests directly), `curl`,
`jq` and `openssl`; for the home module check, a Rust toolchain. No
Cloudflare account and no login: everything runs on `127.0.0.1`.

```sh
npm ci
npm run typecheck
npm test               # the owner gate, the pages' policy, home modules
npm run e2e            # dummy keys, mocks, `wrangler dev --local`, then the checks
```

`npm run e2e` walks the whole flow the way Claude does: discovery, dynamic
registration (a foreign callback refused), the passphrase page (a wrong
passphrase refused), PKCE token exchange, `initialize`, `tools/list`, every
tool against its mock (the Alpaca activity feed across two pages), the error
paths, refresh-token rotation and `/revoke`. It then restarts the Worker
under a new passphrase and checks that the earlier connection's tokens are
refused and the new passphrase connects, and restarts it three more times to
check that each module turns off with its secrets. Each run starts from
empty local storage.

With `E2E_HOME_BIN` set to a CLI that has `mcp --http`, it also runs a home
module: the CLI on 127.0.0.1:8790 with a fresh token, bound through
`mock/vpc-standin.ts` (a local Worker in place of the VPC binding, which
`wrangler dev` cannot run), a call through it, a write tool refused, then the
CLI stopped and its tools answering `offline:`. `make connector-check` in
cli-common passes example-cli. With a real CLI, keep it away from your
keychain: for m1f, set `E2E_HOME_BIN` to `$(which m1f)` and
`M1F_KEYCHAIN_SERVICE` to a service that holds nothing (such as
`piekstra.m1f-e2e`), then `npm run e2e`. Its tools then answer their own
`auth:` error, which the check accepts.

To try a module that is not released yet, point the build at a local file:
`CONNECTOR_MODULE_ALPACA=/path/to/alp.wasm npm run e2e` (the build log says
it is unverified). For a session by hand: `npm run keys`, `npm run mock`,
`npm run mock:alpaca`, then `npm run dev`; the MCP Inspector can connect to
`http://127.0.0.1:8787/mcp` with the passphrase from `.dev.vars`.

Measured in `wrangler dev --local` with `scripts/bench-workerd.sh`: workerd's
process CPU over 200 requests of each kind, in milliseconds per request.
The quiet column is one run on a mostly idle laptop. The loaded columns are
nine runs, made while other work held the same laptop's load average between
40 and 145 on 10 cores; that roughly doubles every figure.

| Request | Quiet | Loaded: median (range) | Loaded, over `tools/list` |
|---|---|---|---|
| `tools/list` (token check, KV read, MCP framing) | 3.2 | 7.5 (6.0 to 9.2) | |
| `coinbase_accounts`, two pages: two requests, each JWT signed by WebCrypto | 4.9 | 10.7 (8.6 to 12.4) | 3.2 (2.6 to 3.7) |
| `alpaca_portfolio`: account and positions, two requests | 4.4 | 9.9 (7.4 to 11.6) | 2.5 (0.7 to 3.4) |
| `alpaca_activities`, a month two pages into the feed: two requests | 4.1 | 10.8 (8.0 to 12.4) | 3.1 (1.3 to 4.9) |

The quiet run used a build of the Alpaca module from just before its v0.3.0
release; for these calls it runs the same code. In the same runs, an Alpaca
call costs no more than `coinbase_accounts`. Cloudflare counts CPU time
only, not time spent waiting on a fetch. A deployed Worker's own figure is
in the dashboard: **Workers & Pages →** the Worker **→ Metrics → CPU time**.

The upload (`wrangler deploy --dry-run`, both modules) is 1,724 KiB, 515 KiB gzipped, against the free plan's 3 MiB gzipped.
