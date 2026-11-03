# Changelog

## Unreleased

- `pk-cli-mcp`: `ResumeRequest`, for a list the provider reads on from a position (an id or a
  cursor of its own), whose rows can arrive or leave between pages. The next cursor holds the
  tool's key for where to read on, so a row that leaves between pages skips nothing and a deep
  page costs one page. `PositionRequest` stays for a list a tool reads whole on every page.
- `pk-cli-secrets`: on Windows, `CredentialStore::set` stores a value over one credential's
  limit across several and `get` puts it back together, so a browser session's tokens no
  longer fail at the end of a sign-in. The limit is 2,560 bytes, which `keyring` fills as
  UTF-16, so 1,280 code units. A value that fits stays one plain item, so what an older
  build stored still reads. A failed write leaves the old value readable, a read that
  cannot put a value together is an error naming the item, and `delete` removes every part.
  macOS and Linux have no such limit, and there each call stays the one keychain operation
  it was. A new workflow runs this on the real Credential Manager, on any change to the crate.

## v0.26.0 — 2026-11-03

- `pk-cli-config`: `~` is `HOME`, then `USERPROFILE`, so a config resolves on Windows, where
  Claude Desktop starts a server with no `HOME`. It failed with "$HOME unset".
  An empty variable counts as unset.
- `pk-cli-selfupdate`: a release archive's binary may be `<binary>.exe`. Every Windows install
  failed with "the release archive did not contain" it.
- `pk-cli-mcp`: a position cursor. `PositionRequest`, beside `PageRequest`, takes every
  matching row paired with its `PositionKey` and an `Order`, sorts them, and returns a cursor
  holding the last row's position instead of a count of rows, so a row that arrives between
  pages cannot repeat one and a row that leaves cannot skip one. A `PositionKey` is a sort value
  and an id; the cursor holds the sort value encoded and a fingerprint of the id. A cursor from
  before a tool moved is honoured as its count, and `PageRequest` refuses a position cursor
  before anything is fetched.
- `pk-cli-utility`: `<prefix>_bills` and `<prefix>_payments` page by position, so every
  `utility/v1` CLI moves on its bump. Rows sharing a date now run in a fixed order of their
  own, the provider's within a day aside. `<prefix>_usage` stays by count of rows: a period's label can be
  a provider's cycle name, which says nothing about where it sorts.
- `pk-cli-core`: `CliError::because(Reason)` types why an error happened and
  `CliError::with_remedy` keeps what to do next apart from what went wrong, on any kind
  of error, so a caller answers by cause without reading the message: `reason()`,
  `remedy()`, `message()`, `base()`, `into_base()` and `is_auth()`. The exit code and slug
  are the wrapped error's. `Display` and `--json` print the remedy after the message,
  joined by an em dash. **A new variant, `CliError::Detailed`:** a `match` on `CliError`
  with no catch-all needs an arm for it, and a pattern such as `CliError::Auth(_)` does
  not see through it, so a CLI that types a cause moves its own patterns to `is_auth()`
  or `base()`. The shared crates' own errors stay plain.
- `pk-cli-mcp`: a remedy kept apart stays out of a tool's text, with nothing searched for
  in the message. An auth message with no remedy kept apart still has its closing remedy
  cut from the text, for messages not yet moved.
- `pk-cli-auth`: `with_reauth` and the one-time-code flow treat any exit-3 error as auth,
  typed or not. `pk-cli-drive`'s `is_stale_write_refusal` and `pk-cli-utility`'s masking
  of account ids look through a typed error too.

## v0.25.0 — 2026-11-03

- `pk-cli-mcp`: `SendDeadline` is DESIGN.md §1.9's "A late write sends nothing" in one
  place. A toolset starts it when a call begins and calls `check()` right before each
  send: at its limit or past it, on the monotonic or the wall clock, the answer is an
  error (exit 1) saying the command was not sent, to check the current state, and to call
  again only if the user still wants the change. `elapsed()` is for a write's later limits,
  such as when a read-back may last start. The clocks sit behind `Clock`, in the core, so
  the type builds for WebAssembly; `SystemClock`, the machine's own, is native only, since
  the real clocks panic on `wasm32-unknown-unknown`. `CALL_LIMIT` is the 120 s `--http`
  gives a call.
- `pk-cli-mcp`: feature `test-util` adds `conformance::ManualClock`, which a test moves by
  hand (`advance`, `sleep`, `set_wall_back`), so a deadline needs no real sleep to test.
- `example-cli`: its write checks a `SendDeadline`, for a CLI copying the shape.

## v0.24.0 — 2026-11-03

- `pk-cli-core`: `transport::require_secure_url` refuses a URL that is not `https`,
  except plain `http` to this machine, naming only the scheme and host. Call it where
  a URL or host comes from outside the binary and a credential goes with the request.
- `pk-cli-http`: `ApiArgs::secure_url` is `ApiArgs::url` with that check, for an `api`
  passthrough that sends a credential.
- `pk-cli-core`: feature `reqwest` adds two redirect policies. `transport::secure_redirects`
  keeps reqwest's limit of ten and errors on a redirect that leaves `https`.
  `transport::same_host_redirects` also errors on a redirect to another host or port,
  for a client that sends a provider's own auth header, which reqwest keeps on every hop.
- `pk-cli-core`: a `reqwest` failure's message names the origin, not the whole URL, since
  a path or query can carry an account id or a token. A redirect policy's reason is now
  in the message.
- `conformance.md`: the rules for these, and every workflow file sets `permissions:` at
  the top level.

## v0.23.0 — 2026-11-03

- `pk-cli-arcgis`: read-only queries against an ArcGIS REST layer, for CLIs over the
  public GIS services many US counties publish parcel data from. Written over `Transport`,
  so it also runs in a Worker. `Layer::rows` pages by the rows received, asks past a full
  page even without `exceededTransferLimit`, and requires a sort. An error in an HTTP 200
  body keeps its exit code on the first request, and is upstream on a later page. A body
  without `features`, a feature without `attributes` or a flag that is not a boolean
  fails instead of reading as no rows. `quote` makes a SQL string literal.

## v0.22.0 — 2026-11-03

- `pk-cli-mcp`: feature `http` exports `HTTP_TOKEN_ACCOUNT`, the keychain item
  `--new-token` writes, so a CLI's `auth logout --forget` can remove it by the writer's
  own name.
- `pk-cli-mcp`: a `CliError::Keychain` reads "the OS keychain could not be used". It said
  "could not be read", which was wrong for a tool that failed writing a renewed session.

## v0.21.0 — 2026-11-03

- `pk-cli-utility`: feature `mcp`, the `utility/v1` MCP tools written once.
  `UtilityTools` serves `<prefix>_balance`, `<prefix>_bills`, `<prefix>_payments`
  and `<prefix>_usage` over a `UtilitySource` a CLI writes from its existing reads.
  The tools are read-only, take `account` by number or address (only the number when
  the login lists no addresses), filter by `since`/`until`, page with cursors, and
  answer concisely by default. A provider without payments, usage or several accounts
  drops the tool or argument with `without_payments`, `without_usage` or
  `single_account`. Concise results name an account by address, or by the last four
  characters of its number when it has none, and send full numbers only under
  `detailed`. The exception is a listed account whose name does not single it out,
  which can only be asked for by number. The feature uses
  `pk-cli-mcp`'s core only, and `make wasm-check` holds it to
  `wasm32-unknown-unknown`.
- `pk-cli-mcp`: `Server::sign_in_command` names a sign-in other than `<bin> auth login`
  for the instructions and every auth error; `Server::hosted` replaces it. A
  `CliError::Auth` converted to a `ToolError` loses the remedy its message closes on
  for a terminal, so one error names one command.

## v0.20.0 — 2026-11-03

- `pk-cli-browser`: `Confirm::Undecided { reason, retry_after }` and
  `Confirm::undecided(reason)`, for a confirm hook's read that could not decide (a
  429, a 5xx, no answer, an edge block's page). The crate keeps the tab and the jar
  and asks again after a backoff of 6 s, doubling to 60 s, or longer if the
  provider's `Retry-After` asks, up to five minutes, with one last ask at the
  deadline. The first
  such answer prints one status line. A wait that runs out with every pending
  capture undecided is `BrowserError::Undecided`, exit 5, rather than `TimedOut`,
  exit 3, because signing in again fixes nothing.
- `pk-cli-auth`: `RefreshLock` unlocks when released rather than relying on the
  close. A process being spawned on another thread holds a copy of the descriptor
  until it execs, so the next holder could find the lock taken. This also failed
  about 1 test run in 25.
- `pk-cli-core`: `HttpResponse::inconclusive(now)` names an answer that says nothing
  about the session (a 429, a 5xx, a 403 with an HTML page), and
  `parse_retry_after(value, now)` reads `Retry-After` in seconds or as an IMF-fixdate.
  `pk-cli-http`: `inconclusive(&response)` and `retry_after(&headers)` do the same for
  a `reqwest` response.
- `pk-cli-auth`: `AuthLogin`, the `auth-login/v1` result, and an optional
  `AuthStatus::verified` for a login that answers with `auth-status/v1`.
- `DESIGN.md` §2.2 and `conformance.md` state what a window capture does when its
  verifying read cannot decide. A refusal stores nothing. A value that can only be
  this sign-in's stores with a warning and exit 0. A value that could predate the
  sign-in answers `Confirm::Undecided`. A 2xx alone is not acceptance. §1.4 defines
  `auth-login/v1` for a CLI with `auth login --browser`, which always carries
  `verified`.

## v0.19.0 — 2026-11-03

- `pk-cli-browser`: `Launch::clear_site_storage(bool)`, default off, for a CLI
  whose provider keeps tokens in site storage. A kept profile retains that
  storage, so on the next launch the site's own script can present a retired
  refresh token, which reuse detection answers by revoking the whole token
  family, a capture can read a stale bundle, and the token sits unencrypted on
  disk between runs. With the option on, a kept profile keeps its cookies and
  never `Default/Local Storage`, `Session Storage`, `IndexedDB`, `WebStorage`,
  `Service Worker`, `File System` or `databases`. They are removed once the run
  lock is held and no Chrome is running there, before Chrome starts (a failure
  is `BrowserError::Profile` naming the path, and no Chrome starts), and again,
  best effort, once the `Browser` guard has stopped Chrome. A crash or a second
  Ctrl-C skips the second removal, as does a Chrome still running in the profile
  once the spawned process has ended, with a warning; the next launch's removal
  covers each. A symlink is
  removed as a link and its target never touched; a `Default` that is a symlink
  refuses the launch. No effect on a scratch profile.
- `pk-cli-browser`: `profile_exists(bin)` and `named_profile_exists(bin, name)`
  say whether a kept profile exists, for `auth status`. They answer and refuse
  as `forget_profile` and `forget_named_profile` do, take no lock and create
  nothing. `test-util` adds `profile_exists_in` and `named_profile_exists_in`.
- `pk-cli-browser`: `BrowserArgs` refuses `--fresh-profile` without `--browser`
  (exit 2) when the CLI marks `browser` as conflicting with another argument.
  Clap counts a `requires` as satisfied when the required argument conflicts with
  one given, so `--fresh-profile --stdin` parsed and the flag was ignored. A CLI
  needs no change beyond the new tag; `BrowserArgs` is the same flags, argument
  ids and group, and implements `clap::Args` and `FromArgMatches` by hand.
- `pk-cli-browser`: a kept profile that belongs to another user is refused with
  a message that says so, without printing uid numbers.
- `pk-cli-browser`: dropping a kept profile unlocks its run lock explicitly. On
  a close alone, the lock lasted as long as a copy of the descriptor in a process
  another thread was spawning, which refused the next sign-in as `ProfileInUse`.
- `pk-cli-browser`: dropping the `Browser` guard on a kept profile asks Chrome to
  quit over its debug port (`Browser.close`) and waits up to 5 s for it to exit,
  so a persistent cookie the provider set just before the close is on disk for
  the next launch. SIGTERM lost it in most runs. A dead connection or a refusal
  goes straight to SIGTERM, and a Chrome still running at 5 s gets it then, so a
  stuck Chrome holds a CLI for up to 11 s, up from 6 s. A normal drop takes about
  80 ms, up from about 55 ms. A second Ctrl-C stays immediate, and a scratch
  profile is stopped as before. Measured in headless Chrome 156 only: a visible
  window is unverified (DESIGN.md §2.2).
- `pk-cli-auth`: `AuthStatus` gains an optional `browser_profile`, left off the
  wire when unset, within `auth-status/v1`. `AuthStatus` is `#[non_exhaustive]`,
  so it must be built with `AuthStatus::new` or `not_required`: a struct literal,
  or a pattern without `..`, no longer compiles. The JSON shape does not change.
- `example-cli`: `auth status` reports the kept profile through a seam its tests
  fill with `profile_exists_in`, and `auth login --browser` turns on
  `clear_site_storage`, since its demo portal keeps tokens in `localStorage`.
- `DESIGN.md` §1.4, §1.7, §2.2, `conformance.md`, `README.md`: these calls, and
  when a CLI turns the option on.

## v0.18.0 — 2026-11-03

- `pk-cli-auth`: `RefreshLock` covers a person around a login's and a logout's
  keychain write. `before_signin` checks the lock before the browser opens or a
  secret is read, so a stuck or unusable lock fails while nothing is lost.
  `store_signin` is the write: it waits four times as long as a renewal, and its
  error says the sign-in worked and the session was not stored.
  `forget_for_binary` and `forget` are a logout's delete, which runs with or
  without the lock. If another process holds it, the delete runs at once and again
  when the lock frees; past the wait, or with no home directory to find the lock
  in, it exits 5 saying to run it again. Past an unusable lock it warns on stderr.
  `try_hold` runs its closure only if the lock is free right now, for a caller
  that does something different when it is not first. `hold` is unchanged in
  behaviour, and its contention text is the same.
- `pk-cli-mcp`: `restart_note` also says a Claude Desktop extension runs its own
  copy, so the new release's `.mcpb` has to be installed.
- `DESIGN.md` §1.7 and `conformance.md`: these calls, and the lock only excluding
  copies that take it, so `SECURITY.md` says which copies must be on a release
  with it.

## v0.17.0 — 2026-11-03

- `pk-cli-auth`: `refresh_lock::RefreshLock`, for a provider that rotates
  refresh tokens. All of a CLI's processes share one keychain item, so an
  in-process mutex does not stop `<bin> mcp --http` and a terminal command
  presenting the same token, and a provider may revoke the session for it.
  `renew` reads the stored session under an exclusive advisory lock, asks the
  caller's `due` whether it still needs renewing, and only then posts the
  grant; `hold` wraps a login's or logout's keychain write. The lock is an
  empty file, `~/.config/<bin>/token-refresh.lock`, whatever `--config` or
  `XDG_CONFIG_HOME` say. The wait is bounded (75 s, then exit 5 naming the
  lock). The crate now needs Rust 1.89 (`File::try_lock`).
- `pk-cli-selfupdate`: `Updater::run_noting` prints a note after an install.
  `run` is unchanged.
- `pk-cli-mcp`: `<bin> mcp --http`'s start line names the version, and
  `restart_note(bin)` is the note for `run_noting`: a running server keeps the
  old code after `self-update`, with the `launchctl kickstart -k` line on macOS.
  `example-cli` uses both; the connector's README has the step.
- `conformance.md`: renewals under the lock; for a home module, the restart
  note and what its `SECURITY.md` must say about the token, the Worker's copy,
  what crosses Cloudflare, and revoking.
- `DESIGN.md`: the `--no-color` row says how `NO_COLOR` is read.

## v0.16.0 — 2026-11-03

Behaviour change in `pk-cli-browser`: a sign-in now runs in a profile each
CLI keeps, where it used to run in a scratch profile deleted after every
sign-in. Additive API; no shape or exit code changes.

- **`pk-cli-browser`: a kept, owner-only Chrome profile per CLI**
  (DESIGN.md §1.7, §2.2). `Launch::spawn` runs Chrome in
  `<data dir>/piekstra.<bin>/browser-profile`
  (`~/Library/Application Support/piekstra.<bin>/browser-profile` on macOS,
  `$XDG_DATA_HOME` or `~/.local/share` elsewhere) and leaves it in place
  when the window closes; Chrome is still stopped on drop. The provider's
  own cookies (its remembered device, its single-sign-on session, a bot
  check's clearance) carry over, so a CLI's second and later
  `auth login --browser` often finish with no password, code or bot check:
  the window opens, the app loads signed in, the capture is read and the
  window closes. The trade, stated in the docs: a profile kept on disk for
  far fewer sign-ins. Chrome encrypts its cookies at rest (on macOS with
  the "Chrome Safe Storage" keychain key) but not a site's `localStorage`.
  - **macOS and Linux.** Elsewhere every sign-in uses a scratch profile,
    with a one-line note on stderr.
  - **Owner-only.** Created `0700` with any missing parents; a launch is
    refused when the profile is a symlink, is not the current user's or is
    not `0700`.
  - **One sign-in at a time.** A sign-in holds an advisory lock on
    `pk-cli-browser.lock` in the profile (released by the kernel if the
    process dies), so a second is refused before anything launches; a
    Chrome still running in the profile (its `SingletonLock` naming a live
    pid whose program is a Chrome) refuses the launch too, and the message
    names the lock to remove by hand if no Chrome is open there. A program
    is a Chrome when the profile's record names it or its name is in the
    Chromium family: each sign-in records in `pk-cli-browser.lock` the
    program it spawns and, once Chrome is up, the program Chrome runs as,
    so a browser picked with `Launch::chrome` or `<PREFIX>_CHROME` (or
    started by a wrapper script) is known to a later launch or logout. A lock
    whose pid is gone, or now runs another program (a pid reused after a
    reboot), is stale: it, the other singleton links and the old
    `DevToolsActivePort` are removed before Chrome starts.
  - **`Launch::fresh_profile(bool)`** opts back into the scratch profile
    for one run (created `0700` in the temp dir, removed on drop), leaving
    the kept one alone. **`BrowserArgs` gains `--fresh-profile`**
    (`fresh_profile`), which requires `--browser`.
  - **`forget_profile(bin) -> Result<bool, BrowserError>`** removes the
    kept profile (`true` when there was one), for `auth logout`. It removes
    the profile directory only and refuses while a sign-in or a Chrome is
    using it, or when the path is a symlink or another user's.
  - **Several accounts:** `Launch::profile_name(name)` keeps a separate
    `browser-profile-<name>` per account at the provider, and
    `forget_named_profile(bin, name)` removes it, so one account's sign-in
    never opens signed in as another. Names are 1 to 64 ASCII letters,
    digits, `-` or `_`, checked and never rewritten; uppercase is written as
    `+` and its lowercase in the directory name, so names that differ only
    in case keep their own profiles on a case-folding filesystem.
  - **`BrowserError::Profile`** (exit 1): the kept profile needs repair (not
    a private directory of the user's, or it cannot be located, created or
    removed). **`BrowserError::ProfileInUse`** (exit 1): another sign-in or
    a running Chrome has it. The enum is `#[non_exhaustive]`, so both are
    additive.
  - **`test-util`** gains `Launch::profile_root(root)`,
    `forget_profile_in(root, bin)` and `forget_named_profile_in(root, bin,
    name)`, so a CLI's logout tests run against a temp dir and never remove
    the owner's real profile.
  - A second Ctrl-C still kills Chrome and exits 130; it removes a scratch
    profile, never the kept one.
- `example-cli`: `auth login --browser --fresh-profile` is passed to
  `Launch::fresh_profile`, and `auth logout` calls `forget_profile` through
  a seam its test fills with `forget_profile_in`.
- `connector/` 0.2.0: every module is optional. A module is on when any of
  its secrets is set (not empty, not `none`); the Deploy form offers each
  module's secrets pre-filled with `none`, and a deployer who fills in only
  Coinbase's gets only Coinbase's tools. With nothing set up, the
  instructions say so.
- `connector/`: alpaca-cli's `alpaca_portfolio` and `alpaca_activities`, from
  its v0.3.0 module (`ALP_API_KEY_ID`, `ALP_API_SECRET_KEY`, optional
  `ALP_ENV`), with a localhost Alpaca mock that checks the key headers and
  pages the activity feed.
- `connector/`: home modules. A portal CLI running `<bin> mcp --http` on the
  owner's computer is reached through a Workers VPC service binding
  (`HOME_<NAME>`) to the owner's Cloudflare Tunnel, or a tunnel hostname
  behind an Access service token (`HOME_<NAME>_URL`), with the bearer token
  in `HOME_<NAME>_TOKEN`; it serves the tools named `<name>_…`. The tool
  list is kept in memory and KV, so the tools stay listed while the computer
  sleeps, and a call pings first, so a sleeping computer answers `offline:`
  at once. README: the owner's steps (tunnel, VPC service, launchd agent,
  token) and the home path's threat model.
- `make connector-check` builds example-cli and runs the e2e with it as a
  home module behind a local stand-in for the VPC binding, stops it to check
  the offline answer, and restarts the Worker with each module turned off.

## v0.15.1 — 2026-11-02

- `pk-cli-core`: `NO_COLOR` follows no-color.org — any non-empty value
  disables color. `CommonArgs` read it through clap's `env`, which parses a
  boolean, so the conventional `NO_COLOR=1` failed every command with a
  usage error (exit 2). It is now the `--no-color` flag's default.
  `example-cli`'s `tests/no_color.rs` runs the binary under each value.
- `pk-cli-auth`: `GroMailbox::credential_ref` passes the ref to `gro` as
  `<SERVICE>_CREDENTIAL_REF` in the child's environment
  (`GOOGLE_READONLY_CREDENTIAL_REF` for `google-readonly/<profile>`), which
  gro 1.x and 2.x both read. It passed `--ref`, which gro 2.x rejects, so a
  named mailbox profile failed every look. The ref must now be
  `<service>/<profile>`.
- `pk-cli-auth`: the code extractor treats "one-time" as introducing a code,
  so an email reading "your one-time password is: 123456" yields its code
  even when it carries other code-length numbers.

## v0.15.0 — 2026-11-02

- **`pk-cli-mcp` feature `http`** (includes `stdio`): `<bin> mcp --http
  --port <PORT>` serves the CLI's tools over stateless Streamable HTTP
  (rmcp's service on a hyper HTTP/1.1 listener, JSON answers, no sessions)
  for the connector's "home" modules, which reach a portal CLI on the
  user's own computer through their Cloudflare Tunnel. It binds 127.0.0.1
  only; refuses to start without a bearer token (32 or more visible ASCII
  characters, from `<BIN>_MCP_TOKEN` or the keychain item `mcp-http-token`
  under `piekstra.<bin>`); checks the token in constant time, `Host`
  (127.0.0.1 or localhost, its own port if one is named) and refuses any
  request with an `Origin`; answers only `POST /mcp`, with no CORS headers;
  serves only read-only tools unless `--allow-writes`; and answers a call
  still running after 120 s with an error (a read may be retried; a
  write's outcome is reported as unknown, to be checked before any retry).
  `<bin> mcp --new-token` saves 256 random bits as the token, reads them
  back, and prints them once for the Worker's secret; it will not replace
  a saved token without `--overwrite`. A CLI turns the feature on;
  `McpArgs` and `run` carry the rest. `example-cli` serves it, and
  `tests/mcp_http.rs` drives the binary over HTTP through every refusal.
- `pk-cli-secrets`: `SecretSpec::remedy` names how to store a secret again,
  for the error when its keychain item is empty (default `auth login
  --overwrite`, right for a sign-in credential; `mcp --http`'s token names
  `mcp --new-token --overwrite`).
- `pk-cli-mcp`: `McpArgs` is `#[non_exhaustive]`, so its shape no longer
  depends on a feature: build it with clap or `McpArgs::default()`, not a
  struct literal. (Breaking for a struct literal; no family CLI uses one.)
- `connector/`: the deploy-your-own Cloudflare Worker that serves
  official-API CLIs' MCP tools to Claude on the web and mobile, replacing
  the spike in #36. Each user deploys it into their own Cloudflare account
  with the Deploy to Cloudflare button; their provider keys are the Worker's
  secrets. It pins each CLI's released module by URL and SHA-256
  (`modules.json`) and its build fetches and verifies them; no binary is
  committed. Only the owner's passphrase (20 characters or more; ten wrong
  tries an hour lock the page) connects a client, over OAuth with dynamic
  registration and PKCE, and authorization codes go only to Claude's
  callback, loopback, or `EXTRA_REDIRECT_URIS`. Each grant records which
  passphrase approved it, so changing the passphrase ends every connection;
  `/revoke` disconnects every client without changing it. Serves coinbase-cli's two tools from its v0.3.0 module. README:
  walkthrough and threat model.
- `make connector-check` and CI's `connector` job: the host copy matches
  `js/pk-host.ts`, typecheck, unit tests, and the OAuth and MCP flow end to
  end under `wrangler dev --local` against a JWT-checking mock, including a
  passphrase change.

## v0.14.0 — 2026-11-02

Additive: new modules, a new feature and a new example crate. One move:
`block_on` now lives in `pk-cli-core` (`pk_cli_core::block_on`);
`pk_cli_mcp::block_on` (feature `test-util`) re-exports it, so existing
callers are unchanged.

- **`pk-cli-core::transport`** — the seam a provider client is written
  against so the same client runs in the terminal binary and in the
  deploy-your-own Worker: `HttpRequest` (builder; `query` percent-encodes
  everything but RFC 3986's unreserved characters; `user_agent(bin,
  version)` for the family's `User-Agent`; `origin()`, without userinfo),
  `HttpResponse` (status, headers shaped as `fetch` gives them, and body
  text; `new`, `header_value`),
  both `#[non_exhaustive]` with a `Debug` that shows no header value, query
  or body; the async `Transport` trait with no `Send` bound and one contract
  for every implementation: no redirects followed (a 3xx comes back with its
  `Location`), no state or default headers between sends, and a request
  with no HTTP answer is
  `CliError::Upstream` naming the origin only; and `PlatformSigner`
  (`sign_es256`: ECDSA P-256 / SHA-256 by the platform's own crypto, `None`
  where it has none), a separate trait so a wrapper around a transport
  cannot drop it unnoticed. Both implemented for `&T`. Builds for
  `wasm32-unknown-unknown`. `pk_cli_core::block_on` drives a blocking
  transport's futures, which finish on their first poll.
- **`pk-cli-http::BlockingTransport`** — the family's blocking `reqwest`
  client behind `Transport` and `PlatformSigner` (always `None`). `new()`
  keeps the transport contract (no redirects, cookie store or default
  headers; a 60 s timeout); `with_client` takes a client that must do the
  same. Repeated response headers are joined as `fetch` joins them.
- **`pk-cli-mcp` feature `wasm`** (no dependencies) — a `Toolset` as a
  self-contained WebAssembly module for the deploy-your-own Worker
  (DESIGN.md §1.9 path B, §2.3):
  - `export_wasm!(factory)` exports ABI version 1, documented in
    `src/wasm/mod.rs`. A module imports only five `pk_host` functions
    (`now_ms`, `random`, `fetch`, `sign_es256`, `log`) and exports
    `pk_describe` (serverInfo, instructions, tools) and
    `pk_call`/`pk_poll`/`pk_complete`/`pk_drop`, which run a `tools/call` as
    a task the host polls while it settles the operations the task started,
    with several calls in flight at once. No `wasm-bindgen`: a module is one
    `.wasm` with no JavaScript glue, so a release ships one file pinned by
    its SHA-256, and one host serves every module.
  - The factory builds the toolset from the host's per-call config JSON;
    every instruction and auth remedy the module sends has the Worker's
    wording (`Server::hosted`: the credentials are Worker secrets).
  - `HostTransport`: the host's `fetch` (redirects never followed; the
    response headers come back framed ahead of the body) behind
    `Transport`, and its WebCrypto ES256 behind `PlatformSigner`, `None`
    where the host has no WebCrypto ECDSA. `fill_random` and `now_ms` replace
    `getrandom` and `SystemTime`, which do not work on
    `wasm32-unknown-unknown` without `wasm-bindgen`. `log` writes to the
    Worker's logs; a panic is logged before the module traps.
  - `js/pk-host.ts`, the reference host: checks a module's imports, exports
    and ABI version, runs calls concurrently, caches imported keys per
    isolate, and replaces an instance that traps with a fresh one. Plain
    TypeScript that Node 22.18+ runs and wrangler bundles as is.
  - With `test-util`, `wasm::testing::{call, describe}` run a Worker
    toolset natively through the same ABI functions, the test answering
    each `fetch` and signature.
- `pk-cli-core`: `CliError` is `Clone`, `PartialEq` and `Eq`, so a
  provider client that failed to build can hand every call the same error.
- `pk-cli-mcp`: `impl Toolset for &T`.
- **`pk-cli-mcp-wasm-demo`** (new, not published): a toolset exported as a
  module. `make wasm-host-check` builds it for wasm32 the way a CLI's
  release does and runs `js/pk-host.test.ts` over it in Node: fetch,
  WebCrypto signatures verified against the public key, concurrent
  operations and calls, the clock and random source, a panic survived, and
  the error kinds.
- `make wasm-check` also lints `pk-cli-core` and `pk-cli-mcp --features
  wasm` for wasm32; `make mcp-check` lints the `wasm` feature natively; CI
  sets up Node 24 and runs `make wasm-host-check`.

## v0.12.0 — 2026-11-02

Additive: a new crate and an `example-cli mcp` subcommand; no existing
crate's API or behaviour changes.

- **`pk-cli-mcp`** (new crate; DESIGN.md §1.9 "MCP profile", §2.3) — MCP
  tools over a CLI's own commands, so anyone can connect Claude to their own
  account through the open-source CLI with no shared hosted service.
  - **Core, default features, builds for `wasm32-unknown-unknown`** (no
    Tokio, keyring, processes or blocking I/O), so the same toolset runs in a
    deploy-your-own Cloudflare Worker. `ToolDef` (`read`/`write`
    constructors that set `title` and every annotation explicitly; `write`
    adds `_meta` `anthropic/requiresUserInteraction: true`; input and
    output JSON Schemas from `schemars` derives, compacted: subschemas
    inlined, `$schema`, Rust integer formats and `null` alternatives
    dropped, descriptions flattened, output schemas without descriptions);
    `ToolOutput`/`CallToolResult` (structured JSON plus the same JSON as
    compact text; errors as `isError` text with no `structuredContent`);
    the `Toolset` trait (`async fn call` with no `Send` bound, tools in a
    stable order); `shape`: `ListArgs` (`limit` 25 by default and at most
    200, opaque `cursor`, `response_format` concise/detailed), `PageRequest`
    (cursors bound to the query's filters; a page that would pass the
    32,000-character budget stops early with `truncated` and a `hint` naming the
    cursor and the tool's narrowing arguments) and the `Page` envelope;
    `ToolError` (the family exit codes as actionable text: an auth error
    names `<bin> auth login` to run in a terminal; `From<CliError>`);
    `instructions` (the standard server instructions, including that
    third-party free text is data, never instructions); dispatch that
    refuses a misspelled argument, maps an unknown tool to JSON-RPC
    `-32602` and replaces a result over 64,000 characters with an error.
    Sizes count characters, as the hosts' caps do. Output schemas follow
    serde's serialize contract (a plain `Option` is required and nullable,
    a `skip_serializing_if` field optional); input schemas must be flat
    objects. The `Toolset` docs state what a `call` future may depend on
    (no Tokio reactor natively) and the per-target provider seam a toolset
    meant for a Worker awaits. `Server::hosted(note,
    sign_in_remedy)` lets a non-local host replace the instructions'
    and auth errors' local wording (credentials on the user's machine,
    `<bin> auth login` in a terminal).
  - **The WASM boundary is MCP-shaped JSON**: `tools_list_json`,
    `call_tool_json`, `Server::server_info_json` and `instructions` give a
    host the `tools/list` result, the `CallToolResult` object and the
    server identity to pass through; the crate docs hold the contract.
  - **`stdio` feature**: `McpArgs` (with `--list-tools`) and `run`, the
    `<bin> mcp` subcommand on rmcp 3.8 over stdin/stdout, each call on
    Tokio's blocking pool outside any Tokio context, so blocking provider
    code runs unchanged.
  - **`test-util` feature**: `conformance::assert_conforms` and
    `conformance::assert_fits`, the checks every CLI's tests run (names,
    titles, all four hints, writes needing user interaction, three-sentence
    descriptions, a flat input object with described arguments, output
    schemas, definitions under
    8,000 characters, stable order, results under the budget and matching
    their output schema with no undeclared field), plus `call` and
    `block_on`.
  - Needs Rust 1.88.
- `example-cli mcp`: `example_devices` (a paged, filterable read) and
  `example_device_rename` (a write), conformance-checked, and driven over
  stdio by `tests/mcp_stdio.rs`.
- `make mcp-check` (the crate feature by feature) and `make wasm-check` (the
  core for `wasm32-unknown-unknown`) join `make verify` and CI; the
  `security` job runs `cargo deny check` against a new `deny.toml` (the
  family's license and source lists).

## v0.11.0 — 2026-10-30

Additive: without the new feature nothing changes, in the API or in
behaviour.

- **`pk-cli-browser`: a `test-util` feature, so a CLI's unit tests can
  build what a capture hands over** (DESIGN.md §2.2). Only the crate could
  construct `Captured`, `JarCookie`, `CapturedRequest` and `LoggedRequest`,
  so a CLI could not run its confirm hook, or its mapping of a capture onto
  its session, on real values. With the feature on, for a CLI's
  `[dev-dependencies]` only (`pk-cli-browser = { …, features =
  ["test-util"] }`, the same tag as the dependency):
  - `Captured::for_test(label, host)` starts a `CapturedBuilder` whose steps
    `value`, `cookie`, `jar`, `request`, `request_log` and `user_agent` set
    what each getter answers (a later step replaces an earlier one; `cookie`
    adds one by name), ended by `build()`. `cookie_header()` is built from
    the jar as for a read one.
  - `JarCookie::for_test(name, value, domain, host_only, path)`,
    `CapturedRequest::for_test(method, url, headers)` and
    `LoggedRequest::for_test(method, host, path, kind, status, mime)` build
    the parts.
  - Values go in as plain strings and come back as `Secret`s, normalized as
    read ones are: the method uppercased, hosts and a cookie domain
    lowercased (its leading dot dropped), header names lowercased and
    sorted, the query and fragment cut. An input no capture could hand over
    (a host that is not a host name, a watched request off `https` or off
    the default port, a logged path without its leading `/`) panics.
- `example-cli`'s confirm hook is a named function, and its unit tests run
  it on a built `Captured` through the feature as a dev-dependency.
- `make test-util-check` (in `make verify` and CI) lints and tests
  `pk-cli-browser` with the feature off and on, on the crate alone, since
  the workspace runs build it with the feature on for example-cli's tests.

## v0.10.0 — 2026-10-30

Additive: every v0.9.0 call compiles and behaves as before; a capture that
asks for none of the new reads makes the same DevTools calls it did.

- **`pk-cli-browser`: the reads `wfr`, `sfi` and `carta` need to drop their
  embedded Chrome capture** (DESIGN.md §2.2).
  - **The whole jar.** `Capture::cookie_jar(domain)` reads every cookie the
    browser would send to `domain` or a host under it (host-only and domain
    cookies on those hosts, a domain cookie on a parent, partitioned cookies
    only in their own first-party site), `HttpOnly` included and at every
    path, matched label for label so `notexample.com` and
    `example.com.evil.example` never count. The domain must be one the
    capture's hosts are on; it may be wider than them, so the login host's
    cookies are read without its page being touched. The capture waits for
    a non-empty jar. `Captured::jar()` hands over each `JarCookie` (name,
    `Secret` value, domain, host-only, path) and `Captured::cookie_header()`
    one `Cookie` header value, longer paths first. A timeout names the
    jar's cookie names, never their values.
  - **The CLI's check before the tab closes.** `Capture::confirm(hook)`
    runs the CLI's own provider read on each poll once everything else is
    ready, before any tab is closed, and acts on its `Confirm`: `NotYet(reason)`
    leaves the tab open and asks again next poll (the timeout names the last
    reason), `Ready` closes the tab and hands the capture over, `Fail(err)`
    ends the wait as `BrowserError::Handler(err)`, after any capture already
    read in that poll is handed over. It works in `wait`, `wait_each` and
    `capture`. The crate only calls the hook; it sends no request itself.
  - **Watching requests.** `Capture::request(url)` waits for the first
    request the capture's pages make to an `https` URL on its hosts (that
    path or one under it, any query) and hands over its method, its URL
    without the query and the headers the page set, values as `Secret`s
    (`Captured::request()`, `CapturedRequest`).
    `Capture::optional_request(url, within)` holds the tab open at most
    `within` for it once the rest is ready, then goes on without it.
    `Capture::request_log(domain, during)` logs each page load, XHR and
    fetch to `domain` once per method, host and path, with the first
    response's status and content type and no header, body or query
    (`Captured::request_log()`, `LoggedRequest`, `RequestKind`), holding the
    tab open `during` once the rest is ready. Watching enables `Network` in
    the capture's own pages only, never a login host's, from a separate
    connection that stays open between polls, detaches from a page that
    leaves those hosts and asks for no request bodies; only requests a
    document on the capture's hosts made count. A window and the confirm
    hook come before the read, so the session handed over is the one read
    in the poll that closes the tab; a capture still in its window at the
    timeout is read once more rather than lost.
  - **The user agent.** `Capture::with_user_agent()` reads the browser's
    `User-Agent` (`Browser.getVersion`, browser-level) into
    `Captured::user_agent()`, for a session bound to the browser that
    earned it.
  - Status lines name a hold (`Example: keeping the tab open up to 25
    seconds for the page to request www.example.com/bff/graphql before
    capturing the session.`); durations under a second read in
    milliseconds. `example-cli`'s browser capture shows a confirm hook.

## v0.9.0 — 2026-10-29

Additive: no existing function, default or `/v1` shape changes.

- **`pk-cli-browser`** (new crate; DESIGN.md §2.2) — `auth login --browser`
  for portals a CLI cannot sign in to by itself, the mechanism `sfi`, `wfr`
  and `carta` each embed and `m1f`, `rhd` and `nlnt` ask the owner to do by
  hand in DevTools. `Launch` starts Chrome
  (`<PREFIX>_CHROME`, else the standard macOS and Linux paths) with only a
  self-chosen debug port, a scratch profile and first-run suppression, and
  waits for `DevToolsActivePort`; the profile is `0700`, created exclusively
  under an unpredictable name, and the returned `Browser` guard stops Chrome
  (SIGTERM, then SIGKILL) and deletes the profile when dropped. A `Capture`
  names the hosts it may be read from (`HostRule::exact`, `HostRule::domain`:
  parsed `https` hosts on the default port, label for label, so confusable
  hosts, credentials in the URL and `http` pages never match) and what to
  read: an expression evaluated in those pages only (it must also report an
  allowed `location.hostname`), and/or cookies read from the browser's jar,
  `HttpOnly` included, chosen as the browser would send them.
  `Browser::wait` (one capture) and `Browser::wait_each` (several providers
  in one window, each handed over as it is read) close every page on a
  capture's hosts the moment it is read, so the web app cannot rotate the
  token it handed over. Values come back as `Secret`s; nothing is logged,
  put on argv or written to disk by the crate. A timeout (10 minutes by
  default) or a closed window is exit 3 naming what was seen; Ctrl-C (a
  process-wide handler, opt-out with `Launch::ctrl_c(false)` plus
  `pk_cli_browser::cancel()`) cancels with cleanup, and a second Ctrl-C
  force-quits. Every call returns a `#[non_exhaustive]` `BrowserError`
  (`Invalid`, `ChromeNotFound`, `Launch`, `Canceled`, `TimedOut`,
  `WindowClosed`, `Handler`), so a CLI can branch on the cause (fall back to
  its `--stdin` path when no Chrome is installed); `?` converts it into
  `CliError` with exit 3 for a timeout or a closed window and 1 otherwise.
  `Launch::spawn` and `Browser::wait_each` are public for the multi-provider
  window (one Chrome signing in to several providers, as a prototype capture
  for `m1f` and `rhd` does); no raw DevTools handle is exported.
  `BrowserArgs` is the flattenable `--browser` flag. The CDP client is
  blocking (tungstenite, no async runtime); tests run offline against an
  in-memory fake browser and a stand-in Chrome script. Needs Rust 1.85.
  `example-cli auth login --browser` shows the flag check and the capture;
  verifying the session and reporting `browser-session` are the real CLI's
  part (DESIGN.md §2.2).
- **`Money::grouped()`**: text display with thousands separators and the
  sign ahead of the symbol, `$1,234.56`, `-$1,234.56`, `1,234.56 EUR`.
  `Display` keeps its ungrouped `$1234.56` (and its `$-5.00` for
  negatives), so existing output and snapshots are unchanged; JSON is
  untouched (the wire amount stays a plain decimal string, SPEC §1.4).
  **`money::group_thousands(&str)`** groups a bare decimal string for
  amounts that are not a `Money`; anything that is not a plain decimal
  (`n/a`, `1e6`, an already-grouped `1,234`) comes back unchanged.
- **Local-date helpers in `pk-cli-core::dates`.** `today()` is UTC, so a
  CLI that stamps a record with it writes tomorrow's date every evening
  west of Greenwich. `today_local()` and `yesterday_local()` give the wall
  calendar's date; `civil_from_unix_local(secs)` converts a timestamp;
  `local_utc_offset(secs)` is the zone's offset at that instant (DST-aware,
  read through `localtime_r`, so it honors `TZ` and the system zone);
  `civil_from_unix_at_offset(secs, offset)` is the pure form for an offset
  that comes from data. Non-Unix targets have no offset source and fall
  back to UTC. `today()` is unchanged and now documents when to use which.
  Adds `libc` (Unix only; bindings, already in the workspace lockfile).
- **`pk-cli-auth::otp`** — the park-and-resume one-time-code login
  every code-login CLI carries its own copy of (a payroll CLI by email; `rpmfl`,
  `pmac`, `sfi`, `rhd`, `m1f` by text message), as one flow.
  `OtpLogin::login` requests a code through the CLI's `OtpTransport`, parks
  the in-flight session in a `ParkingSlot` *before* waiting on anything
  (`KeychainSlot` is one JSON item; a one-item-bundle CLI implements the
  trait over its bundle and checks it with `check_slot_contract`), then reads the code from the mailbox, prompts on a
  TTY, or ends with exit 3 naming `<bin> auth login --code <CODE>`. The resume
  redeems against the parked session, never a new one; a parked login older
  than 20 minutes is exit 3 blaming the wait, not the code. A refused code
  keeps the login parked for a retype, up to 3 refusals, then discards it; a
  provider outage neither counts nor discards. `CodeArgs` (`--code <CODE>`,
  `--code -` for stdin) validates before any keychain read (exit 2).
  `GroMailbox` reads the code through `gro` with nothing secret on its argv,
  only mail received after the request (`after:<unix>`), a bounded poll
  (9 looks, 10 s apart), a 30 s limit per call, and no second call after a
  failure, since `gro`'s keychain read can raise a macOS dialog per call. The
  stored `ParkedLogin` keeps the earlier field names (`rejected` is new and
  defaulted), so adopting it reads the item a CLI already parks. Adopting
  CLIs that exit 0 on "code sent" today change to exit 3.
- **1Password as a credential source in `pk-cli-secrets`** (DESIGN.md
  §1.7). `OpRef` is a validated `op://vault/item/[section/]field[?query]`
  reference that parses from a flag and (de)serializes as a config string.
  `OnePassword::read` resolves one with `op read --no-newline
  [--account <A>] <REF>`: the value comes back over a pipe into a `Secret`,
  never on argv, in logs or on disk; `op`'s stdin is closed; each read is one
  call killed at a timeout (60 s default) and never retried. Not signed in,
  an expired session, a dismissed approval or a timeout is exit 3 naming
  `op signin`; a missing vault, item or field (or an empty value) is exit 4
  naming the reference; another `op` failure is exit 5; no `op` is exit 1.
  `OpArgs` is a flattenable `--op <REF>` flag, read beside
  `SecretSourceArgs` with the new `SecretSourceArgs::read_with_op` (exactly
  one of `--stdin`, `--from-env`, `--op`); a value that is not a
  reference is rejected without being echoed, since it may be the secret
  itself. It is a separate struct, not a
  new `SecretSourceArgs`/`LoginArgs` field, so CLIs that build those by hand
  keep compiling. `SecretResolver` resolves a `SecretSpec` (keychain
  account, optional env var, optional reference) across env, keychain and
  1Password in a `SourceOrder`: default `env,keychain,op`, configurable from
  a `secret_sources` string; a missing source falls through, a failing one
  stops the walk, and an empty value from any source is an error (an empty
  keychain item is exit 3 naming it). The keychain side is the public
  `KeychainRead` trait (implemented for `CredentialStore`), so a CLI's tests
  can run its resolver over an in-memory store. `SourceKind`, `Resolved` and
  `OpArgs` are `#[non_exhaustive]`; build `OpArgs` with
  `OpArgs::reference`. `read_from_env` now reports a non-UTF-8 variable as
  such instead of "not set". `example-cli` shows `auth login --op`, `config set op_ref`
  / `secret_sources`, and a resolver read in `summary`. Tests use a fake
  `op` script; nothing calls a real `op`.
- **`pk-cli-drive`** (new crate; DESIGN.md §2.1) — state in the owner's
  Drive, extracted from tax-cli. `Backend` is one interface over a mounted
  folder (`Mount`), rclone straight through (`Remote`) or rclone behind the
  read cache (`Cached`): read/write small UTF-8 files, copy binary files in
  and out, move (with a no-clobber variant that refuses an occupied
  destination, exit 2), list recursively with sizes and modification times.
  `Remote` maps rclone's exit codes 3/4 to "missing" and everything else to
  exit 5, falls back to a sizes-only listing on an rclone older than 1.63,
  stages uploads in an exclusive owner-only temp file, and can be bounded
  (`with_budget`), pointed at another program (`program`, which `version()`
  uses too) and given the CLI's temp-name prefix (`temp_prefix`).
  `CachedRemote` is the stale-while-revalidate read cache: write-through,
  fresh/stale/expired tiers (900 s / 24 h defaults), a generation counter so
  a refresh never undoes a write, a refusal (exit 1,
  `is_stale_write_refusal`) when a write would derive from a stale copy that
  has since changed, fail-open on cache or remote errors, and a bounded
  detached background refresh (`SpawnRevalidator`, `background_revalidator`,
  `run_revalidation`) deduplicated by a per-entry marker. One `CachePolicy`
  carries every knob to the cache, the spawner and the child;
  `CachePolicy::from_env` reads `<BIN>_NO_CACHE` and
  `<BIN>_CACHE_{TTL,MAX_STALE,REVALIDATE_TIMEOUT,NO_REVALIDATE}`. The cache
  lives under `$XDG_CACHE_HOME/<bin>/`. `private_file::write` is the
  owner-only (0600) writer, exclusive or atomic-replace. The crate needs Rust
  1.89 (`File::lock`). Tests use an in-memory remote and a stub `rclone`
  script; nothing calls a real rclone. `example-cli` gains `state
  get|put|list|sync` and the `data_root` / `remote` config keys, and with
  them a 1.89 floor.
- **`make verify`**: the format check, clippy and tests as one local
  command. CI's `check` job now runs the same `Makefile` targets, so the gate
  has one definition.
- `Display for Money` and `output::scalar`'s money rendering now share one
  private renderer with `grouped()`. Output is byte-identical.
- **`scripts/self-view.sh`**: lets a builder see and drive the desktop app
  it is building, and nothing else. `launch` starts the app and records its
  PID and start time in a pidfile. `shot`, `bounds` and `drive` act only on
  a window owned by that process or a descendant, never one matched by name.
  They refuse when the pidfile is missing, the PID was recycled, the process
  exited, no window is tied to it, or the screen is locked. `drive` also
  refuses when another app's window covers the target point, and reports
  success only once the pointer reads back at the target. `web` renders a
  loopback dev server in a headless browser with a throwaway profile, and
  only when the port's listeners belong to the launched tree. `stop` ends
  the tree. Refusals use the family exit codes (3 permission, 4 nothing to
  act on, 5 environment; a safety refusal is the generic 1). The macOS effects are small Swift
  helpers in `scripts/lib/`, each behind a `SELF_VIEW_*` test seam, so the
  tests run on Linux; `make scripts-check` runs them with shellcheck, as
  part of `make verify` and CI, and the macOS CI job type-checks the
  helpers and holds the window lister to the line format the tests stub.

## v0.8.0 — 2026-09-10

Mechanisms proven this week in `google-home-cli` (and re-implemented by hand
in `govee-cli`, `tplink-cloud-cli` and `lofty-cli` before being lifted here),
plus the first documented-only domain profile. Additive: no existing API or
`/v1` shape changes.

- **`pk-cli-core::confirm`** — SPEC §1.3's mutation gate as functions.
  `require_confirmable(force, interactive, what)` is exit 6 when a mutation
  can neither prompt nor was forced; `confirm(force, prompt)` is the stderr
  `[y/N]` (a refusal is the same `ConfirmationRequired`); `gate(&CommonArgs,
  force, prompt)` is both in one call. The split exists for an ordering rule
  the module documents: decide *whether a prompt is possible* **before** any
  keychain or network work, and prompt with the resolved names after the
  cheap reads that produce them — so a driver that forgot `--force` gets its
  exit 6 without a macOS keychain prompt, a session mint, or a request.
- **`pk-cli-core::resolve::pick`** — the one reference ladder for
  `get <REF>` / `move <REF>`: exact name, exact id (case-insensitive),
  case-insensitive name, unique partial name, in that order. More than one
  hit at any tier — ids included — is `NotFound` naming the candidates; a
  silent first pick never happens (the copies this replaces disagreed
  exactly there, one of them the argument of a write). An empty query is a
  usage error rather than a substring that matches everything.
- **`pk-cli-core::output::{emit_list, emit_one, tagged}`** — `emit_list`
  emits the same `<record>-list/v1` envelope `Paged` does, for the many
  lists that are *not* a profile's and carry no paging (`devices list`,
  `rooms list`); text mode is a `columns`-projected pipe table with a stderr
  `(no <record>s)` when empty. `emit_one` is the single-resource counterpart
  (key/value block in text). `tagged(schema, payload)` is the value `emit`
  prints, exposed so the envelope is a value a CLI can write elsewhere.
- **`pk-cli-secrets`: one keychain item per credential set.**
  `CredentialStore::get_json<T>` / `set_json<T>` read and write a typed JSON
  item; an item that is present but unparseable is an **error naming the
  account**, never silently "absent" — one blob now carries the whole
  session, so a swallowed parse failure would read as an unexplained logout
  and the next `auth login` would overwrite whatever it held.
  `CredentialStore::migrate_from(&legacy, &[(old, new)])` moves items
  between services for the family-wide `piekstra.<bin>` rename: per pair,
  read old → write new → delete old (a failure midway leaves the legacy item
  for the next run), an existing destination wins and the legacy copy is
  still retired so the migration converges and is safe to call on every
  start; returns how many were copied. The hygiene fact behind the rule: on
  macOS every keychain item a freshly built binary reads is a permission
  prompt, so an eight-item session is eight prompts per rebuild. The item
  logic is written against a private seam and tested in memory; no test
  reads the OS keychain.
- **`smart-home/v1` profile, documented — `device-rooms/v1`** (DESIGN.md
  §1.8, PROFILES.md, conformance.md). The vendor CLIs' "every device with
  the room our app files it under" shape — `id`, `name`, `room`, `source`,
  optional `cloud` (false = the vendor cannot expose the device to an
  assistant) and `connectivity` — produced by `govee rooms devices` and
  `tplc groups devices`, consumed by `ghome audit --expect -` joining on `id`
  (punctuation/case-insensitive) then `name`. Meets the PROFILES.md bar
  (two producers, a real consumer, stable shape) but ships **without a
  crate**: the DTO only crosses a process boundary as JSON, so a Rust type
  would have no `use` sites — the "DTOs nobody consumes" failure. PROFILES.md
  gains a "Documented-only profiles" rule for exactly this case.
- **Spec and checklist additions** — SPEC §1.3: the confirmation decision
  comes before any keychain/network work, and a mutation's success comes
  from a **read-back**, never the write's status code (provider write
  responses are routinely empty or ahead of their read side). SPEC §1.7:
  one keychain item per credential set. Both are conformance.md checklist
  lines and AGENTS.md rules. conformance.md also gains `ghome` (conforms,
  v0.4.x / cli-common v0.7.0), moves govee-cli and tplink-cloud-cli to
  "migrating — spec-v1 PR open", and documents the family CI gate
  (ubuntu-latest fmt/clippy/test/smoke + targeted macOS smoke + a
  cargo-audit/gitleaks security job, matching google-home-cli).
- **`example-cli`** — a plain `devices` noun (`list`, `get <REF>`,
  `rename <REF> <NAME> [--force]`) demonstrating `emit_list`/`emit_one`,
  `resolve::pick`, and the two-step `confirm` gate in the order the spec
  requires.

## v0.7.0 — 2026-08-18

Partial-failure reporting for `documents download --all`. Additive on the wire:
`DownloadBatch::new` callers and JSON consumers are unchanged. (Rust-API caveat:
`DownloadBatch` gains a field, so any code that built it by struct literal must
add `skipped` or move to the constructor — none in-tree does.)

- **`pk_cli_documents::DownloadBatch` now carries `skipped`** — a
  `Vec<SkippedDocument>` of listed documents that a `--all` run couldn't produce
  a file for (e.g. the provider's archive has no PDF on record).
  `document-download-batch/v1` previously had **no** place to report this, so a
  partial batch reported only `count`/`bytes_total` and an adopter could
  silently under-report — "3 of 5 downloaded" was indistinguishable from
  "3 exist". Surfaced while adopting the profile in `loxahatchee-cli`, whose
  sibling `bills download --all` already reported skips; the profile now lets
  every adopter (including `tojfl`, which had the same gap) do the same.
  - New **`SkippedDocument`** — `id`, a human `reason`, and an optional
    machine-branchable **`code`** slug (`no_file` / `verify_failed` / `upstream`,
    omitted when unclassified) so a consumer can distinguish a benign permanent
    skip from a retryable one without matching on prose (mirrors
    `CliError`'s `code()`/message split, SPEC §1.4). `SkippedDocument::new(id,
    reason)` + a `with_code(code)` builder step.
  - **`DownloadBatch::new(dir, items).with_skipped(skips)`** — `with_skipped` is
    a consuming builder step (house `with_*` idiom), so `new` is unchanged and
    the next optional field won't force another constructor.
  - Both new fields are `#[serde(default, skip_serializing_if = …)]`, so a
    fully-successful batch serializes to the exact same shape as before and
    older payloads still deserialize. This additive-within-`/v1` change is now
    codified as a carve-out in `AGENTS.md`.

## v0.6.0 — 2026-08-13

Download verification and filename hygiene move to their correct home.
Additive: no existing API changes.

- **`pk_cli_documents::verify`** — mechanism extracted from `robinhood-cli`,
  where both pieces were paid for with live findings:
  - `verify_download(bytes, filetype)` / `verify_pdf(bytes)` — a download is
    not a document until it matches its **declared filetype**. `%PDF` magic
    for PDFs; for text types (`csv`, `html`, …), rejection of the failure
    shapes an expired pre-signed link actually serves (HTML login page, S3
    XML error, any JSON error object) plus a UTF-8 head check that tolerates a
    multi-byte character cut by the inspection window. Genesis: a brokerage's
    consolidated-1099 season ships a PDF *and* a CSV export, and an
    unconditional `%PDF` rule refused the CSV — correctly, which is what
    surfaced the provider's `filetype` field.
  - `fs_safe(s)` — reduce any provider-controlled string that reaches a
    filename (type, filetype, date, id) to path-inert characters. Three
    separate unsanitized sinks were found in one file during review; the rule
    is now "every component, one function".

## v0.5.0 — 2026-08-12

The `documents/v1` domain profile (SPEC §1.8), and the list primitives it
shares with `utility/v1` move to their correct home. Additive: existing
`pk_cli_utility::{Paged, RangeArgs}` imports keep compiling unchanged.

- **`pk-cli-documents`** (new crate) — the `documents/v1` profile: one spelling
  and one shape for "list the files a portal published, and download them",
  which every portal grew its own name for (`pmac documents list|download`,
  `fpl bills download -o`, `tojfl bills get -o`, `lrfl bill --save`, `wabhoa
  statements list` with no download at all). DTOs: `Document`
  (`document/v1` — `id`/`name` + optional `date`/`category`/`file`, and
  deliberately **no** financial fields; a statement's amount is `utility/v1`'s
  concern), `SavedDocument` (`document-download/v1`), `DownloadBatch`
  (`document-download-batch/v1`), `OpenedDocument` (`document-open/v1`), over
  `Paged<T>`. The shapes are promoted verbatim from what `pmac` already ships,
  so it is a rename-to-canonical, not a new invention. Commands are all reads
  (§1.3 confirmation does not apply). Consumer: the `organize-scans` archiver's
  per-CLI download-command table is designed to collapse to `<cli> documents
  list --json` + `<cli> documents download <id> -o <path>` once each CLI adopts
  the profile. That adoption is a **release-window migration, not part of this
  change** — the CLIs pin `cli-common` by tag, so they adopt after v0.5.0 tags;
  `example-cli` demonstrates the profile in-repo here. Sequencing and status
  tracked in issue #8 (PROFILES.md step 7 / bar condition 2).
- **`pk-cli-core::list`: `Paged<T>`, `RangeArgs`** — moved here from
  `pk-cli-utility`. They are profile-*agnostic* (a list is a list whatever the
  records), so a second profile would otherwise have duplicated them, and a
  non-utility CLI like `pmac` was already depending on `pk-cli-utility` purely
  to page a list — the smell PROFILES.md names ("a DTO generic to all CLIs
  belongs in `pk-cli-core`, not a profile"). `pk-cli-utility` re-exports both,
  so nothing downstream changes; new adopters use `pk_cli_core::{Paged,
  RangeArgs}` (or a profile crate's re-export).
- **`example-cli`** — demonstrates both profiles now: `documents list` emits
  `document-list/v1`, and `info` advertises `["utility/v1", "documents/v1"]`.

## v0.4.0 — 2026-08-10

Scraped-portal support. `wabhoa` (`piekstra/westernalliancebank-hoa-cli`, a
Western Alliance Bank HOA assessment portal) is the first family CLI whose
provider answers mostly in rendered HTML rather than JSON. Each addition was
written once privately and is generic enough that the next scraped portal
shouldn't rewrite it.

- **`pk-cli-scrape`** (new crate) — read values out of server-rendered pages
  without a DOM parser: `elements`, `attr`, `block_by_id`, `input_value`,
  `table_rows`, `blocks_with_class`, `cells`, `cells_with_class`,
  `strip_tags`, `decode_entities`. **No dependencies.** Every function is
  total — malformed markup yields `None` or an empty list, never a panic — so
  a provider redesign surfaces as an empty table rather than a crash. It is
  not a general-purpose parser and does not try to be: no selector engine, no
  document tree. *Which* IDs and class names to look for stays with the CLI
  that owns the provider; this crate only knows how to look.
- **`pk-cli-auth::reauth::with_reauth`** — recover from a lapsed session
  without making the user re-run the command: run an operation, and on
  `CliError::Auth` re-authenticate once through a caller-supplied closure and
  retry once. The rails are the point, and they are identical for every CLI:
  only `Auth` triggers recovery (upstream/not-found/usage errors are returned
  untouched), recovery runs **at most once** so a broken provider can't become
  a login storm, a failed login reports *its own* error rather than the stale
  "session expired", and a second failure surfaces as itself. A CLI that can't
  log in unattended — an interactive second factor, say — returns an error
  from the closure and keeps plain exit-3 behaviour. The operation may run
  twice, so this is for reads; the doc says so explicitly.
- **`pk-cli-core::dates`: `parse_dotnet`, `DOTNET_MIN`, `is_dotnet_min`,
  `parse_mm_slash_dd_yyyy`** — ASP.NET / ServiceStack `/Date(millis±hhmm)/`
  timestamps, and the inverse of `fmt_mm_slash_dd_yyyy` for reading dates back
  off a rendered page. The trailing offset is deliberately ignored: it
  re-expresses an instant the milliseconds already carry in UTC, so honouring
  it shifts evening timestamps back a day. `DateTime.MinValue` **parses**
  rather than vanishing — classifying it as "absent" is left to the caller,
  because providers layer their own placeholder dates (1900-01-01 is common)
  that only the caller knows about.
- **`pk-cli-core::output`: `emit`, `table_view`, `rows_of`** — §1.4's output
  contract as functions. These had been copy-pasted verbatim into each portal
  CLI. `emit` tags a DTO with `schema` and flattens an object payload
  *alongside* the tag (so consumers read `.payments`, not `.data.payments`),
  `table_view` projects columns with omit-don't-null, and `rows_of` reads an
  array field back out for the text renderer.

**Behaviour change:** `serde_json`'s `preserve_order` feature is now enabled in
this workspace, so JSON object keys and table columns follow **insertion
order** instead of being alphabetized. `table_view` exists to choose and order
columns, and `kv` renders DTO fields in the order the code builds them —
neither contract was real while ordering depended on whether each downstream
CLI happened to set the flag. Cargo features are additive, so this reaches
every consumer. Values, field names, and schema tags are unchanged; only
ordering is, which affects rendered text and JSON key order (JSON objects are
unordered by definition, so conforming consumers are unaffected).

## v0.3.0 — 2026-08-10

Bearer-token support. `cpmfl`
(`piekstra/campbell-property-management-hoa-cli`, a Vantaca HOA portal) is the
first family CLI whose session is a JWT rather than a cookie, and both
additions below came out of building it — each was written once privately and
is generic enough that the next token-auth CLI shouldn't rewrite it.

- **`pk-cli-auth::token`** — read the claims of a cached bearer token so a CLI
  can answer "is this session still usable?" without spending a request:
  `claims`, `numeric_claim`, `expiry`, `expires_at` (RFC 3339, ready for
  `auth-status/v1`'s existing but until-now unpopulated `expires_at` field),
  and `is_expired(token, now, skew)`. Verifies **nothing** — no signature,
  issuer, or audience check; a CLI is the bearer, not the validator, and the
  server stays the only authority. Accordingly, a token whose claims can't be
  read is reported as *not* expired, so an unfamiliar shape still reaches the
  server rather than locking a user out of a good session. The clock skew is a
  caller argument rather than a house rule; `DEFAULT_SKEW_SECS` (60) is offered
  as a starting point.
- **`pk-cli-core`: `Money::from_cents`** — build `Money` from minor units with
  integer arithmetic (no float touches the value). Providers that report an
  integer number of cents are common, and some mix scales across endpoints —
  the same transaction arriving as `25000` from one and `250.00` from another —
  where an open-coded `/100` is a silent 100× error waiting to happen.
- **`pk-cli-core::dates`: `fmt_rfc3339` and `civil_from_unix`** — a Unix
  timestamp as RFC 3339 UTC, reusing the existing Hinnant conversion so the
  family still needs no calendar crate. Correct for pre-epoch timestamps.

## v0.2.1 — 2026-07-30

- `pk-cli-selfupdate`: private-repo support. When `GITHUB_TOKEN` (then
  `GH_TOKEN`; first non-empty wins, no other source) is exported, the
  `releases/latest` lookup is sent with `Authorization: Bearer <token>` and
  assets are downloaded through the GitHub API asset endpoint
  (`/repos/{repo}/releases/assets/{id}` with `Accept:
  application/octet-stream`) instead of `browser_download_url`, which does
  not accept a bare token. With no token, behavior is unchanged (public
  repos unaffected). A 404 on `releases/latest` without a token now
  mentions that the repo may be private, in addition to "no releases yet".
  schwopts (`piekstra/schwab-options-cli`) is the first private family CLI
  to need this.

## v0.2.0 — 2026-07-19

Domain profiles (SPEC v1.1 §1.8): an optional second layer over the surface
spec — canonical spellings + shared DTOs per domain, declared via `info`.
`PROFILES.md` documents when a domain boundary earns a profile crate and how
to add one.

- **New crate `pk-cli-utility`** — the `utility/v1` profile for account-portal
  CLIs (fpl, tojfl, lrfl, xfin): `UtilitySummary` (`utility-summary/v1`,
  emitted by both `summary` and `balance`), `Statement`, `Payment`,
  `UsagePeriod`, `Transaction`, the `Paged<T>` list envelope
  (`<record>-list/v1`, records under `items`), and `RangeArgs`
  (`--limit`/`--since`/`--until` with ISO validation). Replaces the per-driver
  domain glue (utiman's `balance-fields`/`scale`/`items-path` manifest keys).
- `pk-cli-core`: `cli-info/v1` gains an optional `profiles` field
  (`CliInfo::with_profiles`) — additive; omitted when empty, so existing
  consumers are unaffected.
- `pk-cli-core`: the text renderer now displays `Money` objects as `$12.34`
  (or `12.34 EUR`) in key/value blocks and table cells instead of raw JSON.
- `example-cli`: demonstrates the profile (`summary`, `balance`,
  `bills list [--limit/--since/--until]`, profile declaration in `info`).
- `conformance.md`: family table updated from the 2026-07-19 audit (adds
  lofty + discord, drift notes for the utility four, pre-spec adoption notes);
  new profile-tracking table.

## v0.1.3 — 2026-07-11

- `pk-cli-selfupdate`: on macOS, re-sign the downloaded binary with the stable
  `pk-cli-codesign` identity before installing it, so a prior keychain "Always
  Allow" grant keeps applying across self-updates (no re-prompt on a new
  version). Best-effort — a silent no-op when the identity isn't present.

## v0.1.0 — 2026-07-11

Initial release: SPEC v1 (`DESIGN.md`) and the first extraction of shared
code from `fpl`/`xfin`.

- `pk-cli-core`: `CliError` with stable exit codes 0–6 and `--json` error
  envelope; text/JSON output renderer; `CommonArgs` global flags; date
  helpers (ISO + provider formats); `Money` (string-decimal + currency);
  `cli-info/v1` DTO.
- `pk-cli-secrets`: redacting/zeroizing `Secret`, keychain `CredentialStore`
  with the `piekstra.<bin>` service convention, `--stdin`/`--from-env`
  ingestion via `SecretSourceArgs`.
- `pk-cli-config`: XDG-located JSON `ConfigStore` with typed load/save,
  `--config` override, `clear` for `logout --forget`.
- `pk-cli-selfupdate`: parameterized GitHub-release `Updater` with `--check`,
  atomic in-place replace, `self-update/v1` DTO.
- `pk-cli-auth`: `auth-status/v1` DTO + standard `LoginArgs`,
  `SetCredentialArgs`, `LogoutArgs`.
- `pk-cli-http`: family-default blocking client builder, `ApiArgs`
  passthrough, response→exit-code mapping.
- `example-cli`: runnable template exercising the full surface.

## v0.1.1 — 2026-07-11

- pk-cli-core: optional `reqwest` feature adding `From<reqwest::Error> for CliError` (→ Upstream, exit 5).

## v0.1.2 — 2026-07-11

- pk-cli-selfupdate: `Updater` fields are owned `String`s; added `os_arch()` for `<os>-<arch>` release-asset naming.
