# cli-common

Shared surface specification and library crates for a family of Rust CLIs
([`fpl`](https://github.com/piekstra/fpl),
[`tojfl`](https://github.com/piekstra/town-of-jupiter-fl),
[`lrfl`](https://github.com/piekstra/loxahatchee-river-fl),
[`xfin`](https://github.com/piekstra/xfinity-cli), and friends) so that
scripts, agents, and driver tools like
[`utiman`](https://github.com/piekstra/utiman) can treat every CLI the same
way: same auth commands, same `--json` contract, same exit codes, same
self-update.

**[DESIGN.md](DESIGN.md)** is the specification (SPEC v1);
**[conformance.md](conformance.md)** is the per-CLI checklist.

## Crates

| Crate | What it gives a CLI |
|---|---|
| `pk-cli-core` | error type + stable exit codes (0–6), `--json`/text output renderer, common global flags, date & `Money` helpers, `cli-info/v1` DTO |
| `pk-cli-secrets` | redacting `Secret` type, OS-keychain `CredentialStore` (`piekstra.<bin>`), `--stdin`/`--from-env` ingestion (secrets never on argv) |
| `pk-cli-config` | non-secret JSON config at `~/.config/<bin>/config.json` |
| `pk-cli-selfupdate` | `self-update [--check] [-y]` from GitHub Releases, `self-update/v1` DTO |
| `pk-cli-auth` | `auth login/status/logout/set-credential` arg structs and the canonical `auth-status/v1` DTO |
| `pk-cli-http` | blocking client builder with family defaults, raw `api` passthrough command |
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
carry a `"schema"` tag: `auth-status/v1`, `self-update/v1`, `cli-info/v1`.

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

## Development

```console
cargo test --workspace
cargo clippy --workspace --all-targets
cargo run -p example-cli -- --help
```

## License

MIT OR Apache-2.0, at your option.
