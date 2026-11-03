# SPEC v1 conformance checklist

A CLI "conforms to piekstra-cli/1" when every box is checked. Track family
status here.

## Checklist

- [ ] Global `--json` valid on every command; DTO-only stdout; error DTO + exit code on failure
- [ ] Global `-v/--verbose`, `-q/--quiet`, `--no-color`
- [ ] Exit codes 0–6 per SPEC §1.5
- [ ] `auth login` (`--stdin`/`--from-env`/`--no-verify`/`--overwrite`/`--non-interactive`), `auth status` (auth-status/v1), `auth logout [--forget]`, `auth set-credential`
- [ ] `config path|show|set|unset`
- [ ] `self-update [--check] [-y]` with self-update/v1 DTO
- [ ] `completions <shell>`
- [ ] `info` emitting cli-info/v1
- [ ] Secrets only via keychain (`piekstra.<bin>`), stdin, env, or 1Password through `op` (`--op <REF>`) — never argv
- [ ] One keychain item per credential set (a JSON blob via `get_json`/`set_json`; a legacy per-field layout is migrated on first read, then deleted)
- [ ] ISO `YYYY-MM-DD` accepted on all date flags; `--limit N` on lists
- [ ] Mutations prompt unless `--force`; exit 6 when non-interactive — decided **before** any keychain or network work (`pk_cli_core::confirm`)
- [ ] Mutations report success from a read-back (or the write's echo only when it carries the values), never from the write's status code
- [ ] A portal the CLI cannot sign in to by itself (bot check, OAuth single-page app) offers `auth login --browser` through `pk-cli-browser` (DESIGN.md §2.2), reports `method: "browser-session"`, and verifies the captured session with one provider read: a refusal stores nothing, and an inconclusive read follows §2.2's table, so a capture that can only be this sign-in's stores with a warning and `verified: false`, and one that could predate it answers `Confirm::Undecided` and keeps waiting, unless §2.2's exception for an always-challenging edge applies. An unverified session never replaces a working stored one without `--overwrite`
- [ ] A `--browser` CLI whose provider keeps tokens in site storage (a rotating refresh token in `localStorage`): `Launch::clear_site_storage(true)`, so the kept profile keeps only its cookies (DESIGN.md §2.2)
- [ ] `make install` uses `--locked`, so the installed binary is built from the reviewed Cargo.lock
- [ ] A provider that rotates refresh tokens: every renewal goes through `pk_cli_auth::refresh_lock::RefreshLock` (cli-common v0.18.0 or later), re-reading the session under the lock and posting no grant if another process renewed. A sign-in calls `before_signin` before any work and stores through `store_signin`; a logout deletes through `forget_for_binary`; and `SECURITY.md` says which copies must be on a release with the lock (DESIGN.md §1.7)

### MCP profile (DESIGN.md §1.9), for a CLI that serves tools

- [ ] `<bin> mcp` through `pk-cli-mcp` (`McpArgs` + `run`), two to four workflow-shaped tools over the CLI's own command logic and DTOs
- [ ] Tool names `<service>_<what>`, one prefix, at most 64 characters of `[a-z0-9_]`; arguments take names as well as ids
- [ ] Every tool has a `title`, all four hints set explicitly (`readOnlyHint`, `destructiveHint`, `idempotentHint`, `openWorldHint`), an `outputSchema` generated for serialization, and a description of at least three sentences naming the returned fields; arguments are one flat struct, every one described
- [ ] Every write is its own tool, never combined with a read, never moves money, and carries `_meta` `anthropic/requiresUserInteraction: true`
- [ ] Results are `structuredContent` plus the same JSON as compact text; lists use `ListArgs`/`PageRequest`/`Page` (limit 25 default, 200 max, filter-bound cursors, `response_format` concise/detailed, the 32,000-character budget with `truncated` + `hint`)
- [ ] Errors keep their exit code's meaning and say how to fix the call; an auth error names `<bin> auth login`, and no tool call signs in, prompts, or creates a keychain item
- [ ] `tools/list` touches no credential; the credential is also read from named environment variables (env before keychain), so an `.mcpb` can pass it
- [ ] Tests call `pk_cli_mcp::conformance::assert_conforms` and run each tool through `assert_fits` on scrubbed fixtures, offline; all definitions together stay under 8,000 characters
- [ ] Releases attach `<bin>-<version>.mcpb` (binary server type; macOS universal, Linux and Windows x86_64; credential as `sensitive` `user_config`), and a `claude-plugins` plugin's `.mcp.json` runs `<bin> mcp`

For path B (the deploy-your-own Worker), an official-API CLI also:

- [ ] Writes its provider client once against `pk_cli_core::transport::Transport` (and `PlatformSigner` if it signs ES256) (`pk_cli_http::BlockingTransport` natively, `pk_cli_mcp::wasm::HostTransport` in the Worker), behind a default `native` feature that gates the keychain, clap, self-update and blocking HTTP, and a `wasm` feature with `pk_cli_mcp::export_wasm!`
- [ ] Takes randomness and time from `pk_cli_mcp::wasm::{fill_random, now_ms}` on wasm32, so no dependency pulls in `wasm-bindgen`; the module imports only `pk_host`
- [ ] Builds clean with clippy for `wasm32-unknown-unknown` in CI, and tests its Worker toolset natively through `pk_cli_mcp::wasm::testing`
- [ ] Releases attach `<bin>-mcp-<version>.wasm` and its `.sha256`
- [ ] Is served by `connector/`: its asset pinned in `modules.json`, its module with the secrets and vars it reads (`config`) and the secrets that turn it on (`secrets`) in `src/modules.ts`, those secrets in `.dev.vars.example` (value `none`, so the module stays optional) and `package.json`'s `cloudflare.bindings`, and its tools in a mock, `scripts/smoke.sh` and `scripts/e2e.sh`

For path B through the user's own computer (a "home" module), a portal CLI
instead:

- [ ] Pins cli-common v0.15.0 or later with pk-cli-mcp's `http` feature, so `<bin> mcp --http --port <PORT>` serves its read-only tools on 127.0.0.1 behind a bearer token (`<bin> mcp --new-token`), and its README's web and mobile section points at `connector/`'s home-module steps
- [ ] Names its tools with one prefix (`m1_…`), which becomes the module's name in the Worker (`HOME_M1_TOKEN`, binding `HOME_M1`)
- [ ] Its `self-update` calls `Updater::run_noting` with `pk_cli_mcp::restart_note` (cli-common v0.17.0 or later; v0.18.0 adds the `.mcpb` line), since a running server keeps the old code, and its README's web and mobile section says to restart the server after an update
- [ ] Its `SECURITY.md` lists the bearer token (keychain item or `<BIN>_MCP_TOKEN`, with a copy in the Worker), says tool results cross Cloudflare and reach Claude, links `connector/`'s threat model, and under revoking names `--new-token --overwrite` plus a restart, and stopping the server and tunnel, neither of which `auth logout` does

## Family status

Drift notes are from the 2026-07-19 family audit.

| CLI | Status |
|---|---|
| example-cli | reference implementation (incl. `utility/v1` profile demo) |
| lofty | conforms (cli-common v0.1.2) — `properties list` uses domain paging (`--page`/`--per-page`) |
| discord | conforms (cli-common v0.1.2) — domain nouns flat-plural, no `list|get` verbs |
| xfin | conforms (v0.7.0, cli-common v0.2.0) — gained `config path|show|set|unset`, `summary`/`balance` entry points, `--limit`/range flags on statements. Remaining: `set-credential` also top-level; second auth surface `payments login|logout` |
| fpl | conforms (v0.3.0, cli-common v0.2.0) — gained `config path|show|set|unset` and range flags on lists. Remaining: `init` kept alongside `auth login`; `usage hourly` still `MM-DD-YYYY` |
| lrfl | conforms (v0.6.0 pending — profile PR open) — shared `self-update` (fixes the `--check`-only probe break). Remaining: `config set-account` spelling; hidden legacy `login`/`logout`/`whoami` |
| tojfl | conforms (v0.3.0, cli-common v0.2.0). Remaining: SDK keychain service name unchanged; skips pk-cli-secrets/pk-cli-config; no `auth set-credential`; no `api` |
| gpm2op | conforms (v0.2.0) — no keychain (delegates to `op`); no `config`/`auth` commands (nothing to store) |
| ghome (google-home-cli) | conforms (v0.4.x, cli-common v0.7.0) — the reference for the confirmation gate, the resolve ladder, `emit_list`, the one-item keychain session (legacy two-item layout migrated on first read) and the read-back rail; consumer of `device-rooms/v1`; reports a row without `room` as `unfiled` (0.5.0) |
| target-cli | planned — the credential-free template case (`auth status` with `required: false`) |
| babylist-cli | planned |
| govee-cli (`govee`) | conforms (v0.2.0, cli-common v0.8.0) — text default + `--json`, family exit codes, `auth`/`config`/`self-update`/`info`/`api`, keychain `piekstra.govee` (migrated from `govee-cli`); `rooms devices` emits `device-rooms/v1` (0.2.1, govee-cli #21: a device the app files in no room keeps its row with `room` omitted) |
| tplink-cloud-cli (`tplc`) | conforms (v0.2.0, cli-common v0.8.0) — as above, keychain `piekstra.tplc`; `api` is a cloud-RPC passthrough by method name (documented); `groups devices`/`rooms devices` emit `device-rooms/v1` (0.2.1, tplink-cloud-cli #4: a Tapo device's `id` is the MAC without separators, what Google's `partner_device_id` shows, so rows join; one of Tapo's own devices in no room keeps its row with `room` omitted) |
| insperity-cli (`insp`) | conforms (v0.1.0, cli-common v0.8.0; one-item keychain landed in [insperity-cli #7](https://github.com/piekstra/insperity-cli/pull/7)) — read-only (`api` refuses PUT/PATCH/DELETE with exit 6, so the confirmation lines are vacuous by design); `documents/v1` producer; keychain `piekstra.insp` / `credentials` holds password, session jar, device-signal blob and the parked MFA login as one JSON item (the legacy four-item layout is folded on first read, then deleted); `paystubs list` / `documents list` refuse (exit 5) rather than dress the single current paycheck as a list the portal never publishes |
| slack-rs (`slck`) | pre-spec — **security: token accepted on argv**; fix ingestion before adoption |
| alpaca-rs (`alpaca`) | pre-spec — env-only auth (acceptable; report `method: "env"`), JSON-always, no `--version` |
| pup, twapp | pre-spec — adopt selectively (exit codes, `info`, self-update); surfaces stay their own |

## Profiles (SPEC §1.8)

| CLI | utility/v1 |
|---|---|
| example-cli | demo |
| tojfl | **adopted** (v0.3.0) — summary/balance → utility-summary/v1, bills/usage/transactions → Paged envelopes |
| xfin | **adopted** (v0.7.0) — new summary/balance entry points, statements → Paged |
| fpl | **adopted** (v0.3.0) — summary/accounts balance → utility-summary/v1, bills/payments/history → Paged |
| lrfl | adopted, PR open (v0.6.0) — summary/balance + history → payment-list/v1 |

Consumer: utiman parses the profile shapes from their schema tags with zero
manifest field config (fast path + label-field chains, utiman #22/#26).

| CLI | documents/v1 |
|---|---|
| example-cli | demo (`documents list`) |
| pmac | reference shape — already emits `document-list/v1` / `document-download*/v1` (pre-crate); migrate to `pk-cli-documents` v0.5.0, keep `statements` alias |
| wabhoa | planned — has `statements list` (metadata); gains `documents list` + `download` once a live capture confirms the PDF endpoint |
| fpl, tojfl, lrfl | planned — fold `bills download`/`bill --save` into `documents download` (old spellings kept as aliases) |
| rpmfl | planned — `documents`/`forms` → profile shapes |
| insp | ships — `documents download` (alias `get`) serves the paystub PDF for a pay date as `document-download/v1`; `documents list` refuses (exit 5) because the portal publishes no document index, so no list shape is dressed over a single-document read |

Consumer: the `organize-scans` archiver — one `documents list --json` +
`documents download <id> -o` per CLI replaces its per-CLI download-command
adapter table. The migration is **deferred to the v0.5.0 release window, not
skipped** (PROFILES.md step 7): the CLIs pin `cli-common` by tag and adopt the
crate only after v0.5.0 tags, and `organize-scans` migrates after the CLIs
expose `documents`. Sequencing and status tracked in issue #8.

| CLI | smart-home/v1 (`device-rooms/v1`, documented — no crate) |
|---|---|
| ghome | **consumer** — `audit --expect -` joins on `id` (punctuation/case-insensitive) then `name`; reports a row without `room` as `unfiled` (0.5.0) |
| govee | **producer** — `rooms devices` (ids are `<SKU>_<MAC>`; `cloud: false` for Bluetooth-only devices; `room` omitted for a device in no room, 0.2.1) |
| tplc | **producer** — `groups devices` (Kasa groups), `rooms devices` (Tapo rooms: ids are the MAC without separators, `room` omitted for one of Tapo's own devices in no room; 0.2.1) |

Consumer: `ghome audit` — the vendor's own room placement checked against
Google Home's, replacing a hand-kept device→room table. Producers declare
`smart-home/v1` in `info.profiles` once their spec-v1 PRs land.

## Browser sign-in (DESIGN.md §2.2)

| CLI | `auth login --browser` |
|---|---|
| example-cli | demo of the flag check and the capture (`auth login --browser`) |
| sfi | embedded copy (cookies, plus a request-header watch) — migrate to `pk-cli-browser` once the crate offers a narrow way to observe that request (no raw DevTools handle is exported) |
| wfr | embedded copy (cookies, validated with a read) — migrate; its session cookie is also set for anonymous visitors, so it needs a signed-in-only readiness signal |
| carta | embedded copy (cookies, then endpoint recording) — migrate once the crate offers endpoint recording |
| m1f, rhd, nlnt | planned — today a `--stdin` import of a value the owner copies out of DevTools |

## MCP profile (DESIGN.md §1.9)

| CLI | Tools | Path A packaging | Path B module |
|---|---|---|---|
| example-cli | demo: `example_devices` (paged read), `example_device_rename` (write) | `mcp` subcommand and a stdio end-to-end test; not released | none |
| pk-cli-mcp-wasm-demo | ABI test tools | none | built and run under the reference host in CI; not released |
| coinbase-cli (`cbase`) | pilot: `coinbase_accounts`, `coinbase_transactions` | `.mcpb` on each release from v0.2.0; `coinbase-cli` plugin in `claude-plugins` | `cbase-mcp-<version>.wasm` on each release from v0.3.0; served by `connector/` |
| alpaca-cli (`alp`) | `alpaca_portfolio`, `alpaca_activities` | `.mcpb` on each release from v0.2.0 | `alp-mcp-<version>.wasm` on each release from v0.3.0; served by `connector/` |
| m1-cli (`m1f`) | `m1_portfolio`, `m1_activity`, `m1_documents`, `m1_realized_gains` | `.mcpb` on each release from v0.4.0 | home module: `m1f mcp --http` from v0.5.0 |
| robinhood-cli (`rhd`) | `robinhood_portfolio`, `robinhood_activity`, `robinhood_documents` | `.mcpb` on each release from v0.3.0 | home module: `rhd mcp --http` from v0.4.0 |

Consumers: Claude Code, Claude Desktop, and (path B) a deploy-your-own
Cloudflare Worker reached from claude.ai, which runs a portal CLI's tools on
the user's computer through their own tunnel. The `cli-mcp`
server's generic one-passthrough-tool-per-CLI registration stays for CLIs
without a profile toolset.

## CI

The family gate (matching `google-home-cli`'s workflows, the template for
the rest; this repo's own workflow adopts it in #15): `fmt` / `clippy -D warnings` / `test` / an offline smoke
(`--version`, `--help`, `info`) on **ubuntu-latest**, plus a **targeted macOS
smoke** — build and run the same three commands — rather than a full macOS
matrix, because the only platform-specific sliver is the apple-native keyring
backend and macOS minutes bill at roughly ten times Linux. A separate
**security** job runs `cargo-audit` (rustsec/audit-check) and `gitleaks`.
Tests are offline and never read the OS keychain (an ad-hoc-signed test
binary would prompt once per item per run).
