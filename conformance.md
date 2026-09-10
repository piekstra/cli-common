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
- [ ] Secrets only via keychain (`piekstra.<bin>`), stdin, env — never argv
- [ ] One keychain item per credential set (a JSON blob via `get_json`/`set_json`; a legacy per-field layout is migrated on first read, then deleted)
- [ ] ISO `YYYY-MM-DD` accepted on all date flags; `--limit N` on lists
- [ ] Mutations prompt unless `--force`; exit 6 when non-interactive — decided **before** any keychain or network work (`pk_cli_core::confirm`)
- [ ] Mutations report success from a read-back (or the write's echo only when it carries the values), never from the write's status code

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
| ghome (google-home-cli) | conforms (v0.4.x, cli-common v0.7.0) — the reference for the confirmation gate, the resolve ladder, `emit_list`, the one-item keychain session (legacy two-item layout migrated on first read) and the read-back rail; consumer of `device-rooms/v1` |
| target-cli | planned — the credential-free template case (`auth status` with `required: false`) |
| babylist-cli | planned |
| govee-cli | migrating — spec-v1 PR open (against cli-common v0.7.0): output default flipped to text + `--json`, exit codes, keychain service → `piekstra.govee`; producer of `device-rooms/v1` |
| tplink-cloud-cli (`tplc`) | migrating — spec-v1 PR open (against cli-common v0.7.0): output default, exit codes, eight keychain items → one `session` item, service → `piekstra.tplc`; producer of `device-rooms/v1` |
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

Consumer: the `organize-scans` archiver — one `documents list --json` +
`documents download <id> -o` per CLI replaces its per-CLI download-command
adapter table. The migration is **deferred to the v0.5.0 release window, not
skipped** (PROFILES.md step 7): the CLIs pin `cli-common` by tag and adopt the
crate only after v0.5.0 tags, and `organize-scans` migrates after the CLIs
expose `documents`. Sequencing and status tracked in issue #8.

| CLI | smart-home/v1 (`device-rooms/v1`, documented — no crate) |
|---|---|
| ghome | **consumer** — `audit --expect -` joins on `id` (punctuation/case-insensitive) then `name` |
| govee | **producer** — `rooms devices` (ids are `<SKU>_<MAC>`; `cloud: false` for Bluetooth-only devices) |
| tplc | **producer** — `groups devices` |

Consumer: `ghome audit` — the vendor's own room placement checked against
Google Home's, replacing a hand-kept device→room table. Producers declare
`smart-home/v1` in `info.profiles` once their spec-v1 PRs land.

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
