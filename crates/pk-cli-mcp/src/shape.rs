//! Output shaping: how much a list tool returns, and how the model asks for
//! more or narrows the question.
//!
//! - **Limits.** [`DEFAULT_LIMIT`] records unless the call asks for more, at
//!   most [`MAX_LIMIT`].
//! - **Cursors.** Opaque strings. A cursor records where the next page
//!   starts and a fingerprint of the query's filters, so a cursor replayed
//!   with different filters is refused rather than silently paging a
//!   different list.
//! - **Format.** [`ResponseFormat::Concise`] by default: the fields a person
//!   reads, names rather than ids. `detailed` adds ids, timestamps and
//!   provider detail.
//! - **Size budget.** A page stops early, before [`SIZE_BUDGET`] characters
//!   of JSON, and says so: `truncated: true` plus a `hint` naming the cursor and
//!   the tool's own ways to narrow. Claude Code warns above 10,000 tokens per
//!   result; 32,000 characters of JSON stays clear of that. Sizes count
//!   characters, not bytes, as the hosts' own caps do, so a page of
//!   non-ASCII names is measured as the model sees it.
//!
//! [`ListArgs`] is the argument block a list tool flattens into its input;
//! [`PageRequest`] resolves it, and [`PageRequest::page`] cuts the page.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::error::ToolError;

/// Records per page when the call names no `limit`.
pub const DEFAULT_LIMIT: u32 = 25;
/// The largest `limit` a call may ask for.
pub const MAX_LIMIT: u32 = 200;
/// Characters of compact JSON a list page stays under.
pub const SIZE_BUDGET: usize = 32_000;
/// Characters of compact JSON no result may exceed, list or not: a result over
/// it is replaced by an error telling the model to narrow the call.
pub const HARD_LIMIT: usize = 64_000;
/// Characters of a page's budget set aside for everything beside `items`: the
/// page fields and the tool's own top-level fields, which must stay under it.
pub const ENVELOPE_RESERVE: usize = 1_500;

/// How much of each record to return.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ResponseFormat {
    #[default]
    Concise,
    Detailed,
}

impl ResponseFormat {
    pub fn is_detailed(self) -> bool {
        self == Self::Detailed
    }
}

/// The paging and format arguments of a list tool. Flatten it into the
/// tool's input struct with `#[serde(flatten)]`; its fields join the tool's
/// own in the schema.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, JsonSchema)]
pub struct ListArgs {
    /// Most records to return, 1 to 200 (default 25).
    #[serde(default)]
    #[schemars(range(min = 1, max = 200))]
    pub limit: Option<u32>,
    /// The `next_cursor` of the previous page of this same query, to continue
    /// it; omit for the first page.
    #[serde(default)]
    pub cursor: Option<String>,
    /// `concise` (default): the fields a person reads. `detailed`: adds ids,
    /// timestamps and provider detail.
    #[serde(default)]
    pub response_format: Option<ResponseFormat>,
}

impl ListArgs {
    pub fn format(&self) -> ResponseFormat {
        self.response_format.unwrap_or_default()
    }
}

/// The page fields every list result carries beside `items`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PageInfo {
    /// Records in `items`.
    pub returned: usize,
    /// Pass back as `cursor` for the next page; absent on the last page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
    /// The page stopped early to stay under the size budget.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
    /// How to get the rest, or how to narrow the call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

/// One page of a list: the records and the page fields, flat.
#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
pub struct Page<T> {
    pub items: Vec<T>,
    #[serde(flatten)]
    pub info: PageInfo,
}

/// A resolved page request: how many, from where, under what budget.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageRequest {
    limit: usize,
    offset: usize,
    fingerprint: u64,
    budget: usize,
}

impl PageRequest {
    /// Resolve `args` for a query whose filters are `filters` (every argument
    /// that selects records; not `limit`, `cursor` or `response_format`).
    /// A bad limit, a cursor this tool did not issue, or a cursor issued for
    /// other filters is a usage error saying how to fix the call.
    pub fn new<F: Serialize + ?Sized>(args: &ListArgs, filters: &F) -> Result<Self, ToolError> {
        let limit = args.limit.unwrap_or(DEFAULT_LIMIT);
        if !(1..=MAX_LIMIT).contains(&limit) {
            return Err(ToolError::usage(format!(
                "limit must be between 1 and {MAX_LIMIT}; got {limit}"
            )));
        }
        let fingerprint = fingerprint(filters)?;
        let offset = match args.cursor.as_deref().map(str::trim) {
            None | Some("") => 0,
            Some(c) => {
                let (offset, fp) = decode_cursor(c).ok_or_else(|| {
                    ToolError::usage(
                        "cursor is not one this tool returned; pass a result's next_cursor \
                         exactly as received, or omit cursor to start from the first page",
                    )
                })?;
                if fp != fingerprint {
                    return Err(ToolError::usage(
                        "cursor belongs to a query with different filters; repeat the filters \
                         of the call that returned it, or omit cursor to start over",
                    ));
                }
                offset
            }
        };
        Ok(Self {
            limit: limit as usize,
            offset,
            fingerprint,
            budget: SIZE_BUDGET,
        })
    }

    /// A smaller budget, for tests that exercise truncation on small data.
    pub fn with_budget(mut self, chars: usize) -> Self {
        self.budget = chars;
        self
    }

    pub fn limit(&self) -> usize {
        self.limit
    }

    /// Where this page starts in the full list of matching records.
    pub fn offset(&self) -> usize {
        self.offset
    }

    /// How many matching records the tool must fetch, counted from the first,
    /// for [`Self::page`] to know whether another page exists: the page's
    /// end plus one.
    pub fn wanted(&self) -> usize {
        self.offset.saturating_add(self.limit).saturating_add(1)
    }

    /// Cut this page out of `matched`, every record matching the filters in
    /// order from the first (at least [`Self::wanted`] of them when that many
    /// exist). `narrow` names the tool's own narrowing arguments for the
    /// hint, e.g. "since/until or type".
    pub fn page<T: Serialize>(&self, matched: Vec<T>, narrow: &str) -> Result<Page<T>, ToolError> {
        self.page_with_more(matched, false, narrow)
    }

    /// As [`Self::page`], for a tool that fetched only part of the list:
    /// `more_upstream` says records exist beyond `matched`.
    pub fn page_with_more<T: Serialize>(
        &self,
        matched: Vec<T>,
        more_upstream: bool,
        narrow: &str,
    ) -> Result<Page<T>, ToolError> {
        let available = matched.len().saturating_sub(self.offset);
        let item_budget = self.budget.saturating_sub(ENVELOPE_RESERVE);
        let mut items = Vec::new();
        let mut used = 2; // the array's brackets
        for item in matched.into_iter().skip(self.offset).take(self.limit) {
            let size = serde_json::to_string(&item)
                .map_err(|e| ToolError::other(format!("serializing a record: {e}")))?
                .chars()
                .count()
                + 1; // its comma
            if used + size > item_budget {
                break;
            }
            used += size;
            items.push(item);
        }
        let kept = items.len();
        if kept == 0 && available > 0 {
            return Err(ToolError::usage(format!(
                "a single record is larger than the {}-character result budget; call again \
                 with response_format=concise, or narrow with {narrow}",
                self.budget
            )));
        }
        let truncated = kept < self.limit.min(available);
        let more = kept < available || more_upstream;
        let next_cursor = more.then(|| encode_cursor(self.offset + kept, self.fingerprint));
        // `offset` is bounded by MAX_OFFSET on decode, so the sum is small.
        let hint = if truncated {
            Some(format!(
                "Stopped at {kept} records to stay under the {}-character result budget. Pass \
                 next_cursor as cursor for the rest, or narrow with {narrow} or \
                 response_format=concise.",
                self.budget
            ))
        } else if more {
            Some(format!(
                "More records match: pass next_cursor as cursor for the next page, or narrow \
                 with {narrow}."
            ))
        } else {
            None
        };
        Ok(Page {
            items,
            info: PageInfo {
                returned: kept,
                next_cursor,
                truncated,
                hint,
            },
        })
    }
}

/// FNV-1a over the filters' compact JSON. Not a security boundary (a cursor
/// grants nothing a call could not ask for directly); it only catches a
/// cursor replayed against a different query. Filters that cannot be
/// serialized are an error, not a shared constant fingerprint that would let
/// any cursor through.
fn fingerprint<F: Serialize + ?Sized>(filters: &F) -> Result<u64, ToolError> {
    let bytes = serde_json::to_vec(filters)
        .map_err(|e| ToolError::other(format!("serializing the list filters: {e}")))?;
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    Ok(hash)
}

const CURSOR_VERSION: &str = "c1";

/// The largest offset a cursor may carry. Cursors come back from the client,
/// which can hand-build one, and no real list pages this far.
const MAX_OFFSET: usize = 10_000_000;

/// Hex of `c1:<offset>:<fingerprint>`: opaque to a reader, cheap to check.
fn encode_cursor(offset: usize, fingerprint: u64) -> String {
    let plain = format!("{CURSOR_VERSION}:{offset}:{fingerprint:016x}");
    plain.bytes().map(|b| format!("{b:02x}")).collect()
}

fn decode_cursor(cursor: &str) -> Option<(usize, u64)> {
    if !cursor.len().is_multiple_of(2) || cursor.len() > 128 {
        return None;
    }
    let bytes: Vec<u8> = (0..cursor.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(cursor.get(i..i + 2)?, 16).ok())
        .collect::<Option<_>>()?;
    let plain = String::from_utf8(bytes).ok()?;
    let mut parts = plain.split(':');
    if parts.next()? != CURSOR_VERSION {
        return None;
    }
    let offset: usize = parts.next()?.parse().ok()?;
    if offset > MAX_OFFSET {
        return None;
    }
    let fp = u64::from_str_radix(parts.next()?, 16).ok()?;
    parts.next().is_none().then_some((offset, fp))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn args(limit: Option<u32>, cursor: Option<&str>) -> ListArgs {
        ListArgs {
            limit,
            cursor: cursor.map(String::from),
            response_format: None,
        }
    }

    #[test]
    fn limits_default_to_25_and_cap_at_200() {
        let r = PageRequest::new(&args(None, None), &()).unwrap();
        assert_eq!(r.limit(), 25);
        assert_eq!(r.wanted(), 26);
        assert!(PageRequest::new(&args(Some(200), None), &()).is_ok());
        for bad in [0, 201] {
            let e = PageRequest::new(&args(Some(bad), None), &()).unwrap_err();
            assert!(e.message().contains("between 1 and 200"), "{e}");
        }
    }

    #[test]
    fn cursors_walk_the_list_without_gaps_or_repeats() {
        let all: Vec<u32> = (0..60).collect();
        let filters = json!({"type": "buy"});
        let mut seen = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let r = PageRequest::new(&args(Some(25), cursor.as_deref()), &filters).unwrap();
            let page = r.page(all.clone(), "type").unwrap();
            seen.extend(page.items);
            match page.info.next_cursor {
                Some(c) => cursor = Some(c),
                None => {
                    assert!(page.info.hint.is_none());
                    break;
                }
            }
        }
        assert_eq!(seen, all);
    }

    #[test]
    fn a_cursor_for_other_filters_or_a_made_up_one_is_refused() {
        let page = PageRequest::new(&args(Some(1), None), &json!({"a": 1}))
            .unwrap()
            .page(vec![1, 2, 3], "a")
            .unwrap();
        let cursor = page.info.next_cursor.unwrap();
        assert!(!cursor.contains(':'), "opaque: {cursor}");
        let e = PageRequest::new(&args(None, Some(&cursor)), &json!({"a": 2})).unwrap_err();
        assert!(e.message().contains("different filters"), "{e}");
        for junk in ["abc", "zz", "6331", "page-2"] {
            let e = PageRequest::new(&args(None, Some(junk)), &json!({"a": 1})).unwrap_err();
            assert!(e.message().contains("next_cursor exactly"), "{junk}: {e}");
        }
    }

    /// A client can hand-build a cursor with the right fingerprint (FNV is
    /// unkeyed) and any offset. A huge one is refused, never added to.
    #[test]
    fn a_forged_cursor_with_a_huge_offset_is_refused_not_overflowed() {
        let fp = fingerprint(&json!({"a": 1})).unwrap();
        for offset in [usize::MAX, MAX_OFFSET + 1] {
            let forged = encode_cursor(offset, fp);
            let e = PageRequest::new(&args(None, Some(&forged)), &json!({"a": 1})).unwrap_err();
            assert!(e.message().contains("next_cursor exactly"), "{offset}: {e}");
        }
        let edge = encode_cursor(MAX_OFFSET, fp);
        let r = PageRequest::new(&args(Some(MAX_LIMIT), Some(&edge)), &json!({"a": 1})).unwrap();
        assert_eq!(r.wanted(), MAX_OFFSET + MAX_LIMIT as usize + 1);
        let page = r.page(vec![1, 2, 3], "a").unwrap();
        assert!(page.items.is_empty() && page.info.next_cursor.is_none());
    }

    /// Filters that fail to serialize must not share one fingerprint.
    #[test]
    fn filters_that_cannot_be_serialized_are_an_error() {
        struct Unserializable;
        impl Serialize for Unserializable {
            fn serialize<S: serde::Serializer>(&self, _: S) -> Result<S::Ok, S::Error> {
                Err(serde::ser::Error::custom("no"))
            }
        }
        let e = PageRequest::new(&args(None, None), &Unserializable).unwrap_err();
        assert!(e.message().contains("serializing the list filters"), "{e}");
    }

    /// The budget counts characters, as the hosts do: a page of multi-byte
    /// names is not cut short by its UTF-8 length.
    #[test]
    fn the_budget_counts_characters_not_bytes() {
        let rows: Vec<String> = (0..10).map(|_| "é".repeat(90)).collect();
        // Each record is 92 characters plus a comma but 182 bytes plus one.
        let page = PageRequest::new(&args(Some(10), None), &())
            .unwrap()
            .with_budget(ENVELOPE_RESERVE + 2 + 93 * 10)
            .page(rows, "x")
            .unwrap();
        assert_eq!(page.info.returned, 10, "{:?}", page.info);
        assert!(!page.info.truncated);
    }

    #[test]
    fn a_page_over_budget_stops_early_and_says_how_to_narrow() {
        let rows: Vec<String> = (0..50).map(|i| format!("{i:0>90}")).collect();
        let r = PageRequest::new(&args(Some(50), None), &())
            .unwrap()
            .with_budget(ENVELOPE_RESERVE + 1_000);
        let page = r.page(rows, "since/until").unwrap();
        assert!(page.info.truncated);
        assert!(page.items.len() < 50 && !page.items.is_empty());
        assert_eq!(page.info.returned, page.items.len());
        let hint = page.info.hint.as_deref().unwrap();
        assert!(
            hint.contains("since/until") && hint.contains("next_cursor"),
            "{hint}"
        );
        assert!(page.info.next_cursor.is_some());
        assert!(serde_json::to_string(&page.items).unwrap().len() <= 1_000);
    }

    #[test]
    fn a_full_list_under_budget_is_neither_truncated_nor_paged() {
        let page = PageRequest::new(&args(None, None), &())
            .unwrap()
            .page(vec!["a", "b"], "x")
            .unwrap();
        let v = serde_json::to_value(&page).unwrap();
        assert_eq!(v, json!({"items": ["a", "b"], "returned": 2}));
    }

    #[test]
    fn more_upstream_keeps_a_cursor_on_a_short_fetch() {
        let page = PageRequest::new(&args(Some(2), None), &())
            .unwrap()
            .page_with_more(vec![1, 2], true, "x")
            .unwrap();
        assert!(page.info.next_cursor.is_some());
        assert!(!page.info.truncated);
    }

    #[test]
    fn one_record_over_budget_is_a_usage_error_not_an_endless_empty_page() {
        let r = PageRequest::new(&args(None, None), &())
            .unwrap()
            .with_budget(ENVELOPE_RESERVE + 10);
        let e = r.page(vec!["x".repeat(100)], "type").unwrap_err();
        assert!(e.message().contains("response_format=concise"), "{e}");
    }

    #[test]
    fn a_stale_cursor_past_the_end_yields_an_empty_last_page() {
        let first = PageRequest::new(&args(Some(2), None), &())
            .unwrap()
            .page(vec![1, 2, 3], "x")
            .unwrap();
        let cursor = first.info.next_cursor.unwrap();
        let page = PageRequest::new(&args(Some(2), Some(&cursor)), &())
            .unwrap()
            .page(vec![1], "x")
            .unwrap();
        assert!(page.items.is_empty());
        assert!(page.info.next_cursor.is_none());
    }

    #[test]
    fn response_format_defaults_to_concise() {
        assert_eq!(ListArgs::default().format(), ResponseFormat::Concise);
        let a: ListArgs = serde_json::from_value(json!({"response_format": "detailed"})).unwrap();
        assert!(a.format().is_detailed());
        assert!(serde_json::from_value::<ListArgs>(json!({"response_format": "full"})).is_err());
    }
}
