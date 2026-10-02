# The one definition of the gate. CI's `check` job runs these targets
# (.github/workflows/ci.yml), so a green `make verify` predicts a green PR.

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
