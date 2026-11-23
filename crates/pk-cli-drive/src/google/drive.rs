//! [`Drive`]: a folder in a Google account as a path-addressed store, behind
//! the same interface as the rclone [`crate::Remote`], so the read cache and
//! [`crate::Backend`] work over it unchanged.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use pk_cli_config::profile::ProfileName;
use pk_cli_core::CliError;

use super::api::Api;
use super::files::{md5_file, quote, About, DriveFile, Source, Target};
use crate::backend::{check_deletable, occupied, FileEntry};
use crate::remote::{RemoteRead, RemoteStore};

/// What names a Drive store: the profile that signs in, the id of a folder,
/// and optionally a path under it that is the store's root. Written
/// `gdrive:<profile>/<folder id>[/<path>]`, which is what the cache keys its
/// directory on and what a background refresh is handed (`--remote-spec`).
/// The folder id `root` is the account's My Drive.
///
/// Built only through [`DriveSpec::new`], [`DriveSpec::parse`] and
/// [`DriveSpec::join`], which check the folder id and the path, so two specs
/// naming one store are equal and print the same text.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DriveSpec {
    profile: ProfileName,
    folder: String,
    /// `/`-separated, no leading or trailing `/`; empty for the folder itself.
    path: String,
}

/// The scheme of a [`DriveSpec`]'s text form.
pub const SPEC_PREFIX: &str = "gdrive:";

impl DriveSpec {
    pub fn new(profile: ProfileName, folder: impl Into<String>) -> Result<Self, CliError> {
        let folder = folder.into();
        check_folder_id(&folder)?;
        Ok(DriveSpec {
            profile,
            folder,
            path: String::new(),
        })
    }

    /// The store at `rel` under this one (`inbox` of a records folder).
    pub fn join(&self, rel: &str) -> Result<Self, CliError> {
        let rel = Drive::segments(rel)?.join("/");
        let mut spec = self.clone();
        spec.path = Drive::join(&self.path, &rel);
        Ok(spec)
    }

    /// `gdrive:<profile>/<folder id>[/<path>]`; anything else is exit 2.
    pub fn parse(text: &str) -> Result<Self, CliError> {
        let bad = || {
            CliError::Usage(format!(
                "`{}` is not a Drive store: write gdrive:<profile>/<folder id>[/<path>]",
                text.escape_debug()
            ))
        };
        let rest = text.strip_prefix(SPEC_PREFIX).ok_or_else(bad)?;
        let (profile, rest) = rest.split_once('/').ok_or_else(bad)?;
        let rest = rest.trim_end_matches('/');
        let (folder, path) = rest.split_once('/').unwrap_or((rest, ""));
        DriveSpec::new(ProfileName::parse(profile)?, folder)?.join(path)
    }

    pub fn is_spec(text: &str) -> bool {
        text.starts_with(SPEC_PREFIX)
    }

    /// The Drive profile (the Google account) the store is in.
    pub fn profile(&self) -> &ProfileName {
        &self.profile
    }

    /// The id of the folder the spec names.
    pub fn folder(&self) -> &str {
        &self.folder
    }

    /// The path under that folder that is the store's root; empty for the
    /// folder itself.
    pub fn path(&self) -> &str {
        &self.path
    }
}

impl std::fmt::Display for DriveSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{SPEC_PREFIX}{}/{}", self.profile, self.folder)?;
        if !self.path.is_empty() {
            write!(f, "/{}", self.path)?;
        }
        Ok(())
    }
}

/// A Drive file or folder id: letters, digits, `-` and `_`. Checked because
/// it goes into query strings and URL paths.
pub fn check_folder_id(id: &str) -> Result<(), CliError> {
    let ok = !id.is_empty()
        && id.len() <= 200
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    if ok {
        Ok(())
    } else {
        Err(CliError::Usage(format!(
            "`{}` is not a Google Drive folder id (the part of the folder's URL after /folders/)",
            id.escape_debug()
        )))
    }
}

/// What an existing file gets in [`Drive::put_tree`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Existing {
    /// Never replace a file: one with the same md5 is skipped, one with
    /// other bytes refuses the whole copy before anything is uploaded.
    Keep,
    /// Replace a file whose md5 differs, in place (`files.update`).
    Replace,
}

/// What [`Drive::put_tree`] did, in files.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TreeCopy {
    pub uploaded: usize,
    pub replaced: usize,
    pub unchanged: usize,
    pub bytes: u64,
}

/// Items that share one name in one folder ([`Drive::duplicates`]).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Duplicate {
    /// The folder holding them, relative to the store's root; `""` for the
    /// root itself. Two folders of one name share a path, so `dir_id`
    /// tells them apart.
    pub dir: String,
    /// The Drive id of the folder holding them.
    pub dir_id: String,
    /// The name they share.
    pub name: String,
    /// Whether they are folders (or shortcuts to folders).
    pub folders: bool,
    /// Their Drive ids, sorted.
    pub ids: Vec<String>,
}

/// A file under the store with its Drive md5: a hash listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hashed {
    pub rel: String,
    pub size: u64,
    pub md5: Option<String>,
}

/// What a listing of a [`Drive`] store shows, set with [`Drive::listing`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Listing {
    /// Files with bytes of their own. Google's own documents (Docs, Sheets)
    /// and shortcuts are left out, and paths do not go through shortcuts.
    #[default]
    Bytes,
    /// Everything in the tree, as rclone's default listing shows it:
    /// Google's own documents with no size or md5, and each shortcut as its
    /// target under the shortcut's path (a shortcut to a folder is walked).
    /// Reads follow shortcuts; a write, move or delete through one is exit 2
    /// with reason `shortcut`. A shortcut whose target is gone stops the
    /// listing (`shortcut_target_missing`), as does one inside the folder it
    /// points at (`shortcut_loop`): the listing is whole or an error.
    Whole,
}

#[derive(Debug, Clone)]
struct Node {
    id: String,
    folder: bool,
    /// Reached through a shortcut: this one, or a folder above it.
    via: bool,
}

/// An item at a path: its own, or the target of the shortcut there.
struct Found {
    file: DriveFile,
    via: bool,
}

/// One file of a tree listing, by its path under the listed folder.
struct Listed {
    under: String,
    file: DriveFile,
    via: bool,
}

#[derive(Default)]
struct Tree {
    files: Vec<Listed>,
    /// Folders reached through a shortcut, by their path under the listed
    /// folder.
    via_dirs: Vec<String>,
}

/// A folder in one Google account, addressed by store-relative paths.
///
/// Paths resolve to file ids through a map cached for the life of this
/// value; a listing fills it in bulk. Two children of one folder with the
/// same name (Drive allows it) are an error naming both, never a guess.
/// Deletes go to Drive's trash. Every upload is checked against the md5
/// Drive computes. Requests retry on 429, rate-limit 403s and 5xx.
pub struct Drive {
    api: Api,
    spec: DriveSpec,
    temp_prefix: String,
    listing: Listing,
    nodes: Mutex<HashMap<String, Node>>,
    /// The spec's folder's own id, once checked: the spec's id, or for an
    /// alias (`root`, My Drive) the id Drive answered it with, which is the
    /// one its children name in `parents`.
    root_checked: std::sync::OnceLock<String>,
    /// The id of the store's root: the spec's folder, or the folder at its
    /// path, once found.
    base: std::sync::OnceLock<String>,
}

/// The remedy for a spec whose path is not in Drive.
const MISSING_PATH_REMEDY: &str =
    "check the path in the CLI's settings; each folder name must match exactly, capitals included";

/// Folders asked about in one listing query (`'a' in parents or …`).
const PARENTS_PER_QUERY: usize = 40;

impl Drive {
    pub(crate) fn new(api: Api, spec: DriveSpec) -> Self {
        let mut nodes = HashMap::new();
        nodes.insert(
            String::new(),
            Node {
                id: spec.folder.clone(),
                folder: true,
                via: false,
            },
        );
        Drive {
            api,
            spec,
            temp_prefix: crate::rclone::DEFAULT_TEMP_PREFIX.into(),
            listing: Listing::default(),
            nodes: Mutex::new(nodes),
            root_checked: std::sync::OnceLock::new(),
            base: std::sync::OnceLock::new(),
        }
    }

    /// Name this CLI's temp files `<prefix>-doc-…` (a download's private
    /// dir). Usually the binary name.
    pub fn temp_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.temp_prefix = prefix.into();
        self
    }

    /// What listings show, and whether paths follow shortcuts: see
    /// [`Listing`]. [`Listing::Bytes`] unless set.
    pub fn listing(mut self, listing: Listing) -> Self {
        self.listing = listing;
        self
    }

    fn follows(&self) -> bool {
        self.listing == Listing::Whole
    }

    pub fn spec(&self) -> &DriveSpec {
        &self.spec
    }

    /// The text form of the spec, for [`crate::cache::cache_dir_for`] and a
    /// background refresh's `--remote-spec`.
    pub fn spec_text(&self) -> String {
        self.spec.to_string()
    }

    /// Retry without waiting, for tests against the fake server.
    #[cfg(any(test, feature = "fake-google"))]
    pub fn without_backoff(mut self) -> Self {
        self.api.retry = super::api::Retry::IMMEDIATE;
        self
    }

    // ── paths ───────────────────────────────────────────────────────────
    //
    // Everything below the public and trait methods works on paths under
    // the spec's folder id (`abs`); those methods turn a store path (under
    // the spec's path) into one first.

    /// A store path as a path under the spec's folder id.
    fn abs(&self, rel: &str) -> String {
        Self::join(&self.spec.path, rel.trim_matches('/'))
    }

    /// How a path under the folder id is shown in messages.
    fn shown(&self, abs: &str) -> String {
        let abs = abs.trim_matches('/');
        let root = format!("{SPEC_PREFIX}{}/{}", self.spec.profile, self.spec.folder);
        if abs.is_empty() {
            root
        } else {
            format!("{root}/{abs}")
        }
    }

    fn segments(rel: &str) -> Result<Vec<&str>, CliError> {
        let rel = rel.trim_matches('/');
        if rel.is_empty() {
            return Ok(Vec::new());
        }
        let parts: Vec<&str> = rel.split('/').collect();
        if parts
            .iter()
            .any(|p| p.is_empty() || *p == "." || *p == "..")
        {
            return Err(CliError::Usage(format!(
                "`{}` is not a path in the store: no empty, `.` or `..` parts",
                rel.escape_debug()
            )));
        }
        Ok(parts)
    }

    fn join(parent: &str, name: &str) -> String {
        if parent.is_empty() {
            name.to_string()
        } else {
            format!("{parent}/{name}")
        }
    }

    fn split(rel: &str) -> Result<(String, String), CliError> {
        let parts = Self::segments(rel)?;
        let Some((name, dirs)) = parts.split_last() else {
            return Err(CliError::Usage(
                "a file path is needed, not the store root".into(),
            ));
        };
        Ok((dirs.join("/"), (*name).to_string()))
    }

    fn cached(&self, rel: &str) -> Option<Node> {
        self.nodes
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(rel)
            .cloned()
    }

    fn remember(&self, rel: &str, id: &str, folder: bool, via: bool) {
        self.nodes.lock().unwrap_or_else(|p| p.into_inner()).insert(
            rel.to_string(),
            Node {
                id: id.to_string(),
                folder,
                via,
            },
        );
    }

    /// Forget `rel` and everything under it.
    fn forget(&self, rel: &str) {
        let prefix = format!("{rel}/");
        self.nodes
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .retain(|k, _| k != rel && !k.starts_with(&prefix));
    }

    fn duplicate(&self, parent: &str, name: &str, found: &[DriveFile]) -> CliError {
        let ids: Vec<&str> = found.iter().map(|f| f.id.as_str()).collect();
        CliError::Other(format!(
            "{} has {} items named `{name}` (ids {}); Drive allows that, but a path must name \
             one, so nothing was guessed",
            self.shown(parent),
            found.len(),
            ids.join(", ")
        ))
        .because(crate::reason::DUPLICATE_NAME)
        .with_remedy(
            "open the folder in Google Drive, keep one of them and remove or rename the others",
        )
    }

    fn through_shortcut(&self, rel: &str) -> CliError {
        CliError::Usage(format!(
            "{} is a Google Drive shortcut, or inside a folder one points at; writes, moves \
             and deletes do not go through shortcuts",
            self.shown(rel)
        ))
        .because(crate::reason::SHORTCUT)
        .with_remedy(
            "change the item where it is in Google Drive, or replace the shortcut with a copy",
        )
    }

    fn shortcut_missing(&self, rel: &str, shortcut: &DriveFile) -> CliError {
        CliError::Other(format!(
            "{} is a shortcut (id {}) to an item that is gone or that this sign-in cannot \
             see; a whole listing leaves nothing out, so it stopped",
            self.shown(rel),
            shortcut.id
        ))
        .because(crate::reason::SHORTCUT_TARGET_MISSING)
        .with_remedy(
            "open the folder in Google Drive and remove the shortcut, or restore what it points at",
        )
    }

    fn shortcut_loop(&self, rel: &str) -> CliError {
        CliError::Other(format!(
            "{} is a shortcut to a folder that holds it, so a whole listing would never end",
            self.shown(rel)
        ))
        .because(crate::reason::SHORTCUT_LOOP)
        .with_remedy("open the folder in Google Drive and remove the shortcut")
    }

    /// The item the shortcut at `rel` points at: there, out of the trash,
    /// and of the kind the shortcut says.
    fn follow(&self, rel: &str, shortcut: &DriveFile) -> Result<DriveFile, CliError> {
        let missing = || self.shortcut_missing(rel, shortcut);
        let Some(details) = &shortcut.shortcut_details else {
            return Err(missing());
        };
        if check_folder_id(&details.target_id).is_err() {
            return Err(missing());
        }
        let what = format!("following the shortcut {}", self.shown(rel));
        match self.api.get_meta(&what, &details.target_id)? {
            Some(t)
                if !t.trashed
                    && !t.is_shortcut()
                    && t.is_folder() == shortcut.is_folder_or_points_at_one() =>
            {
                Ok(t)
            }
            _ => Err(missing()),
        }
    }

    /// The one child of `parent_id` named `name` of the kind asked for.
    /// With `follow`, a shortcut counts as the kind it points at and comes
    /// back as its target, marked `via`; without, shortcuts are not there.
    fn child(
        &self,
        parent_rel: &str,
        parent_id: &str,
        name: &str,
        folder: bool,
        follow: bool,
    ) -> Result<Option<Found>, CliError> {
        let at = Self::join(parent_rel, name);
        let what = format!("looking up {}", self.shown(&at));
        let mut found = if follow {
            let mut all = self.api.named(&what, parent_id, name, None)?;
            all.retain(|f| f.is_folder_or_points_at_one() == folder);
            all
        } else {
            let mut found = self.api.named(&what, parent_id, name, Some(folder))?;
            found.retain(|f| !f.is_shortcut());
            found
        };
        match found.len() {
            0 => Ok(None),
            1 => {
                let f = found.pop().expect("one item");
                Ok(Some(if f.is_shortcut() {
                    Found {
                        file: self.follow(&at, &f)?,
                        via: true,
                    }
                } else {
                    Found {
                        file: f,
                        via: false,
                    }
                }))
            }
            _ => Err(self.duplicate(parent_rel, name, &found)),
        }
    }

    /// That the store's folder is a folder this sign-in can see, once per
    /// value. Drive lists the children of an unknown id as nothing, so
    /// without this a wrong id, or a folder `drive.file` does not cover,
    /// reads as an empty store.
    ///
    /// Returns the folder's own id. Drive takes `root` for My Drive wherever
    /// a file id goes, but names My Drive by its own id in every child's
    /// `parents`, so the store works on that id from here on: a listing
    /// that matched children to `root` would find none.
    fn check_root(&self) -> Result<String, CliError> {
        if let Some(id) = self.root_checked.get() {
            return Ok(id.clone());
        }
        let what = format!("opening {}", self.spec_text());
        let found = self.api.get_meta(&what, &self.spec.folder)?;
        let problem = match &found {
            None => Some("Google Drive has no folder with that id that this sign-in may see"),
            Some(f) if f.trashed => Some("that folder is in the Google Drive trash"),
            Some(f) if !f.is_folder() => Some("that id is a file, not a folder"),
            Some(_) => None,
        };
        if let Some(problem) = problem {
            return Err(
                CliError::NotFound(format!("{what}: {problem}")).with_remedy(
                    "check the folder id in the CLI's settings; under the drive.file scope a \
                     folder is visible only when this app's OAuth client created it",
                ),
            );
        }
        let own = found.map(|f| f.id).filter(|id| check_folder_id(id).is_ok());
        let id = own.unwrap_or_else(|| self.spec.folder.clone());
        let _ = self.root_checked.set(id);
        let id = self.root_checked.get().expect("set above").clone();
        self.remember("", &id, true, false);
        Ok(id)
    }

    /// The id of the store's root, once per value: the spec's folder, or the
    /// folder at the spec's path under it. A missing part of that path is
    /// exit 4 naming it, as a wrong folder id is, so a typo never reads as an
    /// empty store and a write never makes a second folder beside the right
    /// one. Only `make` (an explicit `mkdir`) creates the path.
    fn base_id(&self, make: bool) -> Result<String, CliError> {
        self.find_base(make)?.ok_or_else(|| self.missing_base())
    }

    /// [`Drive::base_id`], with `None` for a missing part of the path.
    fn find_base(&self, make: bool) -> Result<Option<String>, CliError> {
        let mut id = self.check_root()?;
        if let Some(base) = self.base.get() {
            return Ok(Some(base.clone()));
        }
        let mut path = String::new();
        for part in Self::segments(&self.spec.path)? {
            let next = Self::join(&path, part);
            // The spec's own path never goes through a shortcut.
            id = match self.child(&path, &id, part, true, false)? {
                Some(f) => f.file.id,
                None if make => {
                    let what = format!("making the folder {}", self.shown(&next));
                    self.api.create_folder(&what, &id, part)?.id
                }
                None => return Ok(None),
            };
            self.remember(&next, &id, true, false);
            path = next;
        }
        let _ = self.base.set(id.clone());
        Ok(Some(id))
    }

    fn missing_base(&self) -> CliError {
        CliError::NotFound(format!(
            "opening {}: there is no folder `{}` in the folder {}",
            self.spec_text(),
            self.spec.path,
            self.spec.folder
        ))
        .with_remedy(MISSING_PATH_REMEDY)
    }

    /// The id of the folder at `rel` (a path from [`Drive::abs`]); with
    /// `create`, the missing folders below the store's root are made.
    fn folder_id(&self, rel: &str, create: bool) -> Result<Option<String>, CliError> {
        Ok(self.folder_node(rel, create)?.map(|n| n.id))
    }

    /// [`Drive::folder_id`], and whether the path went through a shortcut.
    /// `create` is for a write, so it refuses such a path.
    fn folder_node(&self, rel: &str, create: bool) -> Result<Option<Node>, CliError> {
        let parts = Self::segments(rel)?;
        let base = Self::segments(&self.spec.path)?;
        let Some(below) = parts.strip_prefix(base.as_slice()) else {
            return Err(CliError::Other(format!(
                "`{rel}` is outside the store {}",
                self.spec_text()
            )));
        };
        let mut id = self.base_id(false)?;
        let mut via = false;
        let mut path = self.spec.path.clone();
        for part in below {
            let next = Self::join(&path, part);
            match self.cached(&next) {
                Some(node) if node.folder => {
                    id = node.id;
                    via |= node.via;
                }
                _ => match self.child(&path, &id, part, true, self.follows())? {
                    Some(f) => {
                        via |= f.via;
                        self.remember(&next, &f.file.id, true, via);
                        id = f.file.id;
                    }
                    None if create => {
                        if via {
                            return Err(self.through_shortcut(&next));
                        }
                        let what = format!("making the folder {}", self.shown(&next));
                        let made = self.api.create_folder(&what, &id, part)?;
                        self.remember(&next, &made.id, true, false);
                        id = made.id;
                    }
                    None => return Ok(None),
                },
            }
            path = next;
        }
        if create && via {
            return Err(self.through_shortcut(rel));
        }
        Ok(Some(Node {
            id,
            folder: true,
            via,
        }))
    }

    /// The file (not folder) at `rel`, looked up afresh: its folder's id,
    /// the file (a shortcut's target, when followed), and whether the path
    /// went through a shortcut.
    fn file(&self, rel: &str) -> Result<Option<(String, DriveFile, bool)>, CliError> {
        let (dir, name) = Self::split(rel)?;
        let Some(parent) = self.folder_node(&dir, false)? else {
            return Ok(None);
        };
        let Some(found) = self.child(&dir, &parent.id, &name, false, self.follows())? else {
            return Ok(None);
        };
        let via = parent.via || found.via;
        self.remember(rel, &found.file.id, false, via);
        Ok(Some((parent.id, found.file, via)))
    }

    // ── reads ───────────────────────────────────────────────────────────

    /// Everything under the folder `rel`, by path under `rel`: one query
    /// per level of folders, up to 40 folders a query. A missing folder is
    /// empty. Under [`Listing::Whole`], shortcuts are followed.
    fn tree(&self, rel: &str) -> Result<Tree, CliError> {
        struct Pending {
            id: String,
            under: String,
            via: bool,
            /// The folders from the listed one down to this one, by id.
            chain: Vec<String>,
        }
        let rel = rel.trim_matches('/');
        let Some(root) = self.folder_node(rel, false)? else {
            return Ok(Tree::default());
        };
        let follow = self.follows();
        let what = format!("listing {}", self.shown(rel));
        let mut tree = Tree::default();
        let mut level = vec![Pending {
            id: root.id.clone(),
            under: String::new(),
            via: root.via,
            chain: vec![root.id],
        }];
        while !level.is_empty() {
            let mut next = Vec::new();
            for batch in level.chunks(PARENTS_PER_QUERY) {
                // Two shortcuts can reach one folder: its children are
                // listed under each path.
                let mut holders: HashMap<&str, Vec<&Pending>> = HashMap::new();
                for p in batch {
                    holders.entry(p.id.as_str()).or_default().push(p);
                }
                let parents: Vec<String> = holders
                    .keys()
                    .map(|id| format!("{} in parents", quote(id)))
                    .collect();
                let q = format!("({}) and trashed = false", parents.join(" or "));
                let mut by_parent: HashMap<(&str, bool, String), Vec<DriveFile>> = HashMap::new();
                for f in self.api.list(&what, &q)? {
                    let Some(at) = f.parents.iter().find_map(|p| holders.get(p.as_str())) else {
                        continue;
                    };
                    if f.is_shortcut() && !follow {
                        continue;
                    }
                    let folder = f.is_folder_or_points_at_one();
                    for p in at {
                        by_parent
                            .entry((p.under.as_str(), folder, f.name.clone()))
                            .or_default()
                            .push(f.clone());
                    }
                }
                let pending: HashMap<&str, &Pending> =
                    batch.iter().map(|p| (p.under.as_str(), p)).collect();
                for ((parent_under, folder, name), mut files) in by_parent {
                    let p = pending[parent_under];
                    let under = Self::join(parent_under, &name);
                    let path = Self::join(rel, &under);
                    if files.len() > 1 {
                        return Err(self.duplicate(&Self::join(rel, parent_under), &name, &files));
                    }
                    let f = files.pop().expect("one file");
                    let (f, via) = if f.is_shortcut() {
                        (self.follow(&path, &f)?, true)
                    } else {
                        (f, p.via)
                    };
                    let via = via || p.via;
                    self.remember(&path, &f.id, folder, via);
                    if folder {
                        if p.chain.contains(&f.id) {
                            return Err(self.shortcut_loop(&path));
                        }
                        if via {
                            tree.via_dirs.push(under.clone());
                        }
                        let mut chain = p.chain.clone();
                        chain.push(f.id.clone());
                        next.push(Pending {
                            id: f.id,
                            under,
                            via,
                            chain,
                        });
                    } else {
                        tree.files.push(Listed {
                            under,
                            file: f,
                            via,
                        });
                    }
                }
            }
            level = next;
        }
        tree.files.sort_by(|a, b| a.under.cmp(&b.under));
        Ok(tree)
    }

    /// Every name that two or more items share in one folder under `rel`
    /// (recursive): the groups that make a listing or a path stop with
    /// `duplicate_name`. Drive allows them; a path cannot name one. The walk
    /// goes on past each group, into every folder of it, so one call finds
    /// them all. Items group as a listing reads them: by name and by whether
    /// each is a folder, and under [`Listing::Whole`] a shortcut counts as
    /// an item, a shortcut to a folder being walked once (by id). Empty when
    /// `rel` is not there.
    pub fn duplicates(&self, rel: &str) -> Result<Vec<Duplicate>, CliError> {
        let rel = rel.trim_matches('/');
        let Some(root) = self.folder_node(rel, false)? else {
            return Ok(Vec::new());
        };
        let follow = self.follows();
        let what = format!("listing {}", self.shown(rel));
        let mut found = Vec::new();
        let mut walked: HashSet<String> = HashSet::from([root.id.clone()]);
        let mut level = vec![(root.id, String::new())];
        while !level.is_empty() {
            let mut next = Vec::new();
            for batch in level.chunks(PARENTS_PER_QUERY) {
                // Each folder is walked once, so it has one path here.
                let holders: HashMap<&str, &str> = batch
                    .iter()
                    .map(|(id, under)| (id.as_str(), under.as_str()))
                    .collect();
                let parents: Vec<String> = holders
                    .keys()
                    .map(|id| format!("{} in parents", quote(id)))
                    .collect();
                let q = format!("({}) and trashed = false", parents.join(" or "));
                // By the folder's id, not its path: two folders of one name
                // share a path but are two folders.
                let mut groups: BTreeMap<(String, String, bool, String), Vec<DriveFile>> =
                    BTreeMap::new();
                for f in self.api.list(&what, &q)? {
                    if f.is_shortcut() && !follow {
                        continue;
                    }
                    let folder = f.is_folder_or_points_at_one();
                    // Every parent the batch holds: a file in two of them
                    // is in each.
                    for p in &f.parents {
                        if let Some(under) = holders.get(p.as_str()) {
                            groups
                                .entry((under.to_string(), p.clone(), folder, f.name.clone()))
                                .or_default()
                                .push(f.clone());
                        }
                    }
                }
                for ((under, dir_id, folder, name), files) in groups {
                    if files.len() > 1 {
                        let mut ids: Vec<String> = files.iter().map(|f| f.id.clone()).collect();
                        ids.sort();
                        found.push(Duplicate {
                            dir: Self::join(rel, &under),
                            dir_id,
                            name: name.clone(),
                            folders: folder,
                            ids,
                        });
                    }
                    if !folder {
                        continue;
                    }
                    for f in files {
                        let id = match &f.shortcut_details {
                            Some(d) if f.is_shortcut() => d.target_id.clone(),
                            _ => f.id,
                        };
                        if walked.insert(id.clone()) {
                            next.push((id, Self::join(&under, &name)));
                        }
                    }
                }
            }
            level = next;
        }
        found.sort_by(|a, b| {
            (&a.dir, &a.name, a.folders, &a.dir_id).cmp(&(&b.dir, &b.name, b.folders, &b.dir_id))
        });
        Ok(found)
    }

    /// Every file under `rel` (recursive) with its size and md5: one
    /// listing, Google's own documents left out. Under [`Listing::Whole`] a
    /// shortcut to a file is its target.
    pub fn hashes(&self, rel: &str) -> Result<Vec<Hashed>, CliError> {
        let abs = self.abs(rel);
        let rel = rel.trim_matches('/');
        Ok(self
            .tree(&abs)?
            .files
            .into_iter()
            .filter(|l| l.file.is_blob())
            .map(|l| Hashed {
                rel: Self::join(rel, &l.under),
                size: l.file.size().unwrap_or(0),
                md5: l.file.md5(),
            })
            .collect())
    }

    /// The files directly in the folder `rel`, not below it, never from a
    /// cache. Empty when the folder is not there. What counts as a file
    /// follows [`Listing`].
    pub fn list_here(&self, rel: &str) -> Result<Vec<FileEntry>, CliError> {
        let rel = &self.abs(rel);
        let Some(id) = self.folder_id(rel, false)? else {
            return Ok(Vec::new());
        };
        let what = format!("listing {}", self.shown(rel));
        let q = format!(
            "{} in parents and trashed = false and mimeType != {}",
            quote(&id),
            quote(super::files::FOLDER_MIME)
        );
        let follow = self.follows();
        let mut seen: HashMap<String, Vec<DriveFile>> = HashMap::new();
        for f in self.api.list(&what, &q)? {
            let file = if follow {
                !f.is_folder_or_points_at_one()
            } else {
                f.is_blob()
            };
            if file {
                seen.entry(f.name.clone()).or_default().push(f);
            }
        }
        let mut out = Vec::new();
        for (name, mut files) in seen {
            if files.len() > 1 {
                return Err(self.duplicate(rel, &name, &files));
            }
            let f = files.pop().expect("one file");
            let f = if f.is_shortcut() {
                self.follow(&Self::join(rel, &name), &f)?
            } else {
                f
            };
            out.push(entry(name, &f));
        }
        out.sort_by(|a, b| a.rel.cmp(&b.rel));
        Ok(out)
    }

    /// The md5 Drive holds for one file; `None` when it is not there.
    pub fn md5(&self, rel: &str) -> Result<Option<String>, CliError> {
        let rel = &self.abs(rel);
        Ok(self.file(rel)?.and_then(|(_, f, _)| f.md5()))
    }

    /// The account and its storage (replaces `rclone about`).
    pub fn about(&self) -> Result<About, CliError> {
        self.api.about()
    }

    // ── trees ───────────────────────────────────────────────────────────

    /// Copy a local directory's files under `rel`, skipping every file whose
    /// md5 already matches. With [`Existing::Keep`], one already there with
    /// other bytes refuses the whole copy before anything is uploaded.
    pub fn put_tree(
        &self,
        local: &Path,
        rel: &str,
        existing: Existing,
    ) -> Result<TreeCopy, CliError> {
        let rel = &self.abs(rel);
        let mut files = Vec::new();
        walk(local, local, &mut files)?;
        files.sort();
        let tree = self.tree(rel)?;
        let via_dirs = tree.via_dirs;
        let there: HashMap<String, (DriveFile, bool)> = tree
            .files
            .into_iter()
            .map(|l| (l.under, (l.file, l.via)))
            .collect();
        let through: Vec<&String> = files
            .iter()
            .filter(|u| {
                there.get(*u).is_some_and(|(_, via)| *via)
                    || via_dirs.iter().any(|d| u.starts_with(&format!("{d}/")))
            })
            .collect();
        if !through.is_empty() {
            return Err(CliError::Usage(format!(
                "refusing to copy onto {} path(s) under {} that are Google Drive shortcuts or \
                 inside a folder one points at ({}); nothing was uploaded",
                through.len(),
                self.shown(rel),
                through
                    .iter()
                    .take(3)
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
            .because(crate::reason::SHORTCUT));
        }
        let mut plan = Vec::new();
        let mut report = TreeCopy::default();
        let mut conflicts = Vec::new();
        let native: Vec<&String> = files
            .iter()
            .filter(|u| there.get(*u).is_some_and(|(f, _)| !f.is_blob()))
            .collect();
        if !native.is_empty() {
            return Err(CliError::Usage(format!(
                "refusing to copy over {} Google document(s) under {}, which hold no bytes of \
                 their own ({}); nothing was uploaded",
                native.len(),
                self.shown(rel),
                native
                    .iter()
                    .take(3)
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
            .because(crate::reason::GOOGLE_DOCUMENT));
        }
        for under in files {
            let path = local.join(&under);
            let md5 = md5_file(&path)?;
            match there.get(&under) {
                Some((f, _)) if f.md5().as_deref() == Some(md5.as_str()) => report.unchanged += 1,
                Some(_) if existing == Existing::Keep => conflicts.push(under.clone()),
                Some((f, _)) => plan.push((under, path, Some(f.id.clone()))),
                None => plan.push((under, path, None)),
            }
        }
        if !conflicts.is_empty() {
            return Err(CliError::Usage(format!(
                "refusing to replace {} file(s) under {} that hold other bytes ({}{}); nothing \
                 was uploaded",
                conflicts.len(),
                self.shown(rel),
                conflicts
                    .iter()
                    .take(3)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", "),
                if conflicts.len() > 3 { ", …" } else { "" }
            ))
            .because(crate::reason::EXISTING_DIFFERS));
        }
        for (under, path, id) in plan {
            let dest = Self::join(rel, &under);
            let what = format!("uploading {}", self.shown(&dest));
            let len = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            match id {
                Some(id) => {
                    self.api
                        .upload(&what, Target::Existing { id: &id }, &Source::File(&path))?;
                    report.replaced += 1;
                }
                None => {
                    let (dir, name) = Self::split(&dest)?;
                    let parent = self.folder_id(&dir, true)?.expect("made when missing");
                    let made = self.api.upload(
                        &what,
                        Target::New {
                            parent: &parent,
                            name: &name,
                        },
                        &Source::File(&path),
                    )?;
                    self.remember(&dest, &made.id, false, false);
                    report.uploaded += 1;
                }
            }
            report.bytes += len;
        }
        Ok(report)
    }

    /// Download everything under `rel` into `local` (made `0700`), skipping
    /// files already there with the same md5. Files are written owner-only.
    ///
    /// Every path comes from the names of items in Drive, which another
    /// account can choose when the folder is shared or reached through a
    /// shortcut. One that is not a plain relative path once joined onto
    /// `local` (an item named `..`, or one whose name starts with `/`), or two
    /// items that land on one local path (a name holding `/` beside the
    /// folders it spells, or two names that differ only in case, which
    /// macOS's file system holds as one), refuses the whole copy before
    /// anything is written, with reason `unsafe_name`.
    pub fn fetch_tree(&self, rel: &str, local: &Path) -> Result<usize, CliError> {
        let rel = &self.abs(rel);
        // Only what is written is checked: Google's own documents are skipped.
        let files: Vec<Listed> = self
            .tree(rel)?
            .files
            .into_iter()
            .filter(|l| l.file.is_blob())
            .collect();
        // Each file's path, by its parts in lower case, and the folders the
        // files sit in, so a file landing where another lands or where a
        // folder must be refuses the copy.
        let mut landing: HashSet<Vec<String>> = HashSet::new();
        let mut folders: HashSet<Vec<String>> = HashSet::new();
        for Listed { under, .. } in &files {
            let plain = !under.is_empty()
                && Path::new(under)
                    .components()
                    .all(|c| matches!(c, std::path::Component::Normal(_)));
            let key: Vec<String> = Path::new(under)
                .components()
                .map(|c| c.as_os_str().to_string_lossy().to_lowercase())
                .collect();
            let clash = plain
                && (landing.contains(&key)
                    || folders.contains(&key)
                    || (1..key.len()).any(|n| landing.contains(&key[..n])));
            if !plain || clash {
                return Err(CliError::Other(format!(
                    "{} holds an item whose name would land at `{}` under {}, {}; nothing was \
                     downloaded",
                    self.shown(rel),
                    under.escape_debug(),
                    local.display(),
                    if plain {
                        "where another item lands too"
                    } else {
                        "outside it or on no file at all"
                    }
                ))
                .because(crate::reason::UNSAFE_NAME)
                .with_remedy("rename the item in Google Drive, then copy again"));
            }
            for n in 1..key.len() {
                folders.insert(key[..n].to_vec());
            }
            landing.insert(key);
        }
        let mut fetched = 0;
        for Listed { under, file: f, .. } in files {
            let dest = local.join(&under);
            if dest.is_file() && f.md5().is_some_and(|m| md5_file(&dest).ok() == Some(m)) {
                continue;
            }
            if let Some(parent) = dest.parent() {
                pk_cli_core::private_file::create_dir_all(parent)
                    .map_err(|e| CliError::Other(format!("creating {}: {e}", parent.display())))?;
            }
            self.download_to(&Self::join(rel.trim_matches('/'), &under), &f, &dest)?;
            fetched += 1;
        }
        Ok(fetched)
    }

    /// Download `f` to `dest` through an owner-only temp file beside it,
    /// renamed into place once the md5 checks.
    fn download_to(&self, rel: &str, f: &DriveFile, dest: &Path) -> Result<(), CliError> {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let what = format!("reading {}", self.shown(rel));
        let tmp = dest.with_file_name(format!(
            ".{}.{}-{}.part",
            dest.file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("download"),
            std::process::id(),
            SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let mut out = open_private(&tmp)?;
        let res = self.api.download(&what, f, &mut out);
        drop(out);
        if let Err(e) = res {
            let _ = fs::remove_file(&tmp);
            return Err(e);
        }
        fs::rename(&tmp, dest).map_err(|e| {
            let _ = fs::remove_file(&tmp);
            CliError::Other(format!("moving the download to {}: {e}", dest.display()))
        })
    }

    fn upload_to(&self, rel: &str, source: Source<'_>) -> Result<(), CliError> {
        let what = format!("writing {}", self.shown(rel));
        let (dir, name) = Self::split(rel)?;
        let parent = self.folder_id(&dir, true)?.expect("made when missing");
        let existing = self.child(&dir, &parent, &name, false, self.follows())?;
        let made = match existing {
            Some(Found { via: true, .. }) => return Err(self.through_shortcut(rel)),
            Some(Found { file: f, .. }) => {
                if f.md5() == Some(source.md5()?) {
                    self.remember(rel, &f.id, false, false);
                    return Ok(());
                }
                self.api
                    .upload(&what, Target::Existing { id: &f.id }, &source)?
            }
            None => self.api.upload(
                &what,
                Target::New {
                    parent: &parent,
                    name: &name,
                },
                &source,
            )?,
        };
        self.remember(rel, &made.id, false, false);
        Ok(())
    }

    fn move_file(&self, rel_from: &str, rel_to: &str, clobber: bool) -> Result<(), CliError> {
        let from = self.shown(rel_from);
        let Some((from_parent, file, via)) = self.file(rel_from)? else {
            return Err(CliError::NotFound(format!("{from}: nothing to move")));
        };
        if via {
            return Err(self.through_shortcut(rel_from));
        }
        let (dir, name) = Self::split(rel_to)?;
        let to_parent = self.folder_id(&dir, true)?.expect("made when missing");
        let replaced = match self.child(&dir, &to_parent, &name, false, self.follows())? {
            Some(there) if there.via => return Err(self.through_shortcut(rel_to)),
            Some(there) if there.file.id == file.id => return Ok(()),
            Some(_) if !clobber => return Err(occupied(&self.shown(rel_to))),
            there => there.map(|t| t.file),
        };
        let what = format!("moving {from} to {}", self.shown(rel_to));
        self.api
            .rename_move(&what, &file.id, &name, &from_parent, &to_parent)?;
        self.forget(rel_from);
        self.remember(rel_to, &file.id, false, false);
        // The old destination goes to the trash only once the move is done,
        // so a failure between leaves two files of the name (reported as
        // such, both ids named) rather than none.
        if let Some(old) = replaced {
            let what = format!("replacing {}", self.shown(rel_to));
            self.api.trash(&what, &old.id)?;
        }
        if !clobber {
            // Another writer may have taken the name meanwhile: put ours back
            // rather than leave two files of one name.
            let found = self.api.named(&what, &to_parent, &name, Some(false))?;
            if found.len() > 1 {
                let (from_dir, from_name) = Self::split(rel_from)?;
                let _ = from_dir;
                self.api
                    .rename_move(&what, &file.id, &from_name, &to_parent, &from_parent)?;
                self.forget(rel_to);
                return Err(occupied(&self.shown(rel_to)));
            }
        }
        Ok(())
    }
}

fn entry(rel: String, f: &DriveFile) -> FileEntry {
    FileEntry {
        rel,
        size: f.size(),
        modified: f.modified(),
        md5: f.md5(),
    }
}

/// Every regular file under `dir`, as paths under `base` with `/`. Hidden
/// files are copied like any other; symlinks are not followed.
fn walk(base: &Path, dir: &Path, out: &mut Vec<String>) -> Result<(), CliError> {
    let read = fs::read_dir(dir)
        .map_err(|e| CliError::Other(format!("reading {}: {e}", dir.display())))?;
    for entry in read {
        let entry =
            entry.map_err(|e| CliError::Other(format!("reading {}: {e}", dir.display())))?;
        let ft = entry
            .file_type()
            .map_err(|e| CliError::Other(format!("reading {}: {e}", entry.path().display())))?;
        let path = entry.path();
        if ft.is_dir() {
            walk(base, &path, out)?;
        } else if ft.is_file() {
            let rel = path.strip_prefix(base).map_err(|_| {
                CliError::Other(format!("{} is outside {}", path.display(), base.display()))
            })?;
            let parts: Vec<String> = rel
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect();
            out.push(parts.join("/"));
        }
    }
    Ok(())
}

fn open_private(path: &Path) -> Result<fs::File, CliError> {
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts.open(path)
        .map_err(|e| CliError::Other(format!("creating {}: {e}", path.display())))
}

impl RemoteRead for Drive {
    fn target(&self, rel: &str) -> String {
        self.shown(&self.abs(rel))
    }

    fn cat(&self, rel: &str) -> Result<Option<String>, CliError> {
        let rel = &self.abs(rel);
        let Some((_, f, _)) = self.file(rel)? else {
            return Ok(None);
        };
        let what = format!("reading {}", self.shown(rel));
        let dir = crate::backend::private_temp_dir(&format!("{}-remote", self.temp_prefix))?;
        let path = dir.path().join("file");
        let mut out = open_private(&path)?;
        match self.api.download(&what, &f, &mut out) {
            Ok(()) => {}
            Err(e) if e.exit_code() == 4 => {
                self.forget(rel);
                return Ok(None);
            }
            Err(e) => return Err(e),
        }
        drop(out);
        let bytes = fs::read(&path).map_err(|e| CliError::Other(format!("{what}: {e}")))?;
        Ok(Some(String::from_utf8_lossy(&bytes).into_owned()))
    }

    /// Every file under `rel` with its size, modified time and md5. Under
    /// [`Listing::Bytes`], Google's own documents (no bytes of their own)
    /// and shortcuts are left out; under [`Listing::Whole`], a document has
    /// no size or md5 and a shortcut is its target.
    fn list_entries(&self, rel: &str) -> Result<Vec<FileEntry>, CliError> {
        let rel = &self.abs(rel);
        let whole = self.follows();
        Ok(self
            .tree(rel)?
            .files
            .into_iter()
            .filter(|l| whole || l.file.is_blob())
            .map(|l| entry(l.under, &l.file))
            .collect())
    }
}

impl crate::remote::sealed::Sealed for Drive {}

impl RemoteStore for Drive {
    /// Replaces an existing file's content in place (`files.update`, same
    /// id), never a second file of the name; unchanged bytes upload nothing.
    fn write(&self, rel: &str, content: &str) -> Result<(), CliError> {
        let rel = &self.abs(rel);
        self.upload_to(rel, Source::Bytes(content.as_bytes()))
    }

    fn copy_in(&self, local: &Path, rel: &str) -> Result<(), CliError> {
        let rel = &self.abs(rel);
        self.upload_to(rel, Source::File(local))
    }

    fn copy_out(&self, rel: &str, local: &Path) -> Result<(), CliError> {
        let rel = &self.abs(rel);
        let Some((_, f, _)) = self.file(rel)? else {
            return Err(CliError::NotFound(format!(
                "{}: no such file",
                self.shown(rel)
            )));
        };
        // A file already at `local` stays until the download has passed its
        // md5 check, as rclone's `copyto` leaves it on a failure.
        self.download_to(rel, &f, local)
    }

    fn move_to(&self, rel_from: &str, rel_to: &str) -> Result<(), CliError> {
        self.move_file(&self.abs(rel_from), &self.abs(rel_to), true)
    }

    fn move_no_clobber(&self, rel_from: &str, rel_to: &str) -> Result<(), CliError> {
        self.move_file(&self.abs(rel_from), &self.abs(rel_to), false)
    }

    /// Moves the file to Drive's trash. A folder at `rel` is no file: `false`.
    fn delete_file(&self, rel: &str) -> Result<bool, CliError> {
        check_deletable(rel)?;
        let rel = &self.abs(rel);
        let Some((_, f, via)) = self.file(rel)? else {
            return Ok(false);
        };
        if via {
            return Err(self.through_shortcut(rel));
        }
        let what = format!("deleting {}", self.shown(rel));
        let gone = self.api.trash(&what, &f.id)?;
        self.forget(rel);
        Ok(gone)
    }

    /// A file or a folder at `rel`. `false`, not exit 4, when the store's own
    /// path is missing, as for any missing path: `Backend::ensure_dir` asks
    /// this before it makes the folder.
    fn exists(&self, rel: &str) -> Result<bool, CliError> {
        let empty = Self::segments(rel)?.is_empty();
        if self.find_base(false)?.is_none() {
            return Ok(false);
        }
        if empty {
            return Ok(true);
        }
        let rel = &self.abs(rel);
        let (dir, name) = Self::split(rel)?;
        let Some(parent) = self.folder_id(&dir, false)? else {
            return Ok(false);
        };
        if self
            .child(&dir, &parent, &name, false, self.follows())?
            .is_some()
        {
            return Ok(true);
        }
        Ok(self.folder_id(rel, false)?.is_some())
    }

    /// The one write that makes the store's own path when it is missing.
    fn mkdir(&self, rel: &str) -> Result<(), CliError> {
        self.base_id(true)?;
        self.folder_id(&self.abs(rel), true).map(|_| ())
    }

    fn list_dirs(&self, rel: &str) -> Result<Vec<String>, CliError> {
        let rel = &self.abs(rel);
        let Some(dir) = self.folder_node(rel, false)? else {
            return Ok(Vec::new());
        };
        let what = format!("listing {}", self.shown(rel));
        let follow = self.follows();
        let kinds = if follow {
            format!(
                "(mimeType = {} or mimeType = {})",
                quote(super::files::FOLDER_MIME),
                quote(super::files::SHORTCUT_MIME)
            )
        } else {
            format!("mimeType = {}", quote(super::files::FOLDER_MIME))
        };
        let q = format!(
            "{} in parents and trashed = false and {kinds}",
            quote(&dir.id)
        );
        let mut seen: HashMap<String, Vec<DriveFile>> = HashMap::new();
        for f in self.api.list(&what, &q)? {
            if f.is_folder_or_points_at_one() {
                seen.entry(f.name.clone()).or_default().push(f);
            }
        }
        let mut names = Vec::new();
        for (name, mut found) in seen {
            if found.len() > 1 {
                return Err(self.duplicate(rel, &name, &found));
            }
            let path = Self::join(rel.trim_matches('/'), &name);
            let f = found.pop().expect("one folder");
            let (id, via) = if f.is_shortcut() {
                (self.follow(&path, &f)?.id, true)
            } else {
                (f.id, false)
            };
            self.remember(&path, &id, true, dir.via || via);
            names.push(name);
        }
        names.sort();
        Ok(names)
    }

    fn temp_name_prefix(&self) -> &str {
        &self.temp_prefix
    }

    fn with_budget(mut self, budget: Duration) -> Self {
        self.api = self.api.with_deadline(Instant::now() + budget);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn specs_read_and_write_and_refuse_what_is_not_one() {
        let s = DriveSpec::parse("gdrive:adult1/1AbC_d-9").unwrap();
        assert_eq!(s.profile.as_str(), "adult1");
        assert_eq!(s.folder, "1AbC_d-9");
        assert_eq!(s.to_string(), "gdrive:adult1/1AbC_d-9");
        for bad in [
            "adult1/1AbC",
            "gdrive:adult1",
            "gdrive:Adult1/1AbC",
            "gdrive:adult1/1A'bC",
            "gdrive:adult1/",
            "gdrive:adult1/1AbC/a/../b",
            "gdrive:adult1/1AbC/a//b",
        ] {
            assert_eq!(DriveSpec::parse(bad).unwrap_err().exit_code(), 2, "{bad}");
        }
        let sub = DriveSpec::parse("gdrive:adult1/1AbC/inbox/done/").unwrap();
        assert_eq!(sub.path, "inbox/done");
        assert_eq!(sub.to_string(), "gdrive:adult1/1AbC/inbox/done");
        let base = DriveSpec::parse("gdrive:adult1/1AbC").unwrap();
        assert_eq!(base.join("inbox").unwrap().join("done").unwrap(), sub);
    }

    #[test]
    fn paths_refuse_dot_parts_and_split_into_folder_and_name() {
        assert_eq!(Drive::segments("").unwrap(), Vec::<&str>::new());
        assert_eq!(Drive::segments("/a/b/").unwrap(), vec!["a", "b"]);
        for bad in ["a//b", "./a", "a/../b", "a/."] {
            assert_eq!(Drive::segments(bad).unwrap_err().exit_code(), 2, "{bad}");
        }
        assert_eq!(
            Drive::split("a/b/c.json").unwrap(),
            ("a/b".to_string(), "c.json".to_string())
        );
        assert_eq!(
            Drive::split("c.json").unwrap(),
            (String::new(), "c.json".to_string())
        );
        assert_eq!(Drive::split("").unwrap_err().exit_code(), 2);
    }
}
