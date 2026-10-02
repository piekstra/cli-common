# The local gate, matching CI's `check` job (.github/workflows/ci.yml): a green
# `make verify` predicts a green PR.

CARGO ?= cargo

.PHONY: verify fmt fmt-check lint test

verify: fmt-check lint test

fmt:
	$(CARGO) fmt --all

fmt-check:
	$(CARGO) fmt --all -- --check

lint:
	$(CARGO) clippy --workspace --all-targets -- -D warnings

test:
	$(CARGO) test --workspace
