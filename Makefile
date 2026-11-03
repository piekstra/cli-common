# The one definition of the gate. CI's `check` job runs these targets
# (.github/workflows/ci.yml), so a green `make verify` predicts a green PR.

CARGO ?= cargo

.PHONY: verify fmt fmt-check lint test test-util-check mcp-check wasm-check wasm-host-check scripts-check connector-check

verify: fmt-check lint test test-util-check mcp-check wasm-check wasm-host-check scripts-check

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
# above build it with `stdio`, `http` and `test-util` all on (example-cli
# asks for them), so the core alone, the server alone and the test helpers alone
# would otherwise go unchecked.
mcp-check:
	$(CARGO) clippy -p pk-cli-mcp --all-targets -- -D warnings
	$(CARGO) test -p pk-cli-mcp
	$(CARGO) clippy -p pk-cli-mcp --all-targets --features stdio -- -D warnings
	$(CARGO) clippy -p pk-cli-mcp --all-targets --features http -- -D warnings
	$(CARGO) clippy -p pk-cli-mcp --all-targets --features test-util -- -D warnings
	$(CARGO) clippy -p pk-cli-mcp --all-targets --features wasm -- -D warnings
	$(CARGO) test -p pk-cli-mcp --all-features
	$(CARGO) clippy -p pk-cli-utility --all-targets --features mcp -- -D warnings
	$(CARGO) test -p pk-cli-utility --features mcp

# The pk-cli-mcp core and its `wasm` feature, and pk-cli-core under them,
# build for WebAssembly, so a CLI's toolset can run in a deploy-your-own
# Cloudflare Worker: no tokio, keyring, processes or blocking I/O may creep
# into them.
wasm-check:
	@rustup target list --installed 2>/dev/null | grep -qx wasm32-unknown-unknown || { \
		echo "wasm-check needs the target: rustup target add wasm32-unknown-unknown"; exit 1; }
	$(CARGO) check --target wasm32-unknown-unknown -p pk-cli-mcp --no-default-features
	$(CARGO) check --target wasm32-unknown-unknown -p pk-cli-utility --features mcp
	$(CARGO) check --target wasm32-unknown-unknown -p pk-cli-arcgis
	$(CARGO) clippy --target wasm32-unknown-unknown -p pk-cli-core -p pk-cli-mcp --features pk-cli-mcp/wasm -- -D warnings

# The WebAssembly ABI end to end: the demo toolset built as a module the way
# a CLI's release builds one, run under the reference JavaScript host
# (crates/pk-cli-mcp/js) in Node, which strips the TypeScript types itself
# (Node 22.18 or later).
WASM_DEMO := $(or $(CARGO_TARGET_DIR),target)/wasm32-unknown-unknown/release/pk_cli_mcp_wasm_demo.wasm
EXAMPLE_CLI := $(or $(CARGO_TARGET_DIR),target)/debug/example-cli
wasm-host-check:
	@command -v node >/dev/null 2>&1 || { echo "wasm-host-check needs Node 22.18 or later"; exit 1; }
	$(CARGO) rustc -p pk-cli-mcp-wasm-demo --lib --crate-type cdylib --release --target wasm32-unknown-unknown
	PK_WASM_DEMO="$(WASM_DEMO)" node --test crates/pk-cli-mcp/js/pk-host.test.ts

# The shell scripts: shellcheck (skipped with a note where it is not
# installed) and the self-view tests, which stub the window list and so run
# without a display.
scripts-check:
	@if command -v shellcheck >/dev/null 2>&1; then \
		shellcheck scripts/*.sh scripts/test/*.sh; \
	else echo "shellcheck not installed; skipping the lint half of scripts-check"; fi
	bash scripts/test/self-view.test.sh

# The deploy-your-own connector (connector/), CI's `connector` job. Not part
# of `verify`: it needs npm and fetches the pinned modules from their
# releases. Its host must be the reference host byte for byte; then the
# typecheck, the unit tests, and the OAuth and MCP flow end to end under
# `wrangler dev --local` against the Coinbase and Alpaca mocks, with
# example-cli's `mcp --http` as a home module (scripts/e2e.sh). Localhost
# only: no Cloudflare account, no tunnel, nothing deployed.
connector-check:
	cmp connector/src/pk-host.ts crates/pk-cli-mcp/js/pk-host.ts
	$(CARGO) build -p example-cli
	cd connector && npm ci && npm run typecheck && npm test && \
		E2E_HOME_BIN="$(abspath $(EXAMPLE_CLI))" npm run e2e
