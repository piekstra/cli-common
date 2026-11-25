# Google Drive through its own API (feature `google`)

`pk_cli_drive::google` is a store in one Google account's Drive, reached
through the Drive API v3 with an OAuth client the owner registers. It sits
behind the same interface as the rclone `Remote`
(`RemoteRead` + `RemoteStore`), so `CachedRemote` and `Backend` work over it
unchanged (`Backend::Drive`, `Backend::CachedDrive`).

It replaces rclone for Google Drive. rclone's shared Google sign-in client is
being retired, so every Drive writer needs a client of its own, and rclone
keeps its tokens in plain text in `rclone.conf`. Here the OAuth client and the
tokens are vault entries (`pk_cli_auth::vault`): owner-only files under
`~/.config/<bin>/vault/`, each sealed with a key that only the CLI's one
keychain item holds. The rclone
backend stays for other providers, and for Drive until its last user moves.

## Registering the OAuth client

One client serves every profile and every machine of one owner. Google
keeps one grant per account for the whole Cloud project, not per client:
under `drive.file`, files one client of the project makes are visible to
its others (another CLI's Desktop client, a web app's Web client), and a
revoke through any of them ends them all ([Scope](#scope-drivefile-or-full-drive)).

A Cloud project published In production needs an application home page and a
privacy policy link on its Branding page before Google enables "Publish app".
The repository's [PRIVACY.md](../../../PRIVACY.md) is written for that link: it
covers what a CLI on this crate reads from Google and where it keeps it.

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
5. For a second account through the same client, copy it from a profile that
   has it: `<cli> drive client import --profile <NEW> --from-profile <NAME>`.
   The client stays sealed in the vault, so no file is needed once the first
   import deleted it. Under `drive.file` a folder is visible to every client of
   the Cloud project that made it, so a second sign-in that must see what a web
   app of the same project made needs that project's client, not another one.

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
| The OAuth client (id and secret) | vault entry `drive/<profile>/client`: a sealed file under `~/.config/<bin>/vault/` |
| The token set (access and refresh) | vault entry `drive/<profile>/token`, sealed the same way |

`drive.json` is owner-only and holds nothing secret. A profile's account is
fixed by its first sign-in: a later sign-in that lands on another account is
refused, and nothing is stored. `login --account <EMAIL>` sets it in
advance and sends it to Google as `login_hint`.

A vault reads its key from the keychain once and keeps it. On macOS, each
newly built binary asks once before it may read the item. "Always Allow"
lasts for that binary, while "Allow" covers one read. An upgrade is a new
binary too, even signed with the same identity: without an Apple team ID
the item's partition list names each build's code hash. So every install of
a rebuilt CLI costs the owner one "Always Allow" per item, and until it is
answered an unattended run waits on the dialog, or gets `KeyLoad::Waiting`
from a bounded read (below). A process that builds
several `DriveCli` values (a long-running server, a run that opens its store
more than once) shares one vault, so it reads the key once: keep
`Arc::new(DriveCli::default_vault(bin)?)` for the process and give each
`DriveCli` a clone with `with_vault`. To bound the wait on a dialog nobody
answers, call `load_key(wait)` on that vault before the first sign-in is
read. It reads on a thread of its own and holds none of the vault's locks,
so an unanswered dialog is `KeyLoad::Waiting`. Until it is answered, the
vault's reads and writes that need the key fail at once with exit 3. A
process nobody may be watching (a server, a scheduled run) also builds its
vault with `bound_key_reads(wait)`. That holds every later read of the key
to the same bound: the read again after another process replaced the key
(a reset and a new sign-in in a terminal), and a write's. Without it such a
read waits on the dialog holding the vault's key lock, and every other user
of the shared vault waits behind it.

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
| `logout [--revoke] [--forget]` | Removes the token set. `--revoke` also revokes the grant at Google, which ends every sign-in of that account through any client of the Cloud project (a web app's too), on every machine. `--forget` removes the profile and its client too | `drive-logout/v1` |
| `about` | The account's email and storage quota (replaces `rclone about`) | `drive-about/v1` |
| `client import --file <PATH> \| --stdin \| --from-profile <NAME> [--overwrite]` | Stores a Desktop client's JSON (`--client-file`, `--client-stdin` as on `login` work too), or copies the client another profile holds, without its sign-in (`from_profile` in the output). Replacing a different client needs `--overwrite` and drops the profile's sign-in | `drive-client/v1` |
| `folder create <NAME>` | Makes a folder at the top of My Drive for a store and prints its id and spec; one of that name already there (that this sign-in can see) is reused, two are an error. Under `drive.file` a store must be a folder the app's client made | `drive-folder/v1` |
| `folder list` | The folders at the top of My Drive this sign-in can see | `drive-folder-list/v1` |
| `profiles list` | The profiles, with account, scopes and whether signed in. Reads no keychain | `profile-list/v1` |
| `profiles remove <NAME> [--force]` | Removes the sign-in, the client and the record; asks first, exit 6 when nobody can answer. The profile flag names it too | `drive-profile-removed/v1` |

A store opened on a profile that is not configured is exit 2 naming
`<cli> drive login --profile <NAME>`. A profile with no client is exit 2
naming `client import`. A refused refresh (revoked, or unused for six months)
is exit 3 naming the sign-in.

## The store

`DriveCli::drive(&DriveSpec)` opens the folder a spec names:
`gdrive:<profile>/<folder-id>`, or `gdrive:<profile>/<folder-id>/<path>` for
a store at a path under that folder (`DriveSpec::join`, as an inbox inside a
records folder). Nothing is read until the first operation,
and the folder itself is checked once: an id Drive does not know, a file, or
a folder in the trash is exit 4. Drive lists an unknown id's children as
nothing, so without that check a wrong id would read as an empty store. A
spec's path is held to the same rule: a part that is not there (each name
must match exactly, capitals included) is exit 4 naming it, for reads and
writes alike, so a typo never reads as empty or makes a second folder.
`mkdir` (and `Backend::ensure_dir`, since `exists` answers `false` for a missing path) is the one call that makes the path. A spec is built only through
`DriveSpec::new`, `parse` and `join`, so two specs naming one store are equal
and print the same text.

The folder id `root` is My Drive: `gdrive:<profile>/root` is the top of the
account's My Drive, and `gdrive:<profile>/root/<path>` a folder under it.
Drive takes `root` wherever a file id goes, but names My Drive by its own id
in each child's `parents`, so the store reads My Drive's own id from the
folder check and works on that id from then on. The spec keeps `root`, so it
prints, caches and hands a background refresh the same text. Under full
`drive` scope this is the usual store.

| Behaviour | How |
|---|---|
| Paths | Store-relative, `/`-separated; `.`, `..` and empty parts are exit 2. Each part resolves to an id through a map cached for the life of the `Drive` value; a listing fills it in bulk |
| Two items of one name | Drive allows it. A path that names two is an error naming both ids (reason `duplicate_name`), never a guess, and nothing is changed. A listing that a path or a copy is built from (`list_here`, `list_dirs`, `list_entries`, `hashes`, the tree copies) stops at the first. `Drive::list_items(rel, recursive)`, a listing for showing, lists every item instead, each with its id, kind (`ItemKind`: folder, file, document), size, modified time and md5, and walks each folder once. `Drive::duplicates(rel)` walks on past each and returns every group, each with its folder's path and id, the name, and the items' ids. It walks each folder once, so a folder that several shortcuts reach is reported under the first path only, where a whole listing shows it under each |
| A file in two folders | Since Drive's 2020 single-parent change a new file has one parent, but one made before it can still have two. A listing that reads both folders in one query (up to 40 at a time) lists it once, under the first; `duplicates` counts it in each |
| Listings | Size, modified time and md5 (`FileEntry::md5`). A whole tree is one query per level of folders, up to 40 folders a query. By default (`Listing::Bytes`), Google's own documents (Docs, Sheets) and shortcuts are left out, and paths do not go through shortcuts |
| Whole listings | `Drive::listing(Listing::Whole)` lists the tree as rclone's default listing does, for a CLI whose folder other apps also fill. A Google document is listed with no size or md5. A shortcut is listed as its target under the shortcut's path, with the target's id, size, modified time and md5; a shortcut to a folder is walked, and two shortcuts to one folder list it under both paths. Reads (`cat`, `copy_out`, `md5`, `exists`, `list_here`, `list_dirs`, `fetch_tree`, `hashes`) follow shortcuts. A write, move, delete, `mkdir` or `put_tree` onto a shortcut, or into a folder reached through one, is exit 2 with reason `shortcut`, so nothing changes another account's file or makes a second item of the name. A shortcut whose target is gone or out of the sign-in's sight stops the listing with reason `shortcut_target_missing`, naming the shortcut; one inside the folder it points at stops it with `shortcut_loop`. A shortcut beside an item of its name is `duplicate_name`. The spec's own path never goes through a shortcut. A file with two parents (see "A file in two folders") is listed once, under the first parent the walk reaches. Each shortcut costs one metadata read |
| Writes | An existing file's content is replaced in place (`files.update`), keeping its id; never a second file of the same name. Every upload is checked against the md5 Drive computes |
| Large files | Above 5 MiB, a resumable session in 8 MiB pieces, resumed from what Drive reports it holds. Downloads above 16 MiB are read in ranges. Every download is checked against Drive's md5 and written through an owner-only temporary file, so a file already at the destination stays until the new one has passed |
| Tree copies | `put_tree` skips every file whose md5 already matches. `Existing::Keep` refuses the whole copy, before uploading anything, when a file there holds other bytes; `Existing::Replace` replaces it in place. A local path that would land on a Google document (Docs, Sheets: no bytes of their own) refuses the whole copy with exit 2, before anything is uploaded. `fetch_tree` downloads what differs. Its paths come from Drive's item names, which another account chooses in a shared folder, so a name that would land outside the local folder (`..`, a leading `/`), or two items landing on one local path (a name holding `/`, or names differing only in case), refuse the whole download with reason `unsafe_name` before anything is written. Only files with bytes are checked, and a file named like the folder another file lands in counts as landing twice. Refusals name their reason: `existing_differs` (`Existing::Keep`), `google_document`, `shortcut`; `pk_cli_drive::reason` has each as a constant to compare with |
| Many files at once | `Drive::cat_many(rels)` reads a batch: one listing of the folders holding the paths (40 folders a query), then the downloads, 8 at a time. Each path gets its own answer in the order asked: the bytes, `None` for no file, or that path's error (`duplicate_name`, a Google document, an md5 that does not match). `put_tree` uploads 4 at a time after making every missing folder once, and `fetch_tree` downloads 8 at a time; the first failure in the tree's order stops either, and what was copied stays. `Backend::write_many(files)` writes a batch of `(rel, bytes)` pairs, binary-safe, each replacing the file at its path in place: on Drive (cached or not) every folder is made once first, then 4 files go at a time, a file whose md5 already matches is skipped, and `Ok(n)` counts the files sent; a mount and rclone write one after another. A path given twice, or one that does not name a file under the store root, refuses the batch with exit 2 before anything is written. The first failure in the order given stops the batch and is returned, and the files before it stay; through the cache, every path given loses its cached copy even after such a failure. `Drive::transfers(n)` lowers the limits. Drive allows about three writes a second per user, so more upload workers only add 429s, which the retries absorb. `Drive` is `Send + Sync`: threads sharing one make each new folder once and take turns writing one file, so neither ends as two items of the name; a tree copy is not guarded against another thread writing its paths meanwhile, and no lock reaches another process |
| Deletes | To Drive's trash (recoverable for 30 days). `delete_file` trashes a file. `trash_empty_folder` trashes an empty folder, and refuses one holding anything outside the trash (exit 2, reason `not_empty`), the store's own folder, and a shortcut (reason `shortcut`) |
| Moves | One `files.update` with `addParents`/`removeParents`. A clobbering move renames first and only then trashes the old destination, so a failure between leaves two files of the name (an error naming both), never none; `move_no_clobber` refuses an occupied one with exit 2 (reason `occupied`) |
| Retries | 429, a 403 with reason `rateLimitExceeded` or `userRateLimitExceeded`, and 5xx, with exponential backoff and jitter (1 s doubling to 32 s, 7 attempts), honouring `Retry-After` in seconds or as a date. A 5xx or lost answer to a create is not blindly retried: the folder is looked up again, or the new file found by name and md5. A file of that name with other bytes is another writer's: the upload stops with reason `duplicate_name` and replaces nothing. A resumable piece that keeps failing for a reason that may pass resumes from what the session holds; a refusal (auth, a full Drive, an expired session) stops at once |
| One item's metadata | `Drive::details(rel)` reads an item (a folder, a file, or by the spec's own id any item): id, name, kind, its parent's id, size, modified time, md5, whether it is in the trash, owned by the account, shared, and whether the account may share it (`capabilities.canShare`). The parent is `None` when Drive names none this sign-in can see, as for My Drive itself; an item shared with the account may have none (see [Sharing and parents](#sharing-and-parents)). `list_items` names the folder each item is listed in (for a followed shortcut, the one holding the shortcut) |
| Who may open an item | `Drive::permissions(rel)` lists the grants (`Permission`: type, role, email or domain, whether it shows in search, expiry) with the item's details. Read only. A grant's `inherited` is Drive's own answer (`permissionDetails`) where it gives one. Otherwise it is worked out from the folder above's grants, one more request: a grant the folder above also holds, at the same role or higher, reaches the item from it. A grant with a higher role on the item, the owner's, and every grant on an item with limited access (`inheritedPermissionsDisabled`) are on the item itself; a `view: metadata` grant of a limited-access folder is from above, and gives no one its content. Where the folder above is not visible to the sign-in, `inherited` is `None`. An item whose grants Drive will not list is exit 3 with reason `permissions_hidden`; `can_share` in its details says whether this account may share it. See [Sharing and parents](#sharing-and-parents) |
| Refusals | A full Drive is exit 5 with reason `drive_storage_full`; a scope or permission refusal exit 3; a missing file exit 4 |
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

Google's Drive pages do not say whether "the app" is one OAuth client or
the Cloud project. Its OAuth pages say a grant is the project's:

From [Cross-client identity](https://developers.google.com/identity/protocols/oauth2/cross-client-identity)
("Cross-client authorization goals" and "Cross-client access tokens"):

- "…the grant indicates the user's trust in the whole application for that
  scope."
- "…the Google authorization infrastructure can use information about user
  approvals for a client ID within a given project when evaluating whether
  to authorize others in the same project."

From [OAuth 2.0 for iOS & Desktop Apps](https://developers.google.com/identity/protocols/oauth2/native-app)
("Token revocation"):

- "Revocation removes all OAuth 2.0 scopes previously granted to a
  project, invalidating any issued access or refresh…" "…for all clients
  registered under that project."

Neither says what that means for the files `drive.file` reaches. Hence the
test below.

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

### The result (2026-11-23)

| Step | Answer |
|---|---|
| Web client: the Desktop client's folder, its listing, its file and the file's bytes | 200 each; the listing holds the Desktop client's file |
| Web client: create a file in that folder | 200 |
| Desktop client: the Web client's file and its bytes | 200 each |
| Either client: the third app's control file | 404 `notFound` |
| Desktop client: delete the folder, which still holds the control file | 403 |
| Revoke the Web client's token, then the Desktop client's | 200, then 400: the Desktop client's token was already revoked |
| Desktop client: read the folder after the revoke | 401 |

So under `drive.file` the clients of one Cloud project share access: a
file one of them creates is open to the others, and a file another app
creates is open to none. A CLI moving off rclone therefore copies its
files once into a folder its own client makes (`folder create`), and the
owner's web app, a Web client of the same project, reads that folder under
`drive.file` as well. And one revoke ends the grant for every client of the
project: `logout --revoke` in one CLI signs the owner out of the others
and of the web app, on every machine. Plain `logout` removes only this
machine's tokens.

## Sharing and parents

What Google documents:

- **The scopes.** The v3 discovery document (revision 20261117) lists
  `drive.file` among the scopes `files.get` and `permissions.list` accept, as
  it does `drive` and `drive.readonly`. It does not say which grants an
  app sees on a file it did not create. An item the app may not see is a 404
  under any of them, so exit 4.
- **`parents`.** Drive documents one parent folder per file. It does not say
  when a `drive.file` sign-in, or an item shared with the account, gets an
  empty `parents`. `details` reports the parent Drive names and `None`
  otherwise, so a folder is another's parent only where Drive says so.
- **`permissions.list`.** Drive does not say whether a caller who may not
  change an item's sharing gets the list or a refusal, only that
  `files.permissions` "is only available if the requesting user can share the
  file". `Drive::permissions` reports the refusal as `permissions_hidden` and
  never an empty list.
- **Inherited or direct.** `permissionDetails.inherited` is "always populated",
  but `inheritedFrom` "is only populated for items in shared drives", and the
  guide's example is a shared drive. So a My Drive answer may carry
  no details, and the comparison with the folder above covers that case. Drive
  documents that a folder's grants reach everything in it and cannot be
  reduced below it.
- **Link sharing.** A grant of type `anyone` is anyone with the link; one of
  type `domain` is anyone in that domain with it. `allowFileDiscovery` says
  whether the item also appears in search for them.

Observed on a real account with the `drive` scope (2026-11-25, synthetic items
the account owns in My Drive, through gdrv): My Drive itself has no parent, a
top-level folder's parent is My Drive's id, and the parent of anything below is
the folder that holds it.
`Drive::permissions` answers for a folder, a folder in it and a file there, and
an unshared item lists its owner alone, as direct, with no link sharing.

Not observed:

- Anything with a `drive.file` sign-in. Google's documentation leaves open which
  grants it lists, and whether `parents` is there, on an item the app did not
  create.
- An inherited grant to someone else, and whether a My Drive answer carries
  `permissionDetails`.
- An item another account owns and shared: its `parents`, and whether Drive
  lists or refuses its grants.

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
