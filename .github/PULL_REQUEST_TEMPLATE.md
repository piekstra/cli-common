<!-- Keep PRs focused. Link the issue: Closes #___ -->

## What & why

## Contract impact

- [ ] No `/v1` schema, exit code, or standard command spelling changed
      (new shapes get a `/v2` schema; see `DESIGN.md`)
- [ ] `DESIGN.md` / `conformance.md` updated if the spec surface changed
- [ ] `CHANGELOG.md` entry added

## Checks

- [ ] `cargo fmt --all` clean
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` clean
- [ ] `cargo test --workspace` passes
- [ ] `example-cli` still demonstrates the affected surface

## Security

- [ ] No secrets or real account data added (code, tests, commits)
