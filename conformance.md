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
- [ ] ISO `YYYY-MM-DD` accepted on all date flags; `--limit N` on lists
- [ ] Mutations prompt unless `--force`; exit 6 when non-interactive

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
| target-cli | planned — the credential-free template case (`auth status` with `required: false`) |
| babylist-cli | planned |
| govee-cli, tplink-cloud-cli | pre-spec — inverted output default (JSON + `--table`), shifted exit codes, unprefixed keychain services |
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
