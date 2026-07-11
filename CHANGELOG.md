# Changelog

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
