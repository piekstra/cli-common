//! Drive files by id: listing, metadata, folders, uploads (multipart, or
//! resumable above [`RESUMABLE_ABOVE`]), ranged downloads with an md5
//! check, moves, trash and the account's `about`.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use md5::{Digest, Md5};
use pk_cli_core::CliError;
use serde::Deserialize;
use serde_json::{json, Value};

use super::api::{is_ambiguous, Answer, Api};

pub(crate) const FOLDER_MIME: &str = "application/vnd.google-apps.folder";
/// The fields every file answer carries.
pub(crate) const FIELDS: &str = "id,name,mimeType,parents,size,md5Checksum,modifiedTime";
/// Larger files go up in a resumable session, in [`CHUNK`] pieces.
pub const RESUMABLE_ABOVE: u64 = 5 * 1024 * 1024;
/// A resumable upload's piece: a multiple of 256 KiB, as Drive requires.
pub(crate) const CHUNK: u64 = 8 * 1024 * 1024;
/// A download's piece, one ranged request each.
pub(crate) const DOWNLOAD_CHUNK: u64 = 16 * 1024 * 1024;

/// A file or folder as Drive describes it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DriveFile {
    pub id: String,
    pub name: String,
    pub mime_type: String,
    #[serde(default)]
    pub parents: Vec<String>,
    /// Bytes, as Drive's int64-as-string; absent for folders and Google's
    /// own document types.
    #[serde(default)]
    pub size: Option<String>,
    #[serde(default)]
    pub md5_checksum: Option<String>,
    /// RFC 3339 in UTC.
    #[serde(default)]
    pub modified_time: Option<String>,
    /// Only a metadata read asks for it; listings exclude trashed items.
    #[serde(default)]
    pub trashed: bool,
}

impl DriveFile {
    pub fn is_folder(&self) -> bool {
        self.mime_type == FOLDER_MIME
    }

    /// A file with bytes of its own: not a folder, a shortcut or one of
    /// Google's document types (Docs, Sheets …), which have no md5 and
    /// cannot be downloaded as they are.
    pub fn is_blob(&self) -> bool {
        !self.mime_type.starts_with("application/vnd.google-apps.")
    }

    pub fn size(&self) -> Option<u64> {
        self.size.as_deref().and_then(|s| s.parse().ok())
    }

    pub fn md5(&self) -> Option<String> {
        self.md5_checksum.as_deref().map(str::to_ascii_lowercase)
    }

    /// Unix seconds.
    pub fn modified(&self) -> Option<i64> {
        self.modified_time.as_deref().and_then(parse_rfc3339)
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Page {
    #[serde(default)]
    files: Vec<DriveFile>,
    #[serde(default)]
    next_page_token: Option<String>,
}

/// The account behind a profile and its storage, from Drive's `about`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct About {
    pub email: String,
    pub display_name: Option<String>,
    /// Bytes; `None` for unlimited storage.
    pub limit: Option<u64>,
    pub usage: Option<u64>,
    pub usage_in_drive: Option<u64>,
    pub usage_in_trash: Option<u64>,
}

impl About {
    pub fn free(&self) -> Option<u64> {
        Some(self.limit?.saturating_sub(self.usage?))
    }
}

/// A Drive query's string literal: `'…'` with `\` and `'` escaped.
pub(crate) fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\\', "\\\\").replace('\'', "\\'"))
}

/// What an upload sends: bytes in hand, or a local file.
pub(crate) enum Source<'a> {
    Bytes(&'a [u8]),
    File(&'a Path),
}

impl Source<'_> {
    fn len(&self) -> Result<u64, CliError> {
        match self {
            Source::Bytes(b) => Ok(b.len() as u64),
            Source::File(p) => std::fs::metadata(p)
                .map(|m| m.len())
                .map_err(|e| CliError::Other(format!("reading {}: {e}", p.display()))),
        }
    }

    pub(crate) fn md5(&self) -> Result<String, CliError> {
        match self {
            Source::Bytes(b) => Ok(hex(&Md5::digest(b))),
            Source::File(p) => md5_file(p),
        }
    }

    /// Bytes `start..start+len`.
    fn piece(&self, start: u64, len: u64) -> Result<Vec<u8>, CliError> {
        match self {
            Source::Bytes(b) => Ok(b[start as usize..(start + len) as usize].to_vec()),
            Source::File(p) => {
                let failed =
                    |e: std::io::Error| CliError::Other(format!("reading {}: {e}", p.display()));
                let mut f = File::open(p).map_err(failed)?;
                f.seek(SeekFrom::Start(start)).map_err(failed)?;
                let mut buf = vec![0; len as usize];
                f.read_exact(&mut buf).map_err(failed)?;
                Ok(buf)
            }
        }
    }
}

/// Where an upload lands: a new file, or new content for one that exists
/// (`files.update`, which keeps its id and never makes a second file).
pub(crate) enum Target<'a> {
    New { parent: &'a str, name: &'a str },
    Existing { id: &'a str },
}

/// The lowercase hex md5 of a local file, read in pieces.
pub fn md5_file(path: &Path) -> Result<String, CliError> {
    let failed = |e: std::io::Error| CliError::Other(format!("reading {}: {e}", path.display()));
    let mut f = File::open(path).map_err(failed)?;
    let mut h = Md5::new();
    let mut buf = vec![0u8; 1024 * 1024];
    loop {
        let n = f.read(&mut buf).map_err(failed)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(hex(&h.finalize()))
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

impl Api {
    /// Every file a query matches, all pages.
    pub(crate) fn list(&self, what: &str, q: &str) -> Result<Vec<DriveFile>, CliError> {
        let mut out = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let url = format!("{}/files", self.endpoints.api);
            let answer = self.call(what, true, &[], &|http| {
                let mut req = http.get(&url).query(&[
                    ("q", q),
                    ("fields", &format!("nextPageToken,files({FIELDS})")),
                    ("pageSize", "1000"),
                    ("spaces", "drive"),
                ]);
                if let Some(t) = &token {
                    req = req.query(&[("pageToken", t.as_str())]);
                }
                req
            })?;
            let page: Page = answer.json(what)?;
            out.extend(page.files);
            match page.next_page_token {
                Some(t) if !t.is_empty() => token = Some(t),
                _ => return Ok(out),
            }
        }
    }

    /// A file's metadata; `None` when Drive does not know it (or this
    /// client may not see it).
    pub(crate) fn get_meta(&self, what: &str, id: &str) -> Result<Option<DriveFile>, CliError> {
        let url = format!("{}/files/{}", self.endpoints.api, id);
        let answer = self.call(what, true, &[404], &|http| {
            http.get(&url)
                .query(&[("fields", &format!("{FIELDS},trashed"))])
        })?;
        if answer.status == 404 {
            return Ok(None);
        }
        answer.json(what).map(Some)
    }

    /// The children of `parent` named `name`, of the kind asked for.
    pub(crate) fn named(
        &self,
        what: &str,
        parent: &str,
        name: &str,
        folders: bool,
    ) -> Result<Vec<DriveFile>, CliError> {
        let kind = if folders { "=" } else { "!=" };
        let q = format!(
            "name = {} and {} in parents and trashed = false and mimeType {kind} {}",
            quote(name),
            quote(parent),
            quote(FOLDER_MIME)
        );
        Ok(self
            .list(what, &q)?
            .into_iter()
            // Drive names My Drive by its own id in `parents`, never by the
            // `root` alias a query takes.
            .filter(|f| {
                f.name == name && (parent == "root" || f.parents.iter().any(|p| p == parent))
            })
            .collect())
    }

    /// A new folder `name` in `parent`. A create that may or may not have
    /// happened is looked for before it is tried again.
    pub(crate) fn create_folder(
        &self,
        what: &str,
        parent: &str,
        name: &str,
    ) -> Result<DriveFile, CliError> {
        let url = format!("{}/files", self.endpoints.api);
        let body = json!({"name": name, "mimeType": FOLDER_MIME, "parents": [parent]});
        for attempt in 0..3 {
            match self.call(what, false, &[], &|http| {
                http.post(&url).query(&[("fields", FIELDS)]).json(&body)
            }) {
                Ok(answer) => return answer.json(what),
                Err(e) if is_ambiguous(&e) && attempt < 2 => {
                    if let Some(made) = self.named(what, parent, name, true)?.into_iter().next() {
                        return Ok(made);
                    }
                }
                Err(e) => return Err(e),
            }
        }
        unreachable!("the loop returns on its last attempt")
    }

    /// Upload `source` to `target` and check the md5 Drive computed against
    /// the local one: a write succeeds only on that read-back.
    pub(crate) fn upload(
        &self,
        what: &str,
        target: Target<'_>,
        source: &Source<'_>,
    ) -> Result<DriveFile, CliError> {
        let len = source.len()?;
        let md5 = source.md5()?;
        let made = if len > RESUMABLE_ABOVE {
            self.upload_resumable(what, &target, source, len)?
        } else {
            self.upload_simple(what, &target, source, &md5)?
        };
        match made.md5() {
            Some(got) if got == md5 => Ok(made),
            got => Err(CliError::Upstream(format!(
                "{what}: Google Drive holds {} after the upload, not the {md5} sent; the file \
                 there may be damaged, so upload it again",
                got.unwrap_or_else(|| "no md5".into())
            ))),
        }
    }

    fn upload_simple(
        &self,
        what: &str,
        target: &Target<'_>,
        source: &Source<'_>,
        md5: &str,
    ) -> Result<DriveFile, CliError> {
        let bytes = source.piece(0, source.len()?)?;
        match target {
            Target::Existing { id } => {
                let url = format!("{}/files/{}", self.endpoints.upload, id);
                let answer = self.call(what, true, &[], &|http| {
                    http.patch(&url)
                        .query(&[("uploadType", "media"), ("fields", FIELDS)])
                        .header("Content-Type", "application/octet-stream")
                        .body(bytes.clone())
                })?;
                answer.json(what)
            }
            Target::New { parent, name } => {
                let url = format!("{}/files", self.endpoints.upload);
                let boundary = format!(
                    "pk-cli-drive-{}",
                    pk_cli_auth::oauth::random_token(24)?.replace(['-', '_'], "x")
                );
                let meta = json!({"name": name, "parents": [parent]}).to_string();
                let mut body = Vec::with_capacity(bytes.len() + meta.len() + 256);
                body.extend_from_slice(
                    format!(
                        "--{boundary}\r\nContent-Type: application/json; charset=UTF-8\r\n\r\n{meta}\r\n--{boundary}\r\nContent-Type: application/octet-stream\r\n\r\n"
                    )
                    .as_bytes(),
                );
                body.extend_from_slice(&bytes);
                body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
                for attempt in 0..3 {
                    match self.call(what, false, &[], &|http| {
                        http.post(&url)
                            .query(&[("uploadType", "multipart"), ("fields", FIELDS)])
                            .header(
                                "Content-Type",
                                format!("multipart/related; boundary={boundary}"),
                            )
                            .body(body.clone())
                    }) {
                        Ok(answer) => return answer.json(what),
                        Err(e) if is_ambiguous(&e) && attempt < 2 => {
                            // Did it land? Take ours if so; anything else
                            // there gets the content in place.
                            let found = self.named(what, parent, name, false)?;
                            if let Some(f) = found.iter().find(|f| f.md5().as_deref() == Some(md5))
                            {
                                return Ok(f.clone());
                            }
                            if let [only] = found.as_slice() {
                                return self.upload_simple(
                                    what,
                                    &Target::Existing { id: &only.id },
                                    source,
                                    md5,
                                );
                            }
                        }
                        Err(e) => return Err(e),
                    }
                }
                unreachable!("the loop returns on its last attempt")
            }
        }
    }

    fn upload_resumable(
        &self,
        what: &str,
        target: &Target<'_>,
        source: &Source<'_>,
        len: u64,
    ) -> Result<DriveFile, CliError> {
        let (method, url, meta) = match target {
            Target::New { parent, name } => (
                reqwest::Method::POST,
                format!("{}/files", self.endpoints.upload),
                json!({"name": name, "parents": [parent]}),
            ),
            Target::Existing { id } => (
                reqwest::Method::PATCH,
                format!("{}/files/{}", self.endpoints.upload, id),
                json!({}),
            ),
        };
        // Opening a session makes nothing, so it is safe to retry.
        let opened = self.call(what, true, &[], &|http| {
            http.request(method.clone(), &url)
                .query(&[("uploadType", "resumable"), ("fields", FIELDS)])
                .header("X-Upload-Content-Type", "application/octet-stream")
                .header("X-Upload-Content-Length", len.to_string())
                .json(&meta)
        })?;
        let session = opened
            .header("location")
            .ok_or_else(|| {
                CliError::Upstream(format!(
                    "{what}: Google Drive opened no upload session (no Location)"
                ))
            })?
            .to_string();
        // The bearer token goes with every piece, so the session must be on
        // the upload endpoint's own host, whatever the answer named.
        pk_cli_core::transport::require_secure_url(&session).map_err(|_| {
            CliError::Upstream(format!(
                "{what}: refusing an upload session that is not https"
            ))
        })?;
        if !same_origin(&session, &self.endpoints.upload) {
            return Err(CliError::Upstream(format!(
                "{what}: refusing an upload session on another host than Google Drive's upload \
                 endpoint; nothing was sent to it"
            )));
        }
        let mut sent = 0u64;
        let mut stalls = 0;
        loop {
            let piece = CHUNK.min(len - sent);
            let bytes = source.piece(sent, piece)?;
            let range = format!("bytes {}-{}/{}", sent, sent + piece - 1, len);
            let put = self.call(what, true, &[308], &|http| {
                http.put(&session)
                    .header("Content-Range", range.clone())
                    .body(bytes.clone())
            });
            let answer = match put {
                Ok(a) => a,
                // A piece that failed after its retries: ask the session
                // how much it holds, and go on from there.
                Err(_) if stalls < 3 => {
                    stalls += 1;
                    self.upload_status(what, &session, len)?
                }
                Err(e) => return Err(e),
            };
            if answer.status == 308 {
                let held = received(&answer);
                if held <= sent {
                    stalls += 1;
                    if stalls > 3 {
                        return Err(CliError::Upstream(format!(
                            "{what}: the upload session stopped taking bytes at {held} of {len}"
                        )));
                    }
                }
                sent = held;
                if sent >= len {
                    let done = self.upload_status(what, &session, len)?;
                    if done.status == 308 {
                        return Err(CliError::Upstream(format!(
                            "{what}: the upload session holds every byte but did not finish"
                        )));
                    }
                    return done.json(what);
                }
                continue;
            }
            return answer.json(what);
        }
    }

    /// `Content-Range: bytes */<len>`: the session's state.
    fn upload_status(&self, what: &str, session: &str, len: u64) -> Result<Answer, CliError> {
        self.call(what, true, &[308], &|http| {
            http.put(session)
                .header("Content-Range", format!("bytes */{len}"))
                .header("Content-Length", "0")
        })
    }

    /// Download `file` into `out` in ranged pieces, and check the md5.
    pub(crate) fn download(
        &self,
        what: &str,
        file: &DriveFile,
        out: &mut File,
    ) -> Result<(), CliError> {
        if !file.is_blob() {
            return Err(CliError::Usage(format!(
                "{what}: it is a Google {} with no bytes of its own; export it from Drive instead",
                file.mime_type
                    .trim_start_matches("application/vnd.google-apps.")
            )));
        }
        let url = format!("{}/files/{}", self.endpoints.api, file.id);
        let mut hash = Md5::new();
        let write = |out: &mut File, hash: &mut Md5, body: &[u8]| {
            hash.update(body);
            out.write_all(body)
                .map_err(|e| CliError::Other(format!("{what}: writing the download: {e}")))
        };
        match file.size() {
            Some(size) if size > DOWNLOAD_CHUNK => {
                let mut at = 0;
                while at < size {
                    let end = (at + DOWNLOAD_CHUNK).min(size) - 1;
                    let answer = self.call(what, true, &[], &|http| {
                        http.get(&url)
                            .query(&[("alt", "media")])
                            .header("Range", format!("bytes={at}-{end}"))
                    })?;
                    if answer.status != 206 && !(at == 0 && answer.status == 200) {
                        return Err(CliError::Upstream(format!(
                            "{what}: Google Drive answered a ranged read with {}",
                            answer.status
                        )));
                    }
                    if answer.status == 200 {
                        write(out, &mut hash, &answer.body)?;
                        break;
                    }
                    write(out, &mut hash, &answer.body)?;
                    at += answer.body.len() as u64;
                    if answer.body.is_empty() {
                        return Err(CliError::Upstream(format!(
                            "{what}: Google Drive sent an empty piece at byte {at}"
                        )));
                    }
                }
            }
            _ => {
                let answer = self.call(what, true, &[], &|http| {
                    http.get(&url).query(&[("alt", "media")])
                })?;
                write(out, &mut hash, &answer.body)?;
            }
        }
        let got = hex(&hash.finalize());
        match file.md5() {
            Some(want) if want != got => Err(CliError::Upstream(format!(
                "{what}: the download's md5 is {got}, not the {want} Google Drive reports; \
                 nothing was kept"
            ))),
            _ => Ok(()),
        }
    }

    /// Move a file to the trash (recoverable for 30 days, as rclone's
    /// default delete is). `false` when Drive has no such file.
    pub(crate) fn trash(&self, what: &str, id: &str) -> Result<bool, CliError> {
        let url = format!("{}/files/{}", self.endpoints.api, id);
        let answer = self.call(what, true, &[404], &|http| {
            http.patch(&url)
                .query(&[("fields", "id")])
                .json(&json!({"trashed": true}))
        })?;
        Ok(answer.status != 404)
    }

    /// Rename and move in one call (`addParents`/`removeParents`).
    pub(crate) fn rename_move(
        &self,
        what: &str,
        id: &str,
        name: &str,
        from_parent: &str,
        to_parent: &str,
    ) -> Result<DriveFile, CliError> {
        let url = format!("{}/files/{}", self.endpoints.api, id);
        let answer = self.call(what, true, &[], &|http| {
            let mut req = http.patch(&url).query(&[("fields", FIELDS)]);
            if from_parent != to_parent {
                req = req.query(&[("addParents", to_parent), ("removeParents", from_parent)]);
            }
            req.json(&json!({"name": name}))
        })?;
        answer.json(what)
    }

    pub(crate) fn about(&self) -> Result<About, CliError> {
        let what = "reading the account";
        let url = format!("{}/about", self.endpoints.api);
        let answer = self.call(what, true, &[], &|http| {
            http.get(&url).query(&[(
                "fields",
                "user(emailAddress,displayName),storageQuota(limit,usage,usageInDrive,usageInDriveTrash)",
            )])
        })?;
        let v: Value = answer.json(what)?;
        let num = |k: &str| {
            v.pointer(&format!("/storageQuota/{k}"))
                .and_then(Value::as_str)
                .and_then(|s| s.parse().ok())
        };
        let email = v
            .pointer("/user/emailAddress")
            .and_then(Value::as_str)
            .ok_or_else(|| CliError::Upstream(format!("{what}: Google Drive named no account")))?
            .to_string();
        Ok(About {
            email,
            display_name: v
                .pointer("/user/displayName")
                .and_then(Value::as_str)
                .map(str::to_string),
            limit: num("limit"),
            usage: num("usage"),
            usage_in_drive: num("usageInDrive"),
            usage_in_trash: num("usageInDriveTrash"),
        })
    }
}

/// Whether two URLs share scheme, host and port.
fn same_origin(a: &str, b: &str) -> bool {
    match (reqwest::Url::parse(a), reqwest::Url::parse(b)) {
        (Ok(a), Ok(b)) => {
            a.scheme() == b.scheme()
                && a.host_str() == b.host_str()
                && a.port_or_known_default() == b.port_or_known_default()
        }
        _ => false,
    }
}

/// The bytes a resumable session holds, from its `Range: bytes=0-<last>`.
fn received(answer: &Answer) -> u64 {
    answer
        .header("range")
        .and_then(|r| r.rsplit('-').next())
        .and_then(|last| last.trim().parse::<u64>().ok())
        .map_or(0, |last| last + 1)
}

/// `2026-01-02T03:04:05.678Z` (Drive's form, always UTC) as Unix seconds.
pub(crate) fn parse_rfc3339(s: &str) -> Option<i64> {
    let s = s.trim();
    let (date, time) = s.split_once('T')?;
    let mut d = date.splitn(3, '-').map(|p| p.parse::<i64>().ok());
    let (y, m, day) = (d.next()??, d.next()??, d.next()??);
    let time = time.strip_suffix('Z')?;
    let time = time.split('.').next()?;
    let mut t = time.splitn(3, ':').map(|p| p.parse::<i64>().ok());
    let (hh, mm, ss) = (t.next()??, t.next()??, t.next()??);
    if !(1..=12).contains(&m) || !(1..=31).contains(&day) || hh > 23 || mm > 59 || ss > 60 {
        return None;
    }
    // Days from the civil date (Howard Hinnant's algorithm).
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days * 86_400 + hh * 3600 + mm * 60 + ss)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drive_times_read_as_unix_seconds() {
        assert_eq!(parse_rfc3339("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(
            parse_rfc3339("2026-01-01T00:00:00.000Z"),
            Some(1_767_225_600)
        );
        assert_eq!(parse_rfc3339("2024-02-29T12:30:15.5Z"), Some(1_709_209_815));
        assert_eq!(
            pk_cli_core::dates::fmt_rfc3339(1_709_209_815),
            "2024-02-29T12:30:15Z"
        );
        assert_eq!(parse_rfc3339("2026-13-01T00:00:00Z"), None);
        assert_eq!(parse_rfc3339("2026-01-01 00:00:00Z"), None);
        assert_eq!(parse_rfc3339("2026-01-01T00:00:00+01:00"), None);
    }

    #[test]
    fn query_literals_escape_quotes_and_backslashes() {
        assert_eq!(quote("a'b\\c"), r"'a\'b\\c'");
        assert_eq!(quote("plain name.pdf"), "'plain name.pdf'");
    }

    #[test]
    fn listing_fields_read_drive_answers() {
        let page: Page =
            serde_json::from_str(include_str!("../../tests/fixtures/google/files-list.json"))
                .unwrap();
        assert_eq!(page.next_page_token.as_deref(), Some("example-page-2"));
        let [folder, pdf, doc] = page.files.as_slice() else {
            panic!("{:?}", page.files);
        };
        assert!(folder.is_folder() && !folder.is_blob());
        assert_eq!(folder.size(), None);
        assert_eq!(pdf.size(), Some(12_345));
        assert_eq!(
            pdf.md5().as_deref(),
            Some("0123456789abcdef0123456789abcdef")
        );
        assert_eq!(pdf.modified(), Some(1_767_225_600));
        assert_eq!(pdf.parents, vec!["1ExampleFolderId"]);
        assert!(!doc.is_blob());
    }
}
