//! [`Drive`]: a folder in a Google account as a path-addressed store, behind
//! the same interface as the rclone [`crate::Remote`], so the read cache and
//! [`crate::Backend`] work over it unchanged.

use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use pk_cli_config::profile::ProfileName;
use pk_cli_core::{CliError, Reason};

use super::api::Api;
use super::files::{md5_file, quote, About, DriveFile, Source, Target};
use crate::backend::{check_deletable, occupied, FileEntry};
use crate::remote::{RemoteRead, RemoteStore};

/// What names a Drive store: the profile that signs in, the id of a folder,
/// and optionally a path under it that is the store's root. Written
/// `gdrive:<profile>/<folder id>[/<path>]`, which is what the cache keys its
/// directory on and what a background refresh is handed (`--remote-spec`).
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

/// A file under the store with its Drive md5: a hash listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hashed {
    pub rel: String,
    pub size: u64,
    pub md5: Option<String>,
}

#[derive(Debug, Clone)]
struct Node {
    id: String,
    folder: bool,
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
    nodes: Mutex<HashMap<String, Node>>,
    root_checked: std::sync::OnceLock<()>,
    /// The id of the store's root: the spec's folder, or the folder at its
    /// path, once found.
    base: std::sync::OnceLock<String>,
}

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
            },
        );
        Drive {
            api,
            spec,
            temp_prefix: crate::rclone::DEFAULT_TEMP_PREFIX.into(),
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

    fn remember(&self, rel: &str, id: &str, folder: bool) {
        self.nodes.lock().unwrap_or_else(|p| p.into_inner()).insert(
            rel.to_string(),
            Node {
                id: id.to_string(),
                folder,
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
        .because(Reason::Provider("duplicate_name"))
        .with_remedy(
            "open the folder in Google Drive, keep one of them and remove or rename the others",
        )
    }

    /// The one child of `parent_id` named `name` of the kind asked for.
    fn child(
        &self,
        parent_rel: &str,
        parent_id: &str,
        name: &str,
        folder: bool,
    ) -> Result<Option<DriveFile>, CliError> {
        let what = format!("looking up {}", self.shown(&Self::join(parent_rel, name)));
        let mut found = self.api.named(&what, parent_id, name, folder)?;
        if !folder {
            found.retain(|f| f.mime_type != "application/vnd.google-apps.shortcut");
        }
        match found.len() {
            0 => Ok(None),
            1 => Ok(found.pop()),
            _ => Err(self.duplicate(parent_rel, name, &found)),
        }
    }

    /// That the store's folder is a folder this sign-in can see, once per
    /// value. Drive lists the children of an unknown id as nothing, so
    /// without this a wrong id, or a folder `drive.file` does not cover,
    /// reads as an empty store.
    fn check_root(&self) -> Result<(), CliError> {
        if self.root_checked.get().is_some() {
            return Ok(());
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
        let _ = self.root_checked.set(());
        Ok(())
    }

    /// The id of the store's root, once per value: the spec's folder, or the
    /// folder at the spec's path under it. A missing part of that path is
    /// exit 4 naming it, as a wrong folder id is, so a typo never reads as an
    /// empty store and a write never makes a second folder beside the right
    /// one. Only `make` (an explicit `mkdir`) creates the path.
    fn base_id(&self, make: bool) -> Result<String, CliError> {
        self.check_root()?;
        if let Some(id) = self.base.get() {
            return Ok(id.clone());
        }
        let mut id = self.spec.folder.clone();
        let mut path = String::new();
        for part in Self::segments(&self.spec.path)? {
            let next = Self::join(&path, part);
            id = match self.child(&path, &id, part, true)? {
                Some(f) => f.id,
                None if make => {
                    let what = format!("making the folder {}", self.shown(&next));
                    self.api.create_folder(&what, &id, part)?.id
                }
                None => return Err(CliError::NotFound(format!(
                    "opening {}: there is no folder `{next}` in the folder {}",
                    self.spec_text(),
                    self.spec.folder
                ))
                .with_remedy(
                    "check the path in the CLI's settings; each folder name must match exactly, \
                         capitals included",
                )),
            };
            self.remember(&next, &id, true);
            path = next;
        }
        let _ = self.base.set(id.clone());
        Ok(id)
    }

    /// The id of the folder at `rel` (a path from [`Drive::abs`]); with
    /// `create`, the missing folders below the store's root are made.
    fn folder_id(&self, rel: &str, create: bool) -> Result<Option<String>, CliError> {
        let parts = Self::segments(rel)?;
        let base = Self::segments(&self.spec.path)?;
        let Some(below) = parts.strip_prefix(base.as_slice()) else {
            return Err(CliError::Other(format!(
                "`{rel}` is outside the store {}",
                self.spec_text()
            )));
        };
        let mut id = self.base_id(false)?;
        let mut path = self.spec.path.clone();
        for part in below {
            let next = Self::join(&path, part);
            match self.cached(&next) {
                Some(node) if node.folder => id = node.id,
                _ => match self.child(&path, &id, part, true)? {
                    Some(f) => {
                        self.remember(&next, &f.id, true);
                        id = f.id;
                    }
                    None if create => {
                        let what = format!("making the folder {}", self.shown(&next));
                        let made = self.api.create_folder(&what, &id, part)?;
                        self.remember(&next, &made.id, true);
                        id = made.id;
                    }
                    None => return Ok(None),
                },
            }
            path = next;
        }
        Ok(Some(id))
    }

    /// The file (not folder) at `rel`, looked up afresh.
    fn file(&self, rel: &str) -> Result<Option<(String, DriveFile)>, CliError> {
        let (dir, name) = Self::split(rel)?;
        let Some(parent) = self.folder_id(&dir, false)? else {
            return Ok(None);
        };
        let found = self.child(&dir, &parent, &name, false)?;
        if let Some(f) = &found {
            self.remember(rel, &f.id, false);
        }
        Ok(found.map(|f| (parent, f)))
    }

    // ── reads ───────────────────────────────────────────────────────────

    /// Everything under the folder `rel`, as (path under `rel`, file):
    /// one query per level of folders, up to 40 folders a query. A missing
    /// folder is empty.
    fn tree(&self, rel: &str) -> Result<Vec<(String, DriveFile)>, CliError> {
        let rel = rel.trim_matches('/');
        let Some(root) = self.folder_id(rel, false)? else {
            return Ok(Vec::new());
        };
        let what = format!("listing {}", self.shown(rel));
        let mut out = Vec::new();
        // Folders to list next: id → path under `rel`.
        let mut level: Vec<(String, String)> = vec![(root, String::new())];
        while !level.is_empty() {
            let mut next = Vec::new();
            for batch in level.chunks(PARENTS_PER_QUERY) {
                let paths: HashMap<&str, &str> = batch
                    .iter()
                    .map(|(id, p)| (id.as_str(), p.as_str()))
                    .collect();
                let parents: Vec<String> = batch
                    .iter()
                    .map(|(id, _)| format!("{} in parents", quote(id)))
                    .collect();
                let q = format!("({}) and trashed = false", parents.join(" or "));
                let mut by_parent: HashMap<(&str, bool, String), Vec<DriveFile>> = HashMap::new();
                for f in self.api.list(&what, &q)? {
                    let Some(parent) = f
                        .parents
                        .iter()
                        .find_map(|p| paths.get_key_value(p.as_str()).map(|(k, _)| *k))
                    else {
                        continue;
                    };
                    if f.mime_type == "application/vnd.google-apps.shortcut" {
                        continue;
                    }
                    by_parent
                        .entry((parent, f.is_folder(), f.name.clone()))
                        .or_default()
                        .push(f);
                }
                for ((parent, folder, name), mut files) in by_parent {
                    let under = Self::join(paths[parent], &name);
                    if files.len() > 1 {
                        return Err(self.duplicate(&Self::join(rel, paths[parent]), &name, &files));
                    }
                    let f = files.pop().expect("one file");
                    self.remember(&Self::join(rel, &under), &f.id, folder);
                    if folder {
                        next.push((f.id.clone(), under));
                    } else {
                        out.push((under, f));
                    }
                }
            }
            level = next;
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(out)
    }

    /// Every file under `rel` (recursive) with its size and md5: one
    /// listing, Google's own documents left out.
    pub fn hashes(&self, rel: &str) -> Result<Vec<Hashed>, CliError> {
        let abs = self.abs(rel);
        let rel = rel.trim_matches('/');
        Ok(self
            .tree(&abs)?
            .into_iter()
            .filter(|(_, f)| f.is_blob())
            .map(|(under, f)| Hashed {
                rel: Self::join(rel, &under),
                size: f.size().unwrap_or(0),
                md5: f.md5(),
            })
            .collect())
    }

    /// The files directly in the folder `rel`, not below it, never from a
    /// cache. Empty when the folder is not there.
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
        let mut seen: HashMap<String, Vec<DriveFile>> = HashMap::new();
        for f in self.api.list(&what, &q)? {
            if f.is_blob() {
                seen.entry(f.name.clone()).or_default().push(f);
            }
        }
        let mut out = Vec::new();
        for (name, files) in seen {
            if files.len() > 1 {
                return Err(self.duplicate(rel, &name, &files));
            }
            let f = &files[0];
            out.push(entry(name, f));
        }
        out.sort_by(|a, b| a.rel.cmp(&b.rel));
        Ok(out)
    }

    /// The md5 Drive holds for one file; `None` when it is not there.
    pub fn md5(&self, rel: &str) -> Result<Option<String>, CliError> {
        let rel = &self.abs(rel);
        Ok(self.file(rel)?.and_then(|(_, f)| f.md5()))
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
        let there: HashMap<String, DriveFile> = self.tree(rel)?.into_iter().collect();
        let mut plan = Vec::new();
        let mut report = TreeCopy::default();
        let mut conflicts = Vec::new();
        let native: Vec<&String> = files
            .iter()
            .filter(|u| there.get(*u).is_some_and(|f| !f.is_blob()))
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
            )));
        }
        for under in files {
            let path = local.join(&under);
            let md5 = md5_file(&path)?;
            match there.get(&under) {
                Some(f) if f.md5().as_deref() == Some(md5.as_str()) => report.unchanged += 1,
                Some(_) if existing == Existing::Keep => conflicts.push(under.clone()),
                Some(f) => plan.push((under, path, Some(f.id.clone()))),
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
            )));
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
                    self.remember(&dest, &made.id, false);
                    report.uploaded += 1;
                }
            }
            report.bytes += len;
        }
        Ok(report)
    }

    /// Download everything under `rel` into `local` (made `0700`), skipping
    /// files already there with the same md5. Files are written owner-only.
    pub fn fetch_tree(&self, rel: &str, local: &Path) -> Result<usize, CliError> {
        let rel = &self.abs(rel);
        let mut fetched = 0;
        for (under, f) in self.tree(rel)? {
            if !f.is_blob() {
                continue;
            }
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
        let existing = self.child(&dir, &parent, &name, false)?;
        let made = match existing {
            Some(f) => {
                if f.md5() == Some(source.md5()?) {
                    self.remember(rel, &f.id, false);
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
        self.remember(rel, &made.id, false);
        Ok(())
    }

    fn move_file(&self, rel_from: &str, rel_to: &str, clobber: bool) -> Result<(), CliError> {
        let from = self.shown(rel_from);
        let Some((from_parent, file)) = self.file(rel_from)? else {
            return Err(CliError::NotFound(format!("{from}: nothing to move")));
        };
        let (dir, name) = Self::split(rel_to)?;
        let to_parent = self.folder_id(&dir, true)?.expect("made when missing");
        let replaced = match self.child(&dir, &to_parent, &name, false)? {
            Some(there) if there.id == file.id => return Ok(()),
            Some(_) if !clobber => return Err(occupied(&self.shown(rel_to))),
            there => there,
        };
        let what = format!("moving {from} to {}", self.shown(rel_to));
        self.api
            .rename_move(&what, &file.id, &name, &from_parent, &to_parent)?;
        self.forget(rel_from);
        self.remember(rel_to, &file.id, false);
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
            let found = self.api.named(&what, &to_parent, &name, false)?;
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
        let Some((_, f)) = self.file(rel)? else {
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

    /// Every file under `rel` with its size, modified time and md5. Google's
    /// own documents (no bytes of their own) and shortcuts are left out.
    fn list_entries(&self, rel: &str) -> Result<Vec<FileEntry>, CliError> {
        let rel = &self.abs(rel);
        Ok(self
            .tree(rel)?
            .into_iter()
            .filter(|(_, f)| f.is_blob())
            .map(|(under, f)| entry(under, &f))
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
        let Some((_, f)) = self.file(rel)? else {
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
        let Some((_, f)) = self.file(rel)? else {
            return Ok(false);
        };
        let what = format!("deleting {}", self.shown(rel));
        let gone = self.api.trash(&what, &f.id)?;
        self.forget(rel);
        Ok(gone)
    }

    /// A file or a folder at `rel`.
    fn exists(&self, rel: &str) -> Result<bool, CliError> {
        if Self::segments(rel)?.is_empty() {
            return self.base_id(false).map(|_| true);
        }
        let rel = &self.abs(rel);
        let (dir, name) = Self::split(rel)?;
        let Some(parent) = self.folder_id(&dir, false)? else {
            return Ok(false);
        };
        if self.child(&dir, &parent, &name, false)?.is_some() {
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
        let Some(id) = self.folder_id(rel, false)? else {
            return Ok(Vec::new());
        };
        let what = format!("listing {}", self.shown(rel));
        let q = format!(
            "{} in parents and trashed = false and mimeType = {}",
            quote(&id),
            quote(super::files::FOLDER_MIME)
        );
        let mut seen: HashMap<String, Vec<DriveFile>> = HashMap::new();
        for f in self.api.list(&what, &q)? {
            seen.entry(f.name.clone()).or_default().push(f);
        }
        let mut names = Vec::new();
        for (name, found) in seen {
            if found.len() > 1 {
                return Err(self.duplicate(rel, &name, &found));
            }
            self.remember(
                &Self::join(rel.trim_matches('/'), &name),
                &found[0].id,
                true,
            );
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
