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

## Code signing (macOS)

`scripts/setup-dev-signing.sh` (one-time) + `scripts/dev-sign.sh <bin>` keep
keychain ACL grants stable by signing with the `pk-cli-codesign` identity. The
identity lives only in the owner's login keychain — never commit or distribute
it.

**Sign every target that produces a binary.** `cargo build` and
`cargo install` both ad-hoc sign, giving the binary a *new* code identity each
time. macOS scopes keychain "Always Allow" grants to that identity, so any
unsigned rebuild silently revokes the grant and the next run prompts again —
which reads as a flaky keychain rather than a signing problem.

Signing only `install` and `dev` is not enough: the binary most often run
during development is `./target/release/<bin>`, produced by `release` and by
`verify` through `smoke`. Miss it and the prompts do not stop, they just move.
Wire the same step into `build`, `release`, `install`, and `dev`:

```make
build: SIGN_TARGET = target/debug/$(BIN)
build:
	$(CARGO) build
	@$(SIGN)

release: SIGN_TARGET = target/release/$(BIN)
release:
	$(CARGO) build --release
	@$(SIGN)

install: SIGN_TARGET = $${CARGO_INSTALL_ROOT:-$$HOME/.cargo}/bin/$(BIN)
install:
	$(CARGO) install --path . --force
	@$(SIGN)

dev: SIGN_TARGET = target/debug/$(BIN)
dev:
	cargo build
	@$(SIGN)

define SIGN
if [ -x "$$HOME/Dev/cli-common/scripts/dev-sign.sh" ]; then \
	"$$HOME/Dev/cli-common/scripts/dev-sign.sh" "$(SIGN_TARGET)"; \
else echo "cli-common/scripts/dev-sign.sh not found — $(SIGN_TARGET) left ad-hoc signed"; fi
endef
```

Verify with `codesign -dv $(which <bin>)`: you want the stable
`pk-cli-codesign` signature, not `Signature=adhoc`. Changing the identity
invalidates the previous grant, so expect exactly one more prompt after
adopting this.

## Review agents

`.codereview/agents/cli-family/contract` reviews changes against the rules
above — frozen exit codes, `/v1` schema edits, secret ingestion, and behavior
that belongs here rather than in a downstream CLI. It deliberately does not
review Rust quality; the shared `rust:implementation` lane in
`cr-reviewer-catalog` does that, for every Rust repo rather than only this one.

Agent directories are not auto-discovered. A `cr` profile must list this path
in `agent_sources`, alongside the catalog, or neither reviewer runs.

## Workflow

- `cargo test --workspace && cargo clippy --workspace --all-targets` must be
  clean before committing.
- `example-cli` must keep compiling and demonstrating the full surface — it is
  the template new CLIs copy.
- Releases: bump `workspace.package.version`, update `CHANGELOG.md`, tag
  `vX.Y.Z`. Downstream CLIs consume via `tag = "vX.Y.Z"` git deps.
