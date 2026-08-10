# Changelog

## v0.4.0 — 2026-08-10

Scraped-portal support. `wabhoa` (`piekstra/westernalliancebank-hoa-cli`, a
Western Alliance Bank HOA assessment portal) is the first family CLI whose
provider answers mostly in rendered HTML rather than JSON. Each addition was
written once privately and is generic enough that the next scraped portal
shouldn't rewrite it.

- **`pk-cli-scrape`** (new crate) — read values out of server-rendered pages
  without a DOM parser: `elements`, `attr`, `block_by_id`, `input_value`,
  `table_rows`, `blocks_with_class`, `cells`, `cells_with_class`,
  `strip_tags`, `decode_entities`. **No dependencies.** Every function is
  total — malformed markup yields `None` or an empty list, never a panic — so
  a provider redesign surfaces as an empty table rather than a crash. It is
  not a general-purpose parser and does not try to be: no selector engine, no
  document tree. *Which* IDs and class names to look for stays with the CLI
  that owns the provider; this crate only knows how to look.
- **`pk-cli-auth::reauth::with_reauth`** — recover from a lapsed session
  without making the user re-run the command: run an operation, and on
  `CliError::Auth` re-authenticate once through a caller-supplied closure and
  retry once. The rails are the point, and they are identical for every CLI:
  only `Auth` triggers recovery (upstream/not-found/usage errors are returned
  untouched), recovery runs **at most once** so a broken provider can't become
  a login storm, a failed login reports *its own* error rather than the stale
  "session expired", and a second failure surfaces as itself. A CLI that can't
  log in unattended — an interactive second factor, say — returns an error
  from the closure and keeps plain exit-3 behaviour. The operation may run
  twice, so this is for reads; the doc says so explicitly.
- **`pk-cli-core::dates`: `parse_dotnet`, `DOTNET_MIN`, `is_dotnet_min`,
  `parse_mm_slash_dd_yyyy`** — ASP.NET / ServiceStack `/Date(millis±hhmm)/`
  timestamps, and the inverse of `fmt_mm_slash_dd_yyyy` for reading dates back
  off a rendered page. The trailing offset is deliberately ignored: it
  re-expresses an instant the milliseconds already carry in UTC, so honouring
  it shifts evening timestamps back a day. `DateTime.MinValue` **parses**
  rather than vanishing — classifying it as "absent" is left to the caller,
  because providers layer their own placeholder dates (1900-01-01 is common)
  that only the caller knows about.
- **`pk-cli-core::output`: `emit`, `table_view`, `rows_of`** — §1.4's output
  contract as functions. These had been copy-pasted verbatim into each portal
  CLI. `emit` tags a DTO with `schema` and flattens an object payload
  *alongside* the tag (so consumers read `.payments`, not `.data.payments`),
  `table_view` projects columns with omit-don't-null, and `rows_of` reads an
  array field back out for the text renderer.

**Behaviour change:** `serde_json`'s `preserve_order` feature is now enabled in
this workspace, so JSON object keys and table columns follow **insertion
order** instead of being alphabetized. `table_view` exists to choose and order
columns, and `kv` renders DTO fields in the order the code builds them —
neither contract was real while ordering depended on whether each downstream
CLI happened to set the flag. Cargo features are additive, so this reaches
every consumer. Values, field names, and schema tags are unchanged; only
ordering is, which affects rendered text and JSON key order (JSON objects are
unordered by definition, so conforming consumers are unaffected).

## v0.3.0 — 2026-08-10

Bearer-token support. `cpmfl`
(`piekstra/campbell-property-management-hoa-cli`, a Vantaca HOA portal) is the
first family CLI whose session is a JWT rather than a cookie, and both
additions below came out of building it — each was written once privately and
is generic enough that the next token-auth CLI shouldn't rewrite it.

- **`pk-cli-auth::token`** — read the claims of a cached bearer token so a CLI
  can answer "is this session still usable?" without spending a request:
  `claims`, `numeric_claim`, `expiry`, `expires_at` (RFC 3339, ready for
  `auth-status/v1`'s existing but until-now unpopulated `expires_at` field),
  and `is_expired(token, now, skew)`. Verifies **nothing** — no signature,
  issuer, or audience check; a CLI is the bearer, not the validator, and the
  server stays the only authority. Accordingly, a token whose claims can't be
  read is reported as *not* expired, so an unfamiliar shape still reaches the
  server rather than locking a user out of a good session. The clock skew is a
  caller argument rather than a house rule; `DEFAULT_SKEW_SECS` (60) is offered
  as a starting point.
- **`pk-cli-core`: `Money::from_cents`** — build `Money` from minor units with
  integer arithmetic (no float touches the value). Providers that report an
  integer number of cents are common, and some mix scales across endpoints —
  the same transaction arriving as `25000` from one and `250.00` from another —
  where an open-coded `/100` is a silent 100× error waiting to happen.
- **`pk-cli-core::dates`: `fmt_rfc3339` and `civil_from_unix`** — a Unix
  timestamp as RFC 3339 UTC, reusing the existing Hinnant conversion so the
  family still needs no calendar crate. Correct for pre-epoch timestamps.

## v0.2.1 — 2026-07-30

- `pk-cli-selfupdate`: private-repo support. When `GITHUB_TOKEN` (then
  `GH_TOKEN`; first non-empty wins, no other source) is exported, the
  `releases/latest` lookup is sent with `Authorization: Bearer <token>` and
  assets are downloaded through the GitHub API asset endpoint
  (`/repos/{repo}/releases/assets/{id}` with `Accept:
  application/octet-stream`) instead of `browser_download_url`, which does
  not accept a bare token. With no token, behavior is unchanged (public
  repos unaffected). A 404 on `releases/latest` without a token now
  mentions that the repo may be private, in addition to "no releases yet".
  schwopts (`piekstra/schwab-options-cli`) is the first private family CLI
  to need this.

## v0.2.0 — 2026-07-19

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
