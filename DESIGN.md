# cli-common — shared surface & libraries for the piekstra CLI family

Status: draft v1 · 2026-07-11 · §1.8 domain profiles added in v1.1 · 2026-07-19 · confirm/read-back rails, one-item keychain rule, `smart-home/v1` (0.8.0) · 2026-09-10 · 1Password source + secret precedence (§1.7) · 2026-10-02 · `pk-cli-drive`, state in the owner's Drive (§2.1) · 2026-10-02

The family today: `fpl`, `tojfl`, `lrfl`, `xfin`, `gpm2op`, `target-cli`, `babylist-cli`
(and future account-portal CLIs). All Rust, all clap-derive, all keychain-secured,
all self-updating from GitHub releases — but each spells those things slightly
differently. This repo defines (1) a **surface specification** every CLI conforms
to, and (2) a set of **library crates** that implement the shared behavior so
conformance is mostly free.

Consumers like `utiman` currently need per-provider manifest hacks
(`self-update-args = ["update"]` vs `["self-update"]`, `authenticated-field =
"password_in_keychain"` vs `"authenticated"`). The goal is that a driver tool can
treat any conforming CLI uniformly, and eventually auto-derive its manifest.

---

## Part 1 — Surface specification (SPEC v1)

### 1.1 Global flags (every binary)

| Flag | Meaning |
|---|---|
| `--json` | Machine-readable JSON on stdout; diagnostics on stderr. Global, valid on **every** command. |
| `-v, --verbose` | Extra diagnostics on stderr. Never secrets. |
| `-q, --quiet` | Suppress non-error stderr output. |
| `--no-color` | Disable ANSI color. Also honor `NO_COLOR` env. |
| `-a, --account <ID>` | Where multi-account: account to act on. Env fallback `<PREFIX>_ACCOUNT`. |
| `--config <PATH>` | Override config file location. |

Env-var prefix = uppercased binary name (`FPL_`, `TOJFL_`, `LRFL_`, `XFIN_`).
Precedence everywhere: flag > env > config file > default.

### 1.2 Standard command set

Every CLI implements these with these exact spellings (aliases for old
spellings are kept one major version):

```
<bin> auth login        # acquire/store credential. --stdin | --from-env <VAR>,
                        #   --no-verify, --overwrite, --non-interactive. Secrets
                        #   NEVER via argv flags.
<bin> auth status       # canonical DTO (see 1.4). Works logged-out.
<bin> auth logout       # clear session; --forget also clears stored credential+config identity.
<bin> auth set-credential  # raw keychain write for rotation/headless (--stdin | --from-env, --overwrite).

<bin> config path|show|init         # non-secret settings
<bin> config set <key> <value>      # e.g. `config set account 1234567-0`
<bin> config unset <key>

<bin> self-update [--check] [-y|--yes]   # GitHub-release update; `--check` never installs.
<bin> completions <shell>
<bin> info                                # machine discovery, see 1.5
<bin> api <METHOD> <PATH> [--data JSON]   # raw passthrough, where an upstream API exists
```

Notes vs. today:
- `fpl update` → `fpl self-update` (keep `update` as hidden alias).
- `tojfl config set-password` / `lrfl login` → `auth login` (aliases kept).
- Credential-free CLIs (`lrfl` guest reads, `target-cli`) still implement
  `auth status` — it reports `method: "none"` / `authenticated: true`-equivalent
  semantics via `required: false`, so drivers don't special-case them.

### 1.3 Domain nouns (implement the ones that apply)

Noun-verb, plural nouns, `list|get|create` verbs, `ls` alias on every `list`:

```
accounts list|get [ID]|use <ID>|balance [ID]
bills list [--limit N]|latest|get <ID>
payments list|methods|create --amount X [--date D] [--method M] [--force]
usage get|list [--limit N]
transactions list [--limit N]        # ledger (fpl "history" → alias)
outages list                          # provider-specific extras are fine
```

Rules:
- Mutations (`payments create`, anything with side effects) prompt for
  confirmation unless `--force`; in `--json`/non-tty mode they **fail** with
  exit 6 instead of prompting. Whether a prompt is even possible is decided
  **before** any keychain or network work, so a driver that forgot `--force`
  never triggers a credential prompt or a request on its way to exit 6
  (`pk_cli_core::confirm`).
- A mutation reports success from a **read-back**, never from the write's
  status code: re-read the resource (or use the write's echo only when it
  carries the resulting values) before emitting the success DTO. Provider
  write responses are routinely empty, or ahead of their own read side.
- Dates accepted as ISO `YYYY-MM-DD` everywhere (provider formats are an
  internal concern). `--limit N` is the universal pagination knob.

### 1.4 Output contract

**Text mode (default):** key/value blocks for single resources, pipe-delimited
tables for lists (the existing fpl/xfin renderer becomes the shared one).
Stdout = data only; progress/confirmation/diagnostics = stderr.

**JSON mode (`--json`):**
- Success → the DTO alone on stdout (no envelope), pretty-printed.
- Failure → nonzero exit + `{"error": {"code": "<slug>", "message": "..."} }`
  on stdout, message repeated human-readably on stderr.
- DTO conventions: `snake_case` keys; ISO-8601 dates (`YYYY-MM-DD`, timestamps
  RFC 3339); money as `{"amount": "123.45", "currency": "USD"}` (string
  decimal — never floats); omit unknown fields rather than emitting null noise.
- Each top-level DTO carries `"schema": "<name>/v1"` so consumers can detect shape changes.
- **Field and column order is insertion order** — the order the code builds the
  DTO, not alphabetical. `output::table_view` chooses and orders table columns,
  `output::kv` renders fields in build order, and the workspace enables
  `serde_json`'s `preserve_order` so both hold for every consumer. Lead with the
  identifying fields. This binds rendered text and JSON key order only: JSON
  objects are unordered by definition, so it never affects a conforming
  consumer's ability to read a field.

**Canonical `auth status --json` (schema `auth-status/v1`):**
```json
{
  "schema": "auth-status/v1",
  "required": true,
  "authenticated": true,
  "method": "password | browser-session | none",
  "username": "user@example.com",
  "account": "12345-0",
  "credential_in_keychain": true,
  "session_valid": true,
  "expires_at": "2026-07-12T03:00:00Z"
}
```
(`username`/`account`/`expires_at` optional.) This retires utiman's
`authenticated-field` per-provider config.

**Canonical `self-update --check --json` (schema `self-update/v1`):**
```json
{ "schema": "self-update/v1", "current": "0.3.1", "latest": "0.4.0",
  "update_available": true, "release_url": "..." }
```

### 1.5 Exit codes

| Code | Meaning |
|---|---|
| 0 | success |
| 1 | generic / unexpected error |
| 2 | usage error (clap default) |
| 3 | auth required or credential invalid/expired |
| 4 | resource not found |
| 5 | upstream/provider error (portal down, scrape mismatch, rate limit) |
| 6 | confirmation required (mutation attempted non-interactively without `--force`) |

Drivers can branch on 3 ("run login flow") and 5 ("provider issue, retry later")
without parsing messages.

### 1.6 `info` — machine discovery (v1.1, enables manifest auto-generation)

```json
{
  "schema": "cli-info/v1",
  "name": "fpl", "version": "0.4.0",
  "spec": "piekstra-cli/1",
  "repo": "https://github.com/piekstra/fpl-cli",
  "auth": { "required": true, "method": "password", "login_hint": "fpl auth login" },
  "capabilities": ["accounts", "bills", "payments", "usage", "transactions", "outages", "api"]
}
```
`utiman` (and the new driver tool) can bootstrap a provider from `info` +
conventions alone; TOML manifests stay as the escape hatch for non-conforming CLIs.

### 1.7 Security & privacy invariants

- Secrets enter only via prompt, `--stdin`, `--from-env`, or `--op <REF>` —
  never argv. `--op` carries a 1Password secret reference
  (`op://vault/item/field`), which names where the secret is, not the secret.
- Secrets live only in the OS keychain, service name `piekstra.<bin>` (existing
  entries migrated on first run — `CredentialStore::migrate_from`), or in
  1Password, read through the `op` CLI. Neither path writes a secret to a
  file.
- **One keychain item per credential set.** A token, its refresh token and
  their metadata are one JSON item, not one item each: on macOS every item a
  freshly built binary reads is a permission prompt, so a four-item layout
  asks four times after every rebuild and reads as "flaky"
  (`pk_cli_secrets::CredentialStore::{get_json, set_json}`; a legacy
  per-field layout is migrated on first read, then deleted).
- `--verbose` never logs secrets, cookies, or full account numbers.

#### 1Password as a credential source

A CLI may declare a 1Password secret reference per secret, as a flag
(`auth login --op op://Example/Login/password`, `pk_cli_secrets::OpArgs`) or
as config (an `OpRef` field, e.g. `op_ref`). `pk_cli_secrets::OnePassword`
resolves it with `op read --no-newline [--account <A>] <REF>`:

- the value comes back on `op`'s stdout through a pipe into a `Secret`;
  nothing is written to disk, and errors quote `op`'s stderr, never its
  stdout;
- `op`'s stdin is closed, so it cannot wait on a terminal prompt;
- one call per read, killed at a timeout (60 s default), never retried and
  never polled. `op` can raise a Touch ID or app approval; a retry would
  raise another, and a loop would raise one per pass;
- a one-time password field resolves to its current code with
  `?attribute=otp` on the reference.

| `op` outcome | Exit |
|---|---|
| not signed in, session expired, approval dismissed, no answer within the timeout | 3, naming `op signin` |
| vault, item or field missing, or an empty value | 4, naming the reference |
| any other `op` failure | 5 |
| `op` not installed | 1 |

**Precedence.** At runtime a CLI resolves a secret through
`pk_cli_secrets::SecretResolver`, which walks the sources in a
`SourceOrder`. The default is **env, then keychain, then 1Password**:

1. **env** (`<PREFIX>_PASSWORD` or whatever the CLI declares) is set per
   invocation, by `op run --` or CI, so it is the most explicit choice. This
   matches the spec-wide flag > env > config order.
2. **keychain** is a local read that, once granted, never prompts.
3. **1Password** fills in when the keychain is empty. It is last because
   `op` can raise an approval on every session and depends on the app or a
   sign-in.

The walk skips a source the secret does not declare (no env var name, no
reference) and falls through a source that holds nothing (env var unset,
no keychain item). It **stops at a source that fails**, and an empty value
from any source counts as a failure: a dismissed approval or a signed-out `op` is exit 3, never a
silent fallback to a keychain copy the user did not pick or to a second
prompt. When every source is empty, the CLI prompts or exits 3 naming
`<bin> auth login`.

The order is configurable per CLI through a `secret_sources` config key
holding the text form (`op,keychain`, `env,op`; a source left out is never
read), parsed by `SourceOrder`. Usual precedence applies to the key itself:
`<PREFIX>_SECRET_SOURCES` overrides config. A user who keeps 1Password as
the source of truth sets `op,keychain` or `op`.
- Public repos: no internal-employer names, no real account numbers/addresses in
  fixtures, docs, or git history.

### 1.8 Domain profiles (v1.1)

Part 1 above is the **surface** layer: every family CLI implements it, whatever
its domain. A **domain profile** is an optional second layer: canonical command
spellings + shared DTOs for one domain, so a driver can consume any CLI in that
domain with zero per-provider configuration. Profiles are versioned
independently of the spec (`utility/v1`) and declared in `info`:

```json
{ "schema": "cli-info/v1", ..., "profiles": ["utility/v1"] }
```

Rules that apply to every profile:

- A profile owns **spellings and shapes**, never provider logic.
- Profile DTOs follow §1.4 (schema tags, `Money`, ISO dates, snake_case).
- Every profile `list` command emits the `Paged` envelope — records under
  `items`, optional `next_cursor`/`total` — and takes the shared range flags
  `--limit N`, `--since YYYY-MM-DD`, `--until YYYY-MM-DD` (`RangeArgs`).
  Both are profile-agnostic and live in `pk-cli-core` (`pk_cli_core::{Paged,
  RangeArgs}`); profile crates re-export them, so a CLI that adopts two
  profiles gets one type, not two, and a non-utility CLI never depends on the
  utility crate just to page a list.
- When a profile earns a crate, and how to add one: see **[PROFILES.md](PROFILES.md)**.

#### The `utility/v1` profile (crate `pk-cli-utility`)

For account-portal CLIs (`fpl`, `tojfl`, `lrfl`, `xfin`). Commands (implement
the ones the provider supports; spellings are canonical):

```
<bin> summary                       # utility-summary/v1 (balance + due date)
<bin> balance                       # same DTO as summary — second entry point
<bin> bills list|latest|get <ID>    # statement-list/v1 / statement/v1
<bin> bills download <ID> -o <PATH> # statement PDF, where available
<bin> payments list|methods         # payment-list/v1
<bin> payments create --amount X [--date D] [--method M] [--force]
<bin> pay quote|open                # hosted-page hand-off (no credential spend)
<bin> usage list                     # usage-period-list/v1
<bin> transactions list             # transaction-list/v1 (full ledger)
<bin> outages list                  # provider extras stay provider-shaped
```

DTOs: `UtilitySummary` (`utility-summary/v1`), `Statement`, `Payment`,
`UsagePeriod` (quantity + explicit unit — quantities are not money),
`Transaction`, `Paged<T>`. `payments create` is the only real-money mutation
and keeps §1.3's confirmation rules; `pay open` (portal hand-off) is the
driver-safe alternative and drivers (utiman) only ever invoke the latter.

With this profile, utiman's `[summary]`/`[[series]]` manifest sections
(`balance-fields`, `scale = "cents"`, `items-path`) collapse to defaults:
`summary --json` → `balance` + `due_date`, lists → `items`.

#### The `documents/v1` profile (crate `pk-cli-documents`)

For any portal that publishes **files** — statements, escrow analyses, tax
forms, notices, meeting minutes (`pmac`, `rpmfl`, `wabhoa`, `fpl`/`tojfl`/`lrfl`
for their bill PDFs). Orthogonal to `utility/v1`: a CLI may declare both. All
commands are reads — nothing here spends money — so §1.3's confirmation rules
do not apply.

```
<bin> documents list                       # document-list/v1 (newest first)
<bin> documents download <ID> -o <PATH>    # document-download/v1 (alias: get)
<bin> documents download --all -o <DIR>    # document-download-batch/v1
<bin> documents open <ID>                  # document-open/v1 (optional; system viewer)
```

`-o <PATH>` writes to a file (or `-` for stdout); `-o <DIR>`/`--all` writes a
directory; with neither, the portal's own filename in the current directory.
Old spellings stay as hidden aliases for one major version (`bills download`,
`statements`, `bill --save`).

Two invariants, mechanized in the crate's `verify` module: fetched bytes go
through `verify_download(bytes, declared_filetype)` **before** anything is
written or a byte count reported (`%PDF` magic for PDFs; for text filetypes,
rejection of the shapes an expired pre-signed link actually serves — an HTML
login page, an XML error, a JSON error object); and **every**
provider-controlled component of a filename — the portal's `file`, a type, a
filetype, a date, an id — goes through `fs_safe` before it joins a path, so a
crafted response can neither traverse out of the output directory nor fake
success with an error page.

DTOs: `Document` (`document/v1` — `id`, `name`, optional `date`/`category`/
`file`; **no** financial fields — a statement's amount belongs to `utility/v1`,
not the file), `SavedDocument` (`document-download/v1`), `DownloadBatch`
(`document-download-batch/v1`), `OpenedDocument` (`document-open/v1`), all over
`Paged<T>`. `DownloadBatch` also carries an optional `skipped:
[SkippedDocument]` (`{id, reason, optional code}`) so a `--all` run that can't
produce a file for a listed document reports it instead of silently coming up
short; `code` is a machine-branchable slug from an open set whose canonical
members are the `SKIP_NO_FILE` / `SKIP_VERIFY_FAILED` / `SKIP_UPSTREAM`
constants. Both `skipped` and `code` are additive within `/v1` (optional,
defaulted, omitted-when-empty — see the AGENTS.md carve-out).

The profile exists to collapse the `organize-scans` archiver's per-CLI
download-command table (`pmac documents download --all`, `fpl bills download
--date … -o`, `tojfl bills get <n> -o`, `lrfl bill --save`, …) to one call
shape — `<cli> documents list --json` then `<cli> documents download <id> -o
<path>` — once each CLI adopts it. That consumer migration lands across the
release window (the CLIs pin `cli-common` by tag, so they adopt after the
version tags), tracked in issue #8; `conformance.md` marks each CLI's status.

#### The `smart-home/v1` profile (documented; no crate yet)

For the vendor CLIs that front a smart-home cloud (`govee`, `tplc`, …) and the
assistant-side CLI that places their devices in rooms (`ghome`). The first —
so far only — shape is the one a cross-vendor consumer already pays for:

**`device-rooms/v1`** — every device a vendor knows, with the room the
vendor's own app files it under.

```json
{
  "schema": "device-rooms/v1",
  "items": [
    { "id": "H6076_AA11BB22", "name": "Office Lamp", "room": "Office",
      "source": "govee", "cloud": true, "connectivity": "wifi" }
  ]
}
```

| Field | | |
|---|---|---|
| `id` | required | the vendor's own device id — the join key against the assistant's partner device id |
| `name` | required | display name — the fallback join key |
| `room` | optional | room/group name in the vendor's app; **omitted** (never null) when the app files the device in no room, so a consumer reports the vendor side as unfiled instead of never hearing of the device |
| `source` | required | which vendor produced the row (`govee`, `tplink`, …); free text |
| `cloud` | optional | `false` when the vendor cannot expose the device to a cloud/assistant (Bluetooth-only), so a consumer expects no match instead of reporting one missing |
| `connectivity` | optional | `wifi` \| `bluetooth` |

Join rule for consumers: on `id` first, compared stripped of punctuation and
case (vendors render the same MAC as `AA:BB` in one place and `aabb` in
another), then on `name`. Producers: `govee rooms devices`, `tplc rooms
devices` (Tapo), `tplc groups devices` (Kasa). Consumer: `ghome audit --expect -`,
which reports a row without `room` as `unfiled`.

The profile is documented here rather than shipped as a crate: the shape is
one DTO that only ever crosses a process boundary as JSON, so no Rust type is
imported anywhere and a `pk-cli-smart-home` crate would be the
"DTOs nobody consumes" failure PROFILES.md guards against. It earns the crate
when a second shape lands or a Rust consumer needs the type (PROFILES.md,
"Documented-only profiles"). Adopters track in `conformance.md`.

---

## Part 2 — The `cli-common` workspace

Public repo `piekstra/cli-common`. Cargo workspace, dual-licensed MIT/Apache-2.0,
AGENTS.md, same house style as the CLIs.

### Crates

| Crate | Contents | Replaces (today) |
|---|---|---|
| `pk-cli-core` | `GlobalArgs` clap flatten struct; `ExitCode` enum per 1.5; error type with `code` slugs; output renderer (key/value blocks, pipe tables, JSON emit incl. error shape, `emit_list`/`emit_one` for plain lists and single resources); date/money types (`Money` and its grouped `$1,234.56` text display, ISO parsing helpers, UTC and local-timezone `today`); the §1.3 confirmation gate (`confirm`); reference resolution (`resolve::pick` — exact name, exact id, case-insensitive name, unique partial; ties are exit 4 naming the candidates) | fpl/xfin `output.rs`+`dates.rs`+`error.rs`, lrfl `formatter.rs`, tojfl `output.rs`; ghome/govee/lofty `confirm`, ghome/govee resolve ladders |
| `pk-cli-secrets` | keychain read/write/delete under `piekstra.<bin>`; typed one-item JSON credentials (`get_json`/`set_json`) and legacy-service migration (`migrate_from`); secret ingestion (`--stdin`/`--from-env`/`--op` args + logic); 1Password reads through `op` (`OpRef`, `OnePassword`); runtime source precedence (`SecretResolver`, `SourceOrder`); `auth set-credential` command impl | fpl/xfin `secrets.rs`, lrfl `auth/secrets.rs`; ghome `session.rs`/tplc `keychain.rs` item consolidation |
| `pk-cli-config` | `~/.config/<bin>/config.toml` load/save, typed get/set, `config` subcommand impl, `--config` override | four `config.rs` variants |
| `pk-cli-selfupdate` | GitHub-release check + in-place replace, `--check`/`-y`/`--json`, `self-update/v1` DTO, release-asset naming convention | ~580 duplicated lines across 4 repos |
| `pk-cli-auth` | `AuthCmd` clap enum + driver trait: CLI supplies `verify()`/`login()`, crate supplies status DTO (`auth-status/v1`), logout, prompting rules; `otp`, the one-time-code login (request, park for `--code`, mailbox read or prompt) | four auth command modules; the per-CLI park-and-resume code loops |
| `pk-cli-http` | reqwest client builder (UA, cookie store, timeouts, retry-with-backoff), `api` passthrough command impl, error→exit-code-5 mapping | per-CLI `client.rs` boilerplate (session logic stays per-CLI) |
| `pk-cli-core` (list) | profile-agnostic list primitives — `Paged<T>` envelope + `RangeArgs` (`--limit`/`--since`/`--until`) — shared by every domain profile | duplicated paging/range structs, and non-utility CLIs depending on `pk-cli-utility` just to page |
| `pk-cli-utility` | the `utility/v1` domain profile (§1.8): `UtilitySummary`, `Statement`, `Payment`, `UsagePeriod`, `Transaction` (re-exports core's `Paged`/`RangeArgs`) | utiman's per-provider `balance-fields`/`scale`/`items-path` manifest hacks |
| `pk-cli-documents` | the `documents/v1` domain profile (§1.8): `Document`, `SavedDocument`, `DownloadBatch`, `OpenedDocument` — list & download a portal's published files | `organize-scans`' per-CLI download-command adapter table |
| `pk-cli-scrape` | dependency-free HTML scanning for providers that answer in rendered pages: elements, attributes, table rows/cells, entity decoding — all total, never panicking | a DOM-parser dependency, and the ad-hoc `str::find` scraping each portal CLI grows on its own |
| `pk-cli-drive` | state in the owner's Drive (§2.1): one `Backend` over a mounted folder or an rclone remote, the stale-while-revalidate read cache in front of the remote (`CachedRemote`), its bounded background refresh, and owner-only file output (`private_file`) | tax-cli's `drive.rs` + `cache.rs` + `private_file.rs`, and the copy every other CLI that keeps state in Drive would write |

Each crate is small and independent; a CLI adopts them piecemeal. Provider
scraping/session logic (tojfl's DNN dance, xfin's browser-session replay) stays
in each CLI/SDK — cli-common owns *surface*, not *providers*.

### 2.1 State in the owner's Drive (`pk-cli-drive`)

For a CLI whose own state (a registry, notes, settings shared across
machines) lives in a cloud-drive folder the owner already has, rather than
in `~/.config`. The crate owns the mechanism; the CLI owns its layout (which
files, which folders) and its data contract.

**Two ways at one folder.** `Backend::Mount(path)` is a local directory — a
Drive-for-desktop mount or any folder. It is fast but can go stale without
saying so (a plain directory left where a mount used to be accepts writes and
syncs nothing; a lazily-materialized mount answers "no such file" for folders
the remote has), so a CLI certifies a mount before trusting it.
`Remote` is rclone (`cat`/`copyto`/`moveto`/`lsf`/`mkdir`) against a spec
`<remote>:<folder>`: slower, but it talks to the provider's API and is immune
to mount health. When both are configured, the remote is used for every
state operation. rclone's exit codes 3 and 4 read as "missing" (a missing file
is `None`, a missing folder lists empty); every other failure is exit 5,
never an empty result. A move to a computed name uses `move_no_clobber`,
which refuses an occupied destination with exit 2 and both files untouched.

**The read cache** (`CachedRemote`, the default in front of a remote) is a
per-machine accelerator, never a second source of truth:

| Rule | |
|---|---|
| Write-through | every write hits the remote first; only on success is the cache updated (content stored, listings dropped). A `--no-cache` run bypasses reads only; its writes still update the cache |
| Freshness tiers | a copy within the TTL (900 s) is served; past it but within the staleness bound (24 h) it is served at once with a `note:` on stderr and refreshed in the background; past the bound, or with nothing cached, the read fetches before the command continues |
| A write is never undone by a refresh | each write bumps a generation counter under a lock; a fetch stores its result only if the generation is unchanged since before it read |
| A write never derives from an unconfirmed stale copy | before a write, every copy the command was served stale is re-read; if one changed, the write is refused, nothing written, the cache now current |
| Fail-open | a cache miss or disk error falls through to the remote; an unreachable remote with a cached copy serves the copy with a stderr warning |
| Bounded background work | the refresh is a detached child (`<bin> <command> --revalidate-file=<rel> --remote-spec=<spec>`, null stdio, own process group) whose rclone calls are killed at the revalidate timeout (120 s); a per-entry marker stops duplicates |

Location `$XDG_CACHE_HOME/<bin>/<slug>-<hash>/` (else `~/.cache/<bin>/…`),
directory `0700`, files `0600`. The knobs are one `CachePolicy`, resolved
once and passed to the cache, to `background_revalidator` and to the
child's `run_revalidation`. `CachePolicy::from_env(bin)` reads environment
variables with the binary's prefix: `<BIN>_NO_CACHE`, `<BIN>_CACHE_TTL`
(`0`: every read fetches), `<BIN>_CACHE_MAX_STALE` (`0`: no stale serving),
`<BIN>_CACHE_REVALIDATE_TIMEOUT`, `<BIN>_CACHE_NO_REVALIDATE`. A `--no-cache`
flag sets `bypass_reads` on the policy; nothing is exported to the
environment.

A CLI that uses the cache:

- accepts the hidden revalidate arguments on one command
  (`cache::REVALIDATE_FILE_ARG`, `REVALIDATE_LISTING_ARG`, `REMOTE_SPEC_ARG`)
  and calls `cache::run_revalidation` with a `Remote`, cache dir and policy
  built by the same code that built the parent's, so the child runs the same
  rclone under the same bound;
- builds one `CachedRemote` per command (a refused write keeps refusing
  through the same handle; the remedy is a rerun);
- documents the stale-write refusal as an exit-1 case: the remote answered,
  so it is not exit 5, and rerunning at once is safe.
  `cache::is_stale_write_refusal` recognizes it;
- passes store-relative `rel` paths it built itself. The mount backend
  joins them onto the root as given, so text from outside (a user-typed
  name, a provider's filename) is sanitized before it becomes a `rel`.

The crate needs Rust 1.89 (`File::lock`), above the workspace's 1.75 floor,
so adopting it raises the CLI's own minimum. `example-cli state
get|put|list|sync` is the worked example.

### Versioning & consumption

- Single workspace version, semver, tags `v0.x.y`, CHANGELOG per release.
- **Phase 1:** consume as git dependencies pinned to a tag:
  `pk-cli-core = { git = "https://github.com/piekstra/cli-common", tag = "v0.1.0" }`
  — works with the existing `cargo install --git` distribution, no crates.io
  commitment while surfaces are in flux.
- **Phase 2 (once stable):** publish to crates.io under the `pk-cli-*` prefix
  (names are free to bikeshed before first publish; the prefix just needs to be
  unique on crates.io).
- Pre-1.0: breaking changes allowed, called out in CHANGELOG. 1.0 when SPEC v1
  is frozen and three CLIs conform.
- A `conformance.md` checklist in this repo; each CLI's README states
  `Conforms to piekstra-cli spec v1`.

### Testing

- `trycmd`/snapshot tests inside cli-common for the renderer and DTO shapes.
- A tiny `example-cli` binary crate in the workspace exercising every crate —
  doubles as the template for new CLIs (next one starts by copying it).

---

## Part 3 — Migration plan (per CLI, in order of payoff)

1. **cli-common v0.1**: extract `pk-cli-core` + `pk-cli-selfupdate` +
   `pk-cli-secrets` from fpl/xfin (they're near-identical already — xfin was
   forked from fpl).
2. **xfin, fpl**: adopt v0.1. fpl: rename `update`→`self-update` (alias), add
   global `--json` to reads (its biggest gap), `completions`, exit codes.
3. **lrfl**: adopt; alias `login`/`logout`/`whoami` → `auth *`; `config
   set-account` → `config set account`; unify `history` flags with `--limit`.
4. **tojfl**: adopt; move `config set-password`→`auth login --save` path; add
   `-q`/`--no-color`; `self-update --json`; exit codes.
5. **utiman**: add a "conforming CLI" fast path (auth-status/v1, self-update/v1,
   later `info`) and shrink the catalog manifests to `id`+`binary`+domain bits.
6. **babylist-cli / target-cli**: adopt as they mature (target-cli is the
   credential-free case; babylist the template consumer for new CLIs).

Non-goals for v1: plugin systems, config schemas beyond flat keys, i18n,
Windows keychain parity beyond what the `keyring` crate already gives.
