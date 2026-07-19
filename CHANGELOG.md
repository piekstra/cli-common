# Changelog

## v0.2.0 — unreleased

Domain profiles (SPEC v1.1 §1.8): an optional second layer over the surface
spec — canonical spellings + shared DTOs per domain, declared via `info`.
`PROFILES.md` documents when a domain boundary earns a profile crate and how
to add one.

- **New crate `pk-cli-utility`** — the `utility/v1` profile for account-portal
  CLIs (fpl, tojfl, lrfl, xfin): `UtilitySummary` (`utility-summary/v1`,
  emitted by both `summary` and `balance`), `Statement`, `Payment`,
  `UsagePeriod`, `Transaction`, the `Paged<T>` list envelope
  (`<record>-list/v1`, records under `items`), and `RangeArgs`
  (`--limit`/`--since`/`--until` with ISO validation). Replaces the per-driver
  domain glue (utiman's `balance-fields`/`scale`/`items-path` manifest keys).
- `pk-cli-core`: `cli-info/v1` gains an optional `profiles` field
  (`CliInfo::with_profiles`) — additive; omitted when empty, so existing
  consumers are unaffected.
- `pk-cli-core`: the text renderer now displays `Money` objects as `$12.34`
  (or `12.34 EUR`) in key/value blocks and table cells instead of raw JSON.
- `example-cli`: demonstrates the profile (`summary`, `balance`,
  `bills list [--limit/--since/--until]`, profile declaration in `info`).
- `conformance.md`: family table updated from the 2026-07-19 audit (adds
  lofty + discord, drift notes for the utility four, pre-spec adoption notes);
  new profile-tracking table.

## v0.1.3 — 2026-07-11

- `pk-cli-selfupdate`: on macOS, re-sign the downloaded binary with the stable
  `pk-cli-codesign` identity before installing it, so a prior keychain "Always
  Allow" grant keeps applying across self-updates (no re-prompt on a new
  version). Best-effort — a silent no-op when the identity isn't present.

## v0.1.0 — 2026-07-11

Initial release: SPEC v1 (`DESIGN.md`) and the first extraction of shared
code from `fpl`/`xfin`.

- `pk-cli-core`: `CliError` with stable exit codes 0–6 and `--json` error
  envelope; text/JSON output renderer; `CommonArgs` global flags; date
  helpers (ISO + provider formats); `Money` (string-decimal + currency);
  `cli-info/v1` DTO.
- `pk-cli-secrets`: redacting/zeroizing `Secret`, keychain `CredentialStore`
  with the `piekstra.<bin>` service convention, `--stdin`/`--from-env`
  ingestion via `SecretSourceArgs`.
- `pk-cli-config`: XDG-located JSON `ConfigStore` with typed load/save,
  `--config` override, `clear` for `logout --forget`.
- `pk-cli-selfupdate`: parameterized GitHub-release `Updater` with `--check`,
  atomic in-place replace, `self-update/v1` DTO.
- `pk-cli-auth`: `auth-status/v1` DTO + standard `LoginArgs`,
  `SetCredentialArgs`, `LogoutArgs`.
- `pk-cli-http`: family-default blocking client builder, `ApiArgs`
  passthrough, response→exit-code mapping.
- `example-cli`: runnable template exercising the full surface.

## v0.1.1 — 2026-07-11

- pk-cli-core: optional `reqwest` feature adding `From<reqwest::Error> for CliError` (→ Upstream, exit 5).

## v0.1.2 — 2026-07-11

- pk-cli-selfupdate: `Updater` fields are owned `String`s; added `os_arch()` for `<os>-<arch>` release-asset naming.
