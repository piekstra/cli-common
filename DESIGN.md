# cli-common — shared surface & libraries for the piekstra CLI family

Status: draft v1 · 2026-07-11

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
| `pk-cli-dto` (later) | shared cross-provider DTOs: `Balance`, `Statement`, `Payment`, `UsagePeriod`, `Transaction`, `Paged<T>` — only once ≥2 CLIs want the same shape | — |

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
