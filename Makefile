# The one definition of the gate. CI's `check` job runs these targets
# (.github/workflows/ci.yml), so a green `make verify` predicts a green PR.

CARGO ?= cargo

.PHONY: verify fmt fmt-check lint test scripts-check

verify: fmt-check lint test scripts-check

fmt:
	$(CARGO) fmt --all

fmt-check:
	$(CARGO) fmt --all -- --check

lint:
	$(CARGO) clippy --workspace --all-targets -- -D warnings

test:
	$(CARGO) test --workspace

# The shell scripts: shellcheck (skipped with a note where it is not
# installed) and the self-view tests, which stub the window list and so run
# without a display.
scripts-check:
	@if command -v shellcheck >/dev/null 2>&1; then \
		shellcheck scripts/*.sh scripts/test/*.sh; \
	else echo "shellcheck not installed; skipping the lint half of scripts-check"; fi
	bash scripts/test/self-view.test.sh
