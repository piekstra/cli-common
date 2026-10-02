# cli-common

Shared surface specification and library crates for a family of Rust CLIs
([`fpl`](https://github.com/piekstra/fpl-cli),
[`tojfl`](https://github.com/piekstra/town-of-jupiter-fl-cli),
[`lrfl`](https://github.com/piekstra/loxahatchee-river-fl-cli),
[`xfin`](https://github.com/piekstra/xfinity-cli), and friends) so that
scripts, agents, and driver tools like
[`utiman`](https://github.com/piekstra/utiman) can treat every CLI the same
way: same auth commands, same `--json` contract, same exit codes, same
self-update.

**[DESIGN.md](DESIGN.md)** is the specification (SPEC v1);
**[conformance.md](conformance.md)** is the per-CLI checklist;
**[PROFILES.md](PROFILES.md)** is the bar for adding domain profiles
(when a domain boundary earns a `pk-cli-<domain>` crate).

## Crates

| Crate | What it gives a CLI |
|---|---|
| `pk-cli-core` | error type + stable exit codes (0–6), `--json`/text output renderer (incl. `emit_list`/`emit_one`), common global flags, date & `Money` helpers, `cli-info/v1` DTO, shared list primitives (`Paged` envelope + `--limit/--since/--until` range flags), the §1.3 confirmation gate (`confirm`), and reference resolution (`resolve::pick`) |
| `pk-cli-secrets` | redacting `Secret` type, OS-keychain `CredentialStore` (`piekstra.<bin>`) with one-item JSON credentials (`get_json`/`set_json`) and legacy-service `migrate_from`, `--stdin`/`--from-env` ingestion (secrets never on argv) |
| `pk-cli-config` | non-secret JSON config at `~/.config/<bin>/config.json` |
| `pk-cli-selfupdate` | `self-update [--check] [-y]` from GitHub Releases, `self-update/v1` DTO |
| `pk-cli-auth` | `auth login/status/logout/set-credential` arg structs, the canonical `auth-status/v1` DTO, `token` (bearer-token claim reads: expiry, `expires_at`), `reauth::with_reauth` (retry a read once after re-authenticating, with the retry-once/no-login-storm rails), and `otp` (the one-time-code login: request, park for a `--code` resume, read the code from the mailbox through `gro` or prompt) |
| `pk-cli-http` | blocking client builder with family defaults, raw `api` passthrough command |
| `pk-cli-utility` | the `utility/v1` domain profile: `utility-summary/v1` + statement/payment/usage/transaction DTOs |
| `pk-cli-documents` | the `documents/v1` domain profile: list & download a portal's published files (`document-list/v1`, `document-download/v1`, …) |
| `pk-cli-scrape` | dependency-free HTML scanning for providers that answer in rendered pages rather than JSON |
| `example-cli` | a runnable template wiring it all together — copy it to start a new family CLI |

## Consuming

Pin to a tag as a git dependency:

```toml
[dependencies]
pk-cli-core = { git = "https://github.com/piekstra/cli-common", tag = "v0.1.0" }
pk-cli-secrets = { git = "https://github.com/piekstra/cli-common", tag = "v0.1.0" }
```

Pre-1.0, breaking changes are allowed and noted in [CHANGELOG.md](CHANGELOG.md).
Publication to crates.io is planned once SPEC v1 freezes.

## The contract in one screen

Exit codes: `0` ok · `1` other · `2` usage · `3` auth · `4` not found ·
`5` upstream/provider · `6` confirmation required.

`--json` on any command → the DTO alone on stdout; on failure,
`{"error": {"code", "message"}}` plus the matching exit code. Canonical DTOs
carry a `"schema"` tag: `auth-status/v1`, `self-update/v1`, `cli-info/v1`,
plus per-profile shapes (e.g. `utility-summary/v1`, `device-rooms/v1`). A CLI
declares its domain profiles in `info` (`"profiles": ["utility/v1"]`).

```console
$ example-cli --json auth status
{
  "schema": "auth-status/v1",
  "required": true,
  "authenticated": true,
  "method": "password",
  "credential_in_keychain": true
}
```

## macOS dev signing

Plain `cargo build` ad-hoc signs, so macOS keychain "Always Allow" grants die
on every rebuild. One-time: `scripts/setup-dev-signing.sh` creates a stable
self-signed `pk-cli-codesign` identity; then re-sign dev builds with
`scripts/dev-sign.sh target/debug/<bin>` (each family CLI wires this up as
`make dev`). One final "Always Allow" per CLI and the prompts stop.

## Self-view for family desktop apps

`scripts/self-view.sh` lets a builder, often an agent, look at and click
through the desktop app it is building. It acts only on a window owned by the
process it launched, or a descendant of that process. It never matches a
window by app name, title or executable path: the owner's own copy of the app
may be running with real data, and it has the same name.

```console
$ S=~/Dev/cli-common/scripts/self-view.sh
$ $S launch --pidfile .self-view/app.pid -- npm run tauri dev
41234
$ $S shot   --pidfile .self-view/app.pid --out /tmp/app.png   # waits up to 30 s for the window
/tmp/app.png
$ $S drive  --pidfile .self-view/app.pid click 120 64          # window-relative points
$ $S bounds --pidfile .self-view/app.pid
212 95 1100 720
$ $S stop   --pidfile .self-view/app.pid
stopped 41234
```

- **Launch the app as a child.** Start the dev loop (`npm run tauri dev`,
  `cargo run`) or the bundle's executable (`Foo.app/Contents/MacOS/foo`)
  through `launch`. Do not use `open`: launchd becomes the parent, so the
  window cannot be tied to the launch, and `shot` refuses.
- **Refusals are the feature.** `shot`, `bounds` and `drive` capture or send
  nothing, and exit with a message, when the pidfile is missing, the PID now
  belongs to another process, the launched process exited (an app that hands
  off to an already-running copy lands here), no on-screen window is owned
  by the launched tree, or the screen is locked. `drive` also refuses when
  another app's window covers the target point after one attempt to raise
  the launched app, and succeeds only once the pointer reads back at the
  target.
- **Exit codes** follow the family table, so a caller can branch without
  parsing messages:

  | Code | Meaning | What to do |
  | --- | --- | --- |
  | 1 | Refused for safety: another app's window or listener is involved, a non-loopback URL, or a launch is already tracked | stop or bring the app forward |
  | 2 | Usage | fix the call |
  | 3 | Permission missing: Screen Recording (`shot`) or Accessibility (`drive`) | owner grants it |
  | 4 | Nothing to act on: no pidfile, process exited or PID recycled, no window, nothing listening | launch again |
  | 5 | Environment: screen locked, a Swift helper or the browser failed, a tool is missing | retry later or fix the machine |
- **Window pixels only.** `shot` captures the one window with
  `screencapture -l<id>`, never the full screen.
- **Web frontends** that are verified in a browser use
  `web --serve CMD --url http://127.0.0.1:PORT/`. It launches CMD, waits for
  the port, requires every listener on it to be in the launched tree, renders
  the page in a headless Chromium with a throwaway profile (no cookies or
  logins from the owner's browser), writes the PNG and stops CMD. A server
  started earlier with `launch` works with `--pidfile` instead of `--serve`.
  Loopback URLs only.
- **Permissions (macOS):** Screen Recording for `shot`, Accessibility for
  `drive`, both granted to the terminal or agent process.
- **Wiring it into a family app:** keep a thin wrapper in the app repo that
  fixes the pidfile path (gitignored, per checkout, so parallel worktrees do
  not collide) and the launch command, and calls this script from
  `$HOME/Dev/cli-common`. Run the launched app against throwaway data (an
  isolated `HOME`), never the owner's.

`make scripts-check` lints the scripts and runs `scripts/test/self-view.test.sh`,
which stubs the window list and so needs no display.

## Development

```console
cargo test --workspace
cargo clippy --workspace --all-targets
cargo run -p example-cli -- --help
```

## License

MIT OR Apache-2.0, at your option.
