# Google Drive through its own API (feature `google`)

`pk_cli_drive::google` is a store in one Google account's Drive, reached
through the Drive API v3 with an OAuth client the owner registers. It sits
behind the same interface as the rclone `Remote`
(`RemoteRead` + `RemoteStore`), so `CachedRemote` and `Backend` work over it
unchanged (`Backend::Drive`, `Backend::CachedDrive`).

It replaces rclone for Google Drive. rclone's shared Google sign-in client is
being retired, so every Drive writer needs a client of its own, and rclone
keeps its tokens in plain text in `rclone.conf`. Here the OAuth client and the
tokens are vault entries under the CLI's one keychain item. The rclone
backend stays for other providers, and for Drive until its last user moves.

## Registering the OAuth client

One client serves every profile and every machine of one owner.

1. In the owner's Google Cloud project, enable the Google Drive API.
2. Configure the OAuth consent screen (Google Auth Platform → Branding,
   Audience, Data Access). Under Data Access, add the scopes the CLI asks for
   (`.../auth/drive.file` unless the CLI's docs say otherwise). With the app
   *In production* and unverified, Google shows an "unverified app" warning at
   consent and caps the app at 100 users. A refresh token issued to an app
   left *In testing* expires after 7 days.
3. Create a client: Clients → Create client → Application type **Desktop
   app**. Download its JSON.
4. Import it: `<cli> drive client import --profile <NAME> --file <PATH>`
   (or `--stdin`), or pass `--client-file` to the first `login`. The JSON
   goes into the vault and the downloaded file can be deleted. Only an
   `installed` (Desktop app) client is accepted; a Web client's JSON is
   refused, since its secret would be confidential and a loopback redirect is
   not registered for it.

The client JSON's `auth_uri` and `token_uri` are ignored: the endpoints are
Google's, compiled in, so a planted file cannot send the client secret
elsewhere. Google treats a Desktop client's secret as not confidential (it
ships inside every installed app); PKCE (S256) protects the code exchange.

## Profiles

A Drive profile is one Google account. There is no default profile.

| What | Where |
|---|---|
| The account's email, as Drive's `about` named it at sign-in | `drive.json` beside the CLI's config |
| The scopes Google granted, the client id, when it signed in | `drive.json` |
| The OAuth client (id and secret) | vault entry `drive/<profile>/client` |
| The token set (access and refresh) | vault entry `drive/<profile>/token` |

`drive.json` is owner-only and holds nothing secret. A profile's account is
fixed by its first sign-in: a later sign-in that lands on another account is
refused, and nothing is stored. `login --account <EMAIL>` sets it in
advance and sends it to Google as `login_hint`.

## Mounting the commands

```rust,ignore
use pk_cli_drive::google::{DriveArgs, DriveCli, DriveCommand, Scope};

// A CLI with no `--profile` of its own: `<bin> drive --profile <NAME> …`.
#[derive(clap::Subcommand)]
enum Command {
    /// Google Drive sign-in.
    Drive(DriveArgs),
}

fn run(args: &DriveArgs, common: &pk_cli_core::CommonArgs) -> Result<(), pk_cli_core::CliError> {
    DriveCli::new(BIN, VERSION, "mycli drive")?
        .default_scopes(&[Scope::File])
        .run(&args.command, args.profile.as_deref(), common.json, common.interactive())
}
```

A CLI whose global `--profile` means something else (medrec's records
profiles) mounts `DriveCommand` itself and passes the Drive profile it
resolved (from its settings) to `run`, and names its own flag for remedies
with `DriveCli::profile_flag("--drive-profile")`. The environment's fallback
is `<PREFIX>_DRIVE_PROFILE`.

| Command | Does | Output |
|---|---|---|
| `login --profile <NAME>` | Opens the browser to Google's consent (PKCE, loopback redirect on 127.0.0.1), checks a refresh token came back and every scope asked for was granted, reads the account's email from Drive, then stores the client, the tokens and the profile together. `--no-browser` prints the address; `--wait <MINUTES>` (default 5) | `auth-login/v1` plus `profile`, `username`, `scopes` |
| `status [--local]` | The stored sign-in, and one `about` call to Drive unless `--local` (which reads no keychain) | `auth-status/v1` plus `profile`, `scopes`, `client`, `signed_in_at` |
| `logout [--revoke] [--forget]` | Removes the token set. `--revoke` also revokes the grant at Google, which ends every sign-in of that account through that client, on every machine. `--forget` removes the profile and its client too | `drive-logout/v1` |
| `about` | The account's email and storage quota (replaces `rclone about`) | `drive-about/v1` |
| `client import --file <PATH> \| --stdin [--overwrite]` | Stores a Desktop client's JSON. Replacing a different client needs `--overwrite` and drops the profile's sign-in | `drive-client/v1` |
| `folder create <NAME>` | Makes a folder at the top of My Drive for a store and prints its id and spec; one of that name already there (that this sign-in can see) is reused, two are an error. Under `drive.file` a store must be a folder the app's client made | `drive-folder/v1` |
| `folder list` | The folders at the top of My Drive this sign-in can see | `drive-folder-list/v1` |
| `profiles list` | The profiles, with account, scopes and whether signed in. Reads no keychain | `profile-list/v1` |
| `profiles remove <NAME> [--force]` | Removes the sign-in, the client and the record; asks first, exit 6 when nobody can answer | `drive-profile-removed/v1` |

A store opened on a profile that is not configured is exit 2 naming
`<cli> drive login --profile <NAME>`. A profile with no client is exit 2
naming `client import`. A refused refresh (revoked, or unused for six months)
is exit 3 naming the sign-in.

## The store

`DriveCli::drive(&DriveSpec)` opens the folder a spec names:
`gdrive:<profile>/<folder-id>`. Nothing is read until the first operation,
and the folder itself is checked once: an id Drive does not know, a file, or
a folder in the trash is exit 4. Drive lists an unknown id's children as
nothing, so without that check a wrong id would read as an empty store.

| Behaviour | How |
|---|---|
| Paths | Store-relative, `/`-separated; `.`, `..` and empty parts are exit 2. Each part resolves to an id through a map cached for the life of the `Drive` value; a listing fills it in bulk |
| Two items of one name | Drive allows it. A path that names two is an error naming both ids (reason `duplicate_name`), never a guess, and nothing is changed |
| Listings | Size, modified time and md5 (`FileEntry::md5`). A whole tree is one query per level of folders, up to 40 folders a query. Google's own documents (Docs, Sheets) and shortcuts are left out |
| Writes | An existing file's content is replaced in place (`files.update`), keeping its id; never a second file of the same name. Every upload is checked against the md5 Drive computes |
| Large files | Above 5 MiB, a resumable session in 8 MiB pieces, resumed from what Drive reports it holds. Downloads above 16 MiB are read in ranges. Every download is checked against Drive's md5 and written through an owner-only temporary file |
| Tree copies | `put_tree` skips every file whose md5 already matches. `Existing::Keep` refuses the whole copy, before uploading anything, when a file there holds other bytes; `Existing::Replace` replaces it in place. `fetch_tree` downloads what differs |
| Deletes | To Drive's trash (recoverable for 30 days) |
| Moves | One `files.update` with `addParents`/`removeParents`. A clobbering move trashes the destination first; `move_no_clobber` refuses an occupied one with exit 2 |
| Retries | 429, a 403 with reason `rateLimitExceeded` or `userRateLimitExceeded`, and 5xx, with exponential backoff and jitter (1 s doubling to 32 s, 7 attempts), honouring `Retry-After`. A 5xx or lost answer to a create is not blindly retried: the folder is looked up again, or a new file found by name and md5, so a create is never made twice |
| Tokens | Renewed under the CLI's refresh lock, so two processes never spend one refresh twice; a 401 renews once and retries |
| Bounds | `with_budget` caps every request of the store to the time left, as the cache's background refresh needs |

## Scope: `drive.file` or full `drive`

A CLI that moves off rclone has files in Drive that rclone's client (or the
owner, in the Drive web app) made. Under `drive.file` a new client cannot
see them. The question this section settles is whether two clients of the
owner's one Cloud project share `drive.file` access, so that files one makes
are visible to the other.

### What Google says

From [Choose Google Drive API scopes](https://developers.google.com/workspace/drive/api/guides/api-specific-auth):

- `drive.file`: "Create new Drive files, or modify existing files, that you
  open with an app or that the user shares with an app while using the
  Google Picker API or the app's file picker."
- "The `drive.file` OAuth scope lets users choose which files they want to
  share with your app."
- "Using non-sensitive scopes, such as `drive.file`, grants per-file and
  narrow access to specific features needed by an app."
- "Since `drive.file` is non-sensitive, it allows for a more streamlined
  verification process."
- `drive`, `drive.readonly`, `drive.metadata` and `drive.metadata.readonly`
  are listed under "Restricted scopes", which "require restricted scope"
  verification; "If you store restricted scope data on servers (or
  transmit), then you must go through a security assessment."

From [Sensitive and restricted scope requirements](https://support.google.com/cloud/answer/13464321):

- "You must only request the **narrowest scope(s)** your app needs to
  function".

None of Google's pages says whether "the app" is one OAuth client or the
Cloud project. Hence the test below.

### The test

On a synthetic folder in an account that holds no records, with two clients
of one Cloud project, each signed in with `drive.file` alone (the granted
scope confirmed through Google's `tokeninfo`):

1. The **Desktop app** client (PKCE, loopback redirect) creates a folder and
   a file in it.
2. A **third app** (a Drive connector outside the project) creates a control
   file in the same folder.
3. The project's **Web** client (a browser sign-in on a registered
   localhost origin) reads the folder's listing, gets the Desktop client's
   file and its bytes, and creates a file of its own there.
4. The Desktop client gets and reads the Web client's file.
5. Both clients try to get the control file.
6. Everything is deleted and both grants revoked.

## Tests

`google::fake::FakeDrive` (feature `fake-google`) is a Drive API server on
127.0.0.1 holding synthetic files in memory. It serves the slice of the API
the backend uses: `files.list` with the query forms the backend sends, paged;
metadata; `alt=media` with `Range`; folder creates; multipart, media and
resumable uploads; `PATCH` for rename, move and trash; `about`; the token
endpoint (authorization code with PKCE, and refresh); revoke; and an
authorization endpoint that approves at once. Faults can be queued for the
next matching requests, answered before the request takes effect or after
it, which is how retries and lost answers are tested. Its client secret is
the synthetic one in `tests/fixtures/google/desktop-client.json`.

A CLI's own black-box tests point its binary at a fake with
`<PREFIX>_DRIVE_TEST_ENDPOINT=http://127.0.0.1:<port>` (anything but plain
http to 127.0.0.1 or localhost is refused) and at an empty keychain service
with `<PREFIX>_KEYCHAIN_SERVICE`, so nothing reads the real keychain or
reaches Google. In-process tests give `DriveCli::with_vault` a vault over
`MemoryKeys`.
