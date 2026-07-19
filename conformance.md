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
| xfin | conforms (v0.2.0) — `set-credential` also kept top-level. Drift: pinned cli-common v0.1.2; **no `config` subcommand**; second auth surface `payments login|logout`; no `--limit` |
| fpl | conforms (v0.2.0) — `init` kept alongside `auth login`. Drift: **no `config` subcommand**; three date formats in one binary (`usage hourly` is `MM-DD-YYYY`); no `--limit` |
| lrfl | conforms (v0.2.0) — `config set-account` spelling retained; guest reads need no auth. Drift: hand-rolled `self-update` supports only `--check` (no `--yes`/`--json` — breaks driver probes); hidden legacy `login`/`logout`/`whoami` |
| tojfl | conforms (v0.1.x) — SDK keychain service name unchanged; `config set/unset` pending. Drift: skips pk-cli-secrets/pk-cli-config; no `auth set-credential`; no `api` |
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
| fpl, tojfl, lrfl, xfin | pending — adopt `pk-cli-utility` DTOs + canonical spellings, then declare via `info.profiles` |
