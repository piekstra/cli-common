# Agent guide — cli-common

Shared surface spec + library crates for the piekstra CLI family. Read
`DESIGN.md` (the SPEC) before changing any public shape — downstream CLIs
(`fpl`, `tojfl`, `lrfl`, `xfin`, `utiman`) pin tags of this repo.

## Rules

- **Every public DTO shape is a contract.** Fields carry a `"schema": "<name>/v1"`
  tag; changing a shape means a new `/v2` schema, not an edit to `/v1`.
- **Exit codes 0–6 are frozen** (see `pk-cli-core::CliError`). Never renumber.
- **Secrets never on argv, never in logs.** All ingestion goes through
  `pk-cli-secrets` (`--stdin` / `--from-env` / no-echo prompt).
- This repo is public: no employer-internal names, no real account numbers,
  addresses, or personal data in code, fixtures, docs, or git history.
- Keep crates dependency-light; provider-specific logic belongs in the CLIs,
  not here.

## Workflow

- `cargo test --workspace && cargo clippy --workspace --all-targets` must be
  clean before committing.
- `example-cli` must keep compiling and demonstrating the full surface — it is
  the template new CLIs copy.
- Releases: bump `workspace.package.version`, update `CHANGELOG.md`, tag
  `vX.Y.Z`. Downstream CLIs consume via `tag = "vX.Y.Z"` git deps.
