# Contributing to cli-common

This repo holds the **piekstra-cli/1** surface spec (`DESIGN.md`) and the
shared `pk-cli-*` crates behind a family of CLIs (fpl, xfin, lrfl, tojfl, …).
Contributions of shared, reusable pieces are encouraged — if two CLIs want the
same behavior, it belongs here, not copied into each repo.

## Before you start

- Open or comment on an issue describing the change.
- Branch from `main`; don't push to `main` directly.
- Read `DESIGN.md`: public DTO shapes, exit codes, and command spellings are
  contracts consumed by tag-pinned downstreams.

## Local checks (must pass)

```sh
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

## Ground rules

- **Schemas are immutable.** A shape change means a new `<name>/v2` schema,
  never an edit to `/v1`. Exit codes 0–6 are frozen.
- **Secrets never on argv, never in logs** — all ingestion via `pk-cli-secrets`.
- Keep crates dependency-light; provider-specific logic stays in the CLIs.
- `example-cli` must keep compiling and demonstrating the full surface.
- Public repo: no employer-internal names, no real account data anywhere,
  including git history.

## Releasing

Bump `workspace.package.version`, update `CHANGELOG.md`, tag `vX.Y.Z`, push
the tag. Downstreams bump their `tag = "vX.Y.Z"` pins.

## License of contributions

Dual-licensed MIT OR Apache-2.0, like the project.
