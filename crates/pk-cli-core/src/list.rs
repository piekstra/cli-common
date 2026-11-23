//! List primitives shared by every domain profile (SPEC v1.1 §1.8): the
//! [`Paged`] envelope every `list` command emits, and the [`RangeArgs`] flags
//! every `list` command takes.
//!
//! These are deliberately profile-agnostic. A `list` is a `list` whether the
//! records are utility statements, mortgage documents, or anything else, so the
//! envelope and the range flags live in `core` and each profile crate
//! (`pk-cli-utility`, `pk-cli-documents`, …) re-exports them. Keeping one copy
//! means a CLI that adopts two profiles gets one `RangeArgs`/`Paged` type, not
//! two identical ones — and a non-utility CLI no longer reaches into the
//! utility crate just to page a list.

use clap::Args;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{dates, output, CliError};

/// The list envelope every profile `list` command emits. Records live under
/// `items`; text mode renders them as the standard pipe table, and prints each
/// of `warnings` on stderr.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Paged<T> {
    /// `<record>-list/v1`, e.g. `statement-list/v1`, `document-list/v1`.
    pub schema: String,
    pub items: Vec<T>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
    /// Total available upstream, when known (items may be a `--limit` slice).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total: Option<u64>,
    /// Why `items` may be short of what the caller asked for (see
    /// [`ListWarning`]). Absent when empty, so an envelope without any reads
    /// exactly as before.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<ListWarning>,
}

/// A data-integrity note on one list: the provider reported more rows than it
/// served, a per-row detail fetch failed, the range runs past the data on
/// hand, the snapshot is older than the day asked for. A consumer treats a list
/// carrying any warning as possibly incomplete.
///
/// It lives in the envelope because the stderr note a CLI prints for people
/// does not reach a program: `--quiet` silences it and a `--json` consumer
/// rarely reads stderr. So a CLI never drops a warning under `--quiet`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListWarning {
    /// A stable kebab-case word a program branches on (`short-read`,
    /// `coverage-gap`, …). A CLI documents the codes it emits; a consumer
    /// treats an unknown code as a warning all the same.
    pub code: String,
    /// The same note for people.
    pub message: String,
}

impl<T: Serialize> Paged<T> {
    /// `record` is the singular schema name: `Paged::new("statement", items)`
    /// tags the envelope `statement-list/v1`.
    pub fn new(record: &str, items: Vec<T>) -> Self {
        Paged {
            schema: format!("{record}-list/v1"),
            items,
            next_cursor: None,
            total: None,
            warnings: Vec::new(),
        }
    }

    /// Record a data-integrity warning on the list (see [`ListWarning`]).
    pub fn warn(&mut self, code: impl Into<String>, message: impl Into<String>) -> &mut Self {
        self.warnings.push(ListWarning {
            code: code.into(),
            message: message.into(),
        });
        self
    }

    /// Emit per the output contract: the full envelope in json mode, the
    /// items as a pipe table otherwise.
    pub fn emit(&self, json_mode: bool) {
        let v = serde_json::to_value(self).unwrap_or(Value::Null);
        if json_mode {
            output::json(&v);
            return;
        }
        if let Some(items) = v.get("items").and_then(Value::as_array) {
            output::table(items);
        }
        for w in &self.warnings {
            eprintln!("warning: {}", w.message);
        }
    }
}

/// The universal range flags for profile list commands (SPEC v1.1 §1.8):
/// `--limit` is the pagination knob, `--since`/`--until` bound by ISO date.
#[derive(Args, Debug, Default, Clone)]
pub struct RangeArgs {
    /// Maximum records to return.
    #[arg(long, value_name = "N")]
    pub limit: Option<u32>,
    /// Only records on or after this date (ISO `YYYY-MM-DD`).
    #[arg(long, value_name = "YYYY-MM-DD")]
    pub since: Option<String>,
    /// Only records on or before this date (ISO `YYYY-MM-DD`).
    #[arg(long, value_name = "YYYY-MM-DD")]
    pub until: Option<String>,
}

impl RangeArgs {
    /// Validate the date bounds (usage error on malformed or inverted range).
    pub fn validate(&self) -> Result<(), CliError> {
        let since = self.since.as_deref().map(dates::parse_iso).transpose()?;
        let until = self.until.as_deref().map(dates::parse_iso).transpose()?;
        if let (Some(s), Some(u)) = (since, until) {
            if s > u {
                return Err(CliError::Usage(format!(
                    "--since {} is after --until {}",
                    self.since.as_deref().unwrap_or_default(),
                    self.until.as_deref().unwrap_or_default()
                )));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Serialize;

    #[derive(Serialize)]
    struct Row {
        id: String,
    }

    #[test]
    fn paged_envelope_shape() {
        let page = Paged::new("document", vec![Row { id: "abc".into() }]);
        let v = serde_json::to_value(&page).unwrap();
        assert_eq!(v["schema"], "document-list/v1");
        assert_eq!(v["items"][0]["id"], "abc");
        // Optional envelope fields stay absent until set.
        assert!(v.get("next_cursor").is_none());
        assert!(v.get("total").is_none());
        assert!(v.get("warnings").is_none());
    }

    #[test]
    fn warnings_serialize_in_order_and_old_envelopes_still_read() {
        let mut page = Paged::new("transaction", vec![Row { id: "t1".into() }]);
        page.warn("short-read", "reported 3 rows and served 1")
            .warn("coverage-gap", "no history before 2026-01-01");
        let v = serde_json::to_value(&page).unwrap();
        assert_eq!(v["warnings"][0]["code"], "short-read");
        assert_eq!(v["warnings"][0]["message"], "reported 3 rows and served 1");
        assert_eq!(v["warnings"][1]["code"], "coverage-gap");

        // An envelope written before the field existed deserializes with none.
        let old: Paged<serde_json::Value> =
            serde_json::from_str(r#"{"schema":"transaction-list/v1","items":[],"total":0}"#)
                .unwrap();
        assert!(old.warnings.is_empty());
        let back: Paged<serde_json::Value> = serde_json::from_value(v).unwrap();
        assert_eq!(back.warnings.len(), 2);
    }

    #[test]
    fn range_args_validate() {
        let ok = RangeArgs {
            limit: Some(10),
            since: Some("2026-01-01".into()),
            until: Some("2026-06-30".into()),
        };
        assert!(ok.validate().is_ok());

        let inverted = RangeArgs {
            limit: None,
            since: Some("2026-06-30".into()),
            until: Some("2026-01-01".into()),
        };
        assert!(matches!(inverted.validate(), Err(CliError::Usage(_))));

        let malformed = RangeArgs {
            limit: None,
            since: Some("06/30/2026".into()),
            until: None,
        };
        assert!(malformed.validate().is_err());
    }
}
