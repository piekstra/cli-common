# The one definition of the gate. CI's `check` job runs these targets
# (.github/workflows/ci.yml), so a green `make verify` predicts a green PR.

CARGO ?= cargo

.PHONY: verify fmt fmt-check lint test test-util-check mcp-check wasm-check scripts-check

verify: fmt-check lint test test-util-check mcp-check wasm-check scripts-check

fmt:
	$(CARGO) fmt --all

fmt-check:
	$(CARGO) fmt --all -- --check

lint:
	$(CARGO) clippy --workspace --all-targets -- -D warnings

test:
	$(CARGO) test --workspace

# pk-cli-browser's `test-util` feature, the constructors a CLI's tests build
# a capture's results with, checked both ways on the crate alone. The
# workspace runs above build it with the feature on, because example-cli's
# dev-dependency asks for it, so without these the build a CLI ships (the
# feature off) would go unchecked.
test-util-check:
	$(CARGO) clippy -p pk-cli-browser --all-targets -- -D warnings
	$(CARGO) test -p pk-cli-browser
	$(CARGO) clippy -p pk-cli-browser --all-targets --features test-util -- -D warnings
	$(CARGO) test -p pk-cli-browser --features test-util

# pk-cli-mcp feature by feature, on the crate alone: the workspace runs
# above build it with `stdio` and `test-util` both on (example-cli asks for
# both), so the core alone, the server alone and the test helpers alone
# would otherwise go unchecked.
mcp-check:
	$(CARGO) clippy -p pk-cli-mcp --all-targets -- -D warnings
	$(CARGO) test -p pk-cli-mcp
	$(CARGO) clippy -p pk-cli-mcp --all-targets --features stdio -- -D warnings
	$(CARGO) clippy -p pk-cli-mcp --all-targets --features test-util -- -D warnings
	$(CARGO) test -p pk-cli-mcp --all-features

# The pk-cli-mcp core builds for WebAssembly, so a CLI's toolset can run in
# a deploy-your-own Cloudflare Worker: no tokio, keyring, processes or
# blocking I/O may creep into its default features.
wasm-check:
	@rustup target list --installed 2>/dev/null | grep -qx wasm32-unknown-unknown || { \
		echo "wasm-check needs the target: rustup target add wasm32-unknown-unknown"; exit 1; }
	$(CARGO) check --target wasm32-unknown-unknown -p pk-cli-mcp --no-default-features

# The shell scripts: shellcheck (skipped with a note where it is not
# installed) and the self-view tests, which stub the window list and so run
# without a display.
scripts-check:
	@if command -v shellcheck >/dev/null 2>&1; then \
		shellcheck scripts/*.sh scripts/test/*.sh; \
	else echo "shellcheck not installed; skipping the lint half of scripts-check"; fi
	bash scripts/test/self-view.test.sh
