//! The documents-domain profile (`documents/v1`, SPEC v1.1 §1.8).
//!
//! Almost every account portal publishes **files** — statements, escrow
//! analyses, tax forms, notices, meeting minutes — and almost every family CLI
//! grew its own spelling for "list them and pull the PDF": `pmac documents
//! list|download`, `fpl bills download -o`, `tojfl bills get -o`, `lrfl bill
//! --save`, `wabhoa statements list` (no download at all). A downstream
//! archiver (the `organize-scans` workflow) carried a per-CLI adapter table to
//! paper over the differences — the documents analog of the manifest hacks
//! `utility/v1` deleted. This crate owns the shared shape so that table
//! collapses to `<cli> documents list --json` + `<cli> documents download <id>
//! -o <path>`.
//!
//! Rules (see DESIGN.md §1.8 for the command spellings):
//! - `documents list` emits a [`Paged`] envelope of [`Document`] under `items`
//!   (`document-list/v1`), newest first, taking the shared [`RangeArgs`].
//! - `documents download <ID> -o <PATH>` writes one file and emits
//!   [`SavedDocument`] (`document-download/v1`); `--all` writes a directory and
//!   emits [`DownloadBatch`] (`document-download-batch/v1`).
//! - `documents open <ID>` (optional) hands the saved file to the system
//!   viewer and emits [`OpenedDocument`] (`document-open/v1`).
//! - Documents are **reads** — nothing here spends money or mutates the
//!   account — so no §1.3 confirmation surface applies.
//!
//! The list envelope and range flags come from `pk-cli-core` (they are
//! profile-agnostic); this crate re-exports them so an adopter needs one
//! `use pk_cli_documents::*`.
//!
//! A CLI advertises the profile via `info`:
//! `CliInfo::new(...).with_profiles(&[pk_cli_documents::PROFILE])`.

use pk_cli_core::output;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub mod verify;

pub use pk_cli_core::{Paged, RangeArgs};
pub use verify::{fs_safe, verify_download, verify_pdf};

/// Profile identifier for `cli-info/v1` `profiles`.
pub const PROFILE: &str = "documents/v1";

/// Emit any DTO per the output contract: pretty JSON in json mode, the
/// standard key/value block otherwise. (Lists use [`Paged::emit`] instead.)
pub fn emit<T: Serialize>(dto: &T, json_mode: bool) {
    let v = serde_json::to_value(dto).unwrap_or(Value::Null);
    if json_mode {
        output::json(&v);
    } else {
        output::render(&v);
    }
}

/// One published document, as listed by `documents list` (`document/v1`; it is
/// carried inside `document-list/v1`). Only what an archiver needs to file it
/// and fetch it — no financial fields; a statement's *amount* is the utility
/// profile's concern, not the file's.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Document {
    /// Stable identifier used by `documents download <ID>`. A string so GUID
    /// and numeric providers share one shape. Provider-controlled — like
    /// [`Document::file`], pass through [`verify::fs_safe`] if it ever
    /// becomes part of a path (e.g. an undated-document filename fallback).
    pub id: String,
    /// Issue/posting date, ISO `YYYY-MM-DD`, when the provider states one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub date: Option<String>,
    /// Human title/description ("June 2026 Statement", "2025 1098").
    pub name: String,
    /// Provider's document class — `statement` | `escrow` | `tax` | `notice` |
    /// `bylaws` | … — verbatim from the provider, not a closed set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    /// The portal's own filename, used to name the saved file when the caller
    /// gives no explicit path. **Provider-controlled** — pass it (or each
    /// component it is built from) through [`verify::fs_safe`] before it
    /// becomes a path, or a crafted response can traverse out of the output
    /// directory.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
}

impl Document {
    /// A document with just the required fields; set the optional ones after.
    pub fn new(id: impl Into<String>, name: impl Into<String>) -> Self {
        Document {
            id: id.into(),
            date: None,
            name: name.into(),
            category: None,
            file: None,
        }
    }
}

/// The result of writing one document to disk (`document-download/v1`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SavedDocument {
    pub schema: String,
    pub id: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub date: Option<String>,
    /// Filename written (the leaf of `path`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    /// Where it was written on disk.
    pub path: String,
    pub bytes: u64,
}

impl SavedDocument {
    /// Build the download result from the listed [`Document`] plus where it
    /// landed, so the two stay in sync.
    pub fn from_document(doc: &Document, path: impl Into<String>, bytes: u64) -> Self {
        SavedDocument {
            schema: "document-download/v1".into(),
            id: doc.id.clone(),
            name: doc.name.clone(),
            category: doc.category.clone(),
            date: doc.date.clone(),
            file: doc.file.clone(),
            path: path.into(),
            bytes,
        }
    }
}

/// A listed document that a `--all` download couldn't produce a file for
/// (e.g. the provider's archive has no PDF on record for it). Reported in
/// [`DownloadBatch::skipped`] so a partial batch doesn't silently under-report.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkippedDocument {
    /// The document id (from `documents list`) that was skipped.
    pub id: String,
    /// Why it was skipped, in human-readable form.
    pub reason: String,
}

impl SkippedDocument {
    pub fn new(id: impl Into<String>, reason: impl Into<String>) -> Self {
        SkippedDocument {
            id: id.into(),
            reason: reason.into(),
        }
    }
}

/// The result of a `--all` download (`document-download-batch/v1`): every file
/// written, plus the totals — and any listed documents that were skipped.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DownloadBatch {
    pub schema: String,
    pub count: u64,
    pub bytes_total: u64,
    /// Directory the batch was written to (`.` when the current dir).
    pub dir: String,
    pub items: Vec<SavedDocument>,
    /// Documents that couldn't be downloaded (e.g. no PDF on file). Omitted
    /// from JSON when empty, so a fully-successful batch is unchanged and older
    /// consumers see the same shape.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skipped: Vec<SkippedDocument>,
}

impl DownloadBatch {
    /// Total the written documents into the batch envelope (no skips).
    pub fn new(dir: impl Into<String>, items: Vec<SavedDocument>) -> Self {
        DownloadBatch::with_skipped(dir, items, Vec::new())
    }

    /// Total the written documents into the batch envelope, recording any
    /// listed documents that couldn't be produced.
    pub fn with_skipped(
        dir: impl Into<String>,
        items: Vec<SavedDocument>,
        skipped: Vec<SkippedDocument>,
    ) -> Self {
        DownloadBatch {
            schema: "document-download-batch/v1".into(),
            count: items.len() as u64,
            bytes_total: items.iter().map(|d| d.bytes).sum(),
            dir: dir.into(),
            items,
            skipped,
        }
    }
}

/// The result of `documents open <ID>` (`document-open/v1`): saved to a temp
/// file and handed to the system viewer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenedDocument {
    pub schema: String,
    pub id: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    pub path: String,
    /// The launcher used (`open` on macOS, `xdg-open` elsewhere).
    pub opened_with: String,
}

impl OpenedDocument {
    pub fn new(doc: &Document, path: impl Into<String>, opened_with: impl Into<String>) -> Self {
        OpenedDocument {
            schema: "document-open/v1".into(),
            id: doc.id.clone(),
            name: doc.name.clone(),
            file: doc.file.clone(),
            path: path.into(),
            opened_with: opened_with.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Document {
        let mut d = Document::new("1042", "June 2026 Statement");
        d.date = Some("2026-06-15".into());
        d.category = Some("statement".into());
        d.file = Some("2026-06-statement.pdf".into());
        d
    }

    #[test]
    fn document_list_shape() {
        let page = Paged::new("document", vec![sample()]);
        let v = serde_json::to_value(&page).unwrap();
        assert_eq!(v["schema"], "document-list/v1");
        assert_eq!(v["items"][0]["id"], "1042");
        assert_eq!(v["items"][0]["name"], "June 2026 Statement");
        assert_eq!(v["items"][0]["category"], "statement");
    }

    #[test]
    fn omitted_fields_stay_absent() {
        // A bare document — no date/category/file — omits them rather than
        // emitting null, per §1.4.
        let v = serde_json::to_value(Document::new("x", "Notice")).unwrap();
        assert!(v.get("date").is_none());
        assert!(v.get("category").is_none());
        assert!(v.get("file").is_none());
    }

    #[test]
    fn saved_document_mirrors_the_listing() {
        let saved = SavedDocument::from_document(&sample(), "/tmp/2026-06-statement.pdf", 4096);
        let v = serde_json::to_value(&saved).unwrap();
        assert_eq!(v["schema"], "document-download/v1");
        assert_eq!(v["id"], "1042");
        assert_eq!(v["path"], "/tmp/2026-06-statement.pdf");
        assert_eq!(v["bytes"], 4096);
    }

    #[test]
    fn batch_totals_the_items() {
        let items = vec![
            SavedDocument::from_document(&sample(), "/out/a.pdf", 100),
            SavedDocument::from_document(&sample(), "/out/b.pdf", 250),
        ];
        let batch = DownloadBatch::new("/out", items);
        let v = serde_json::to_value(&batch).unwrap();
        assert_eq!(v["schema"], "document-download-batch/v1");
        assert_eq!(v["count"], 2);
        assert_eq!(v["bytes_total"], 350);
        assert_eq!(v["dir"], "/out");
        // A fully-successful batch omits `skipped` entirely (back-compat).
        assert!(v.get("skipped").is_none());
    }

    #[test]
    fn batch_records_skipped() {
        let items = vec![SavedDocument::from_document(&sample(), "/out/a.pdf", 100)];
        let skipped = vec![SkippedDocument::new("2025-02-01", "no PDF on file")];
        let batch = DownloadBatch::with_skipped("/out", items, skipped);
        let v = serde_json::to_value(&batch).unwrap();
        // `count`/`bytes_total` still reflect only what was written…
        assert_eq!(v["count"], 1);
        assert_eq!(v["bytes_total"], 100);
        // …and skips are surfaced so a partial batch isn't silently short.
        assert_eq!(v["skipped"][0]["id"], "2025-02-01");
        assert_eq!(v["skipped"][0]["reason"], "no PDF on file");
        // Round-trips (Deserialize) and tolerates the field's absence.
        let back: DownloadBatch = serde_json::from_value(v).unwrap();
        assert_eq!(back.skipped.len(), 1);
        let none: DownloadBatch =
            serde_json::from_str(r#"{"schema":"document-download-batch/v1","count":0,"bytes_total":0,"dir":".","items":[]}"#)
                .unwrap();
        assert!(none.skipped.is_empty());
    }

    #[test]
    fn opened_document_shape() {
        let opened = OpenedDocument::new(&sample(), "/tmp/x.pdf", "open");
        let v = serde_json::to_value(&opened).unwrap();
        assert_eq!(v["schema"], "document-open/v1");
        assert_eq!(v["opened_with"], "open");
    }
}
