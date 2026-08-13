# cli-common — shared surface & libraries for the piekstra CLI family

Status: draft v1 · 2026-07-11 · §1.8 domain profiles added in v1.1 · 2026-07-19

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
  exit 6 instead of prompting.
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

- Secrets enter only via prompt, `--stdin`, or `--from-env` — never argv.
- Secrets live only in the OS keychain, service name `piekstra.<bin>` (existing
  entries migrated on first run).
- `--verbose` never logs secrets, cookies, or full account numbers.
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
`Paged<T>`.

The profile exists to collapse the `organize-scans` archiver's per-CLI
download-command table (`pmac documents download --all`, `fpl bills download
--date … -o`, `tojfl bills get <n> -o`, `lrfl bill --save`, …) to one call
shape — `<cli> documents list --json` then `<cli> documents download <id> -o
<path>` — once each CLI adopts it. That consumer migration lands across the
release window (the CLIs pin `cli-common` by tag, so they adopt after the
version tags), tracked in issue #8; `conformance.md` marks each CLI's status.

---

## Part 2 — The `cli-common` workspace

Public repo `piekstra/cli-common`. Cargo workspace, dual-licensed MIT/Apache-2.0,
AGENTS.md, same house style as the CLIs.

### Crates

| Crate | Contents | Replaces (today) |
|---|---|---|
| `pk-cli-core` | `GlobalArgs` clap flatten struct; `ExitCode` enum per 1.5; error type with `code` slugs; output renderer (key/value blocks, pipe tables, JSON emit incl. error shape); date/money types (`Money`, ISO parsing helpers) | fpl/xfin `output.rs`+`dates.rs`+`error.rs`, lrfl `formatter.rs`, tojfl `output.rs` |
| `pk-cli-secrets` | keychain read/write/delete under `piekstra.<bin>`; secret ingestion (`--stdin`/`--from-env` args + logic); `auth set-credential` command impl | fpl/xfin `secrets.rs`, lrfl `auth/secrets.rs` |
| `pk-cli-config` | `~/.config/<bin>/config.toml` load/save, typed get/set, `config` subcommand impl, `--config` override | four `config.rs` variants |
| `pk-cli-selfupdate` | GitHub-release check + in-place replace, `--check`/`-y`/`--json`, `self-update/v1` DTO, release-asset naming convention | ~580 duplicated lines across 4 repos |
| `pk-cli-auth` | `AuthCmd` clap enum + driver trait: CLI supplies `verify()`/`login()`, crate supplies status DTO (`auth-status/v1`), logout, prompting rules | four auth command modules |
| `pk-cli-http` | reqwest client builder (UA, cookie store, timeouts, retry-with-backoff), `api` passthrough command impl, error→exit-code-5 mapping | per-CLI `client.rs` boilerplate (session logic stays per-CLI) |
| `pk-cli-core` (list) | profile-agnostic list primitives — `Paged<T>` envelope + `RangeArgs` (`--limit`/`--since`/`--until`) — shared by every domain profile | duplicated paging/range structs, and non-utility CLIs depending on `pk-cli-utility` just to page |
| `pk-cli-utility` | the `utility/v1` domain profile (§1.8): `UtilitySummary`, `Statement`, `Payment`, `UsagePeriod`, `Transaction` (re-exports core's `Paged`/`RangeArgs`) | utiman's per-provider `balance-fields`/`scale`/`items-path` manifest hacks |
| `pk-cli-documents` | the `documents/v1` domain profile (§1.8): `Document`, `SavedDocument`, `DownloadBatch`, `OpenedDocument` — list & download a portal's published files | `organize-scans`' per-CLI download-command adapter table |
| `pk-cli-scrape` | dependency-free HTML scanning for providers that answer in rendered pages: elements, attributes, table rows/cells, entity decoding — all total, never panicking | a DOM-parser dependency, and the ad-hoc `str::find` scraping each portal CLI grows on its own |

Each crate is small and independent; a CLI adopts them piecemeal. Provider
scraping/session logic (tojfl's DNN dance, xfin's browser-session replay) stays
in each CLI/SDK — cli-common owns *surface*, not *providers*.

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
