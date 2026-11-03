//! Output shaping: how much a list tool returns, and how the model asks for
//! more or narrows the question.
//!
//! - **Limits.** [`DEFAULT_LIMIT`] records unless the call asks for more, at
//!   most [`MAX_LIMIT`].
//! - **Cursors.** Opaque strings. A cursor records where the next page
//!   starts and a fingerprint of the query's filters, so a cursor replayed
//!   with different filters is refused rather than silently paging a
//!   different list. Where the next page starts is a count of rows
//!   ([`PageRequest::page`]), or, for a list whose rows can arrive or leave
//!   between pages, the last row's own position ([`PageRequest::page_by_position`]),
//!   so a new row cannot repeat one and a removed row cannot skip one.
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
//! [`PageRequest`] resolves it, and [`PageRequest::page`] or
//! [`PageRequest::page_by_position`] cuts the page.

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

/// Which way a list runs, by its rows' position keys ([`PageRequest::page_by_position`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Order {
    /// Largest key first: newest first, for a key that starts with an ISO date. A row
    /// with no date, whose key starts with nothing, comes last.
    Descending,
    /// Smallest key first: by symbol or by name.
    Ascending,
}

impl Order {
    fn cmp(self, a: &str, b: &str) -> std::cmp::Ordering {
        match self {
            Self::Ascending => a.cmp(b),
            Self::Descending => b.cmp(a),
        }
    }
}

/// A row's place in a list: its key, and how many rows before it share that key.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Position {
    key: String,
    repeat: usize,
}

/// A resolved page request: how many, from where, under what budget.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageRequest {
    limit: usize,
    offset: usize,
    position: Option<Position>,
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
        let (offset, position) = match args.cursor.as_deref().map(str::trim) {
            None | Some("") => (0, None),
            Some(c) => {
                let cursor = decode_cursor(c).ok_or_else(foreign_cursor)?;
                if cursor.fingerprint != fingerprint {
                    return Err(ToolError::usage(
                        "cursor belongs to a query with different filters; repeat the filters \
                         of the call that returned it, or omit cursor to start over",
                    ));
                }
                (cursor.offset, cursor.position)
            }
        };
        Ok(Self {
            limit: limit as usize,
            offset,
            position,
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

    /// Where this page starts in the full list of matching records. Under a position
    /// cursor, where it started when the cursor was issued, which rows that arrived
    /// since have moved: only an estimate of how far to read.
    pub fn offset(&self) -> usize {
        self.offset
    }

    /// How many matching records the tool must fetch, counted from the first,
    /// for [`Self::page`] to know whether another page exists: the page's
    /// end plus one.
    ///
    /// Under a position cursor, rows that arrived since push the position down, so this
    /// many may hold fewer rows of the page, and not the extra one. A tool that reads only
    /// this far says so through [`Self::page_by_position_with_more`]'s `more_upstream`:
    /// the page may come up short, and it still continues.
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
        if self.position.is_some() {
            return Err(foreign_cursor());
        }
        let available = matched.len().saturating_sub(self.offset);
        let rows = matched.into_iter().skip(self.offset);
        self.cut(rows, available, more_upstream, narrow, |kept| {
            encode_cursor(self.offset + kept, self.fingerprint)
        })
    }

    /// [`Self::page`] for a list whose rows can arrive, leave or change between pages:
    /// the next cursor holds the last row's position rather than a count of rows.
    ///
    /// `keyed` pairs every record matching the filters with its key, a string that says
    /// where the row belongs and does not change while the row exists, such as
    /// `format!("{date} {id}")`. A count of rows would repeat one when a new row arrives
    /// above the page's end, and skip one when a row above it leaves; a position does
    /// neither, and a total added up across pages stays right. Rows that arrive above a
    /// position already passed are not seen until the list is read again from the start,
    /// and a row whose key changes moves with it.
    ///
    /// The rows are sorted here by key in `order`, stably, so a tool cannot return them
    /// in an order its cursor disagrees with. Rows sharing a key keep the order they came
    /// in, and are told apart by count. A key over [`MAX_KEY_BYTES`] is cut to it.
    ///
    /// The key travels in the cursor, encoded and not encrypted: build it from what the
    /// tool's `detailed` format already shows. A cursor from a count-of-rows page of this
    /// tool, held across an upgrade, is honoured as that count.
    pub fn page_by_position<T: Serialize>(
        &self,
        keyed: Vec<(String, T)>,
        order: Order,
        narrow: &str,
    ) -> Result<Page<T>, ToolError> {
        self.page_by_position_with_more(keyed, false, order, narrow)
    }

    /// As [`Self::page_by_position`], for a tool that fetched only part of the list:
    /// `more_upstream` says records exist beyond `keyed`.
    pub fn page_by_position_with_more<T: Serialize>(
        &self,
        mut keyed: Vec<(String, T)>,
        more_upstream: bool,
        order: Order,
        narrow: &str,
    ) -> Result<Page<T>, ToolError> {
        for (key, _) in &mut keyed {
            cut_key(key);
        }
        keyed.sort_by(|a, b| order.cmp(&a.0, &b.0));
        let (keys, rows): (Vec<String>, Vec<T>) = keyed.into_iter().unzip();
        let start = match &self.position {
            Some(at) => position_after(&keys, at, order),
            None => self.offset.min(keys.len()),
        };
        let available = keys.len() - start;
        let rows = rows.into_iter().skip(start);
        self.cut(rows, available, more_upstream, narrow, |kept| {
            let next = start + kept;
            let position = match next.checked_sub(1).filter(|_| kept > 0) {
                Some(last) => Some(position_of(&keys, last, order)),
                None => self.position.clone(),
            };
            match position {
                Some(position) => encode_position_cursor(next, self.fingerprint, &position),
                None => encode_cursor(next, self.fingerprint),
            }
        })
    }

    /// Fill a page from `rows`, the list from where this page starts, of which
    /// `available` were read; `next_cursor` makes the cursor once the count kept is known.
    fn cut<T: Serialize>(
        &self,
        rows: impl Iterator<Item = T>,
        available: usize,
        more_upstream: bool,
        narrow: &str,
        next_cursor: impl FnOnce(usize) -> String,
    ) -> Result<Page<T>, ToolError> {
        let item_budget = self.budget.saturating_sub(ENVELOPE_RESERVE);
        let mut items = Vec::new();
        let mut used = 2; // the array's brackets
        for item in rows.take(self.limit) {
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
        // `offset` is bounded by MAX_OFFSET on decode, so a count of rows stays small.
        let next_cursor = more.then(|| next_cursor(kept));
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

fn foreign_cursor() -> ToolError {
    ToolError::usage(
        "cursor is not one this tool returned; pass a result's next_cursor exactly as \
         received, or omit cursor to start from the first page",
    )
}

/// The most of a position key a cursor holds. A key is a date and an id; this bounds a
/// cursor when a provider's id is not what it looks like.
pub const MAX_KEY_BYTES: usize = 200;

/// Cut at a character boundary, for every row and the cursor alike, so a long key still
/// compares the same on the next page.
fn cut_key(key: &mut String) {
    if key.len() > MAX_KEY_BYTES {
        let end = (0..=MAX_KEY_BYTES)
            .rev()
            .find(|i| key.is_char_boundary(*i))
            .unwrap_or(0);
        key.truncate(end);
    }
}

/// How many rows of `keys`, sorted by `order`, come at or before `at`.
fn position_after(keys: &[String], at: &Position, order: Order) -> usize {
    let first = keys.partition_point(|k| order.cmp(k, &at.key).is_lt());
    let past = keys.partition_point(|k| order.cmp(k, &at.key).is_le());
    first.saturating_add(at.repeat).saturating_add(1).min(past)
}

/// The position of the row at `index` of `keys`, sorted by `order`.
fn position_of(keys: &[String], index: usize, order: Order) -> Position {
    let key = &keys[index];
    Position {
        key: key.clone(),
        repeat: index - keys.partition_point(|k| order.cmp(k, key).is_lt()),
    }
}

const CURSOR_VERSION: &str = "c1";
const POSITION_CURSOR_VERSION: &str = "c2";

/// The largest offset a cursor may carry. Cursors come back from the client,
/// which can hand-build one, and no real list pages this far.
const MAX_OFFSET: usize = 10_000_000;

/// The hex of the longest cursor: a position's key, and room for the version and counts.
const MAX_CURSOR_CHARS: usize = (MAX_KEY_BYTES + 64) * 2;

/// A cursor as it decodes.
struct DecodedCursor {
    offset: usize,
    fingerprint: u64,
    position: Option<Position>,
}

fn to_hex(plain: &str) -> String {
    plain.bytes().map(|b| format!("{b:02x}")).collect()
}

/// Hex of `c1:<offset>:<fingerprint>`: opaque to a reader, cheap to check.
fn encode_cursor(offset: usize, fingerprint: u64) -> String {
    to_hex(&format!("{CURSOR_VERSION}:{offset}:{fingerprint:016x}"))
}

/// Hex of `c2:<offset>:<fingerprint>:<repeat>:<key>`, the key last because it may hold a
/// colon. The offset only estimates how far to read ([`PageRequest::wanted`]).
fn encode_position_cursor(offset: usize, fingerprint: u64, position: &Position) -> String {
    to_hex(&format!(
        "{POSITION_CURSOR_VERSION}:{offset}:{fingerprint:016x}:{}:{}",
        position.repeat, position.key
    ))
}

fn decode_cursor(cursor: &str) -> Option<DecodedCursor> {
    if !cursor.len().is_multiple_of(2) || cursor.len() > MAX_CURSOR_CHARS {
        return None;
    }
    let bytes: Vec<u8> = (0..cursor.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(cursor.get(i..i + 2)?, 16).ok())
        .collect::<Option<_>>()?;
    let plain = String::from_utf8(bytes).ok()?;
    let mut parts = plain.splitn(5, ':');
    let version = parts.next()?;
    let offset: usize = parts.next()?.parse().ok()?;
    if offset > MAX_OFFSET {
        return None;
    }
    let fingerprint = u64::from_str_radix(parts.next()?, 16).ok()?;
    let position = match version {
        CURSOR_VERSION => None,
        POSITION_CURSOR_VERSION => {
            let repeat = parts.next()?.parse().ok()?;
            let key = parts.next()?;
            (key.len() <= MAX_KEY_BYTES).then(|| Position {
                key: key.to_string(),
                repeat,
            })
        }
        _ => return None,
    };
    if position.is_none() && version != CURSOR_VERSION {
        return None;
    }
    parts.next().is_none().then_some(DecodedCursor {
        offset,
        fingerprint,
        position,
    })
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

    /// `(key, row)` for rows named by date and id.
    fn keyed(rows: &[(&str, &str)]) -> Vec<(String, String)> {
        rows.iter()
            .map(|(date, id)| (format!("{date} {id}"), format!("{date}/{id}")))
            .collect()
    }

    fn ledger() -> Vec<(&'static str, &'static str)> {
        vec![
            ("2026-03-05", "e"),
            ("2026-03-04", "d"),
            ("2026-03-03", "c"),
            ("2026-03-02", "b"),
            ("2026-03-01", "a"),
        ]
    }

    /// One page of `rows` after `cursor`, newest first.
    fn newest_page(
        rows: &[(&str, &str)],
        cursor: Option<&str>,
        limit: u32,
    ) -> (Vec<String>, Option<String>) {
        let page = PageRequest::new(&args(Some(limit), cursor), &())
            .unwrap()
            .page_by_position(keyed(rows), Order::Descending, "since/until")
            .unwrap();
        (page.items, page.info.next_cursor)
    }

    #[test]
    fn a_position_cursor_walks_a_list_without_gaps_or_repeats() {
        let mut seen = Vec::new();
        let mut cursor = None;
        loop {
            let (items, next) = newest_page(&ledger(), cursor.as_deref(), 2);
            seen.extend(items);
            match next {
                Some(c) => cursor = Some(c),
                None => break,
            }
        }
        assert_eq!(
            seen,
            [
                "2026-03-05/e",
                "2026-03-04/d",
                "2026-03-03/c",
                "2026-03-02/b",
                "2026-03-01/a"
            ]
        );
    }

    /// The scenario on schwab-cli #6: a trade posts between pages, which an offset would
    /// answer by showing a row twice.
    #[test]
    fn a_row_that_arrives_between_pages_repeats_nothing() {
        let (first, cursor) = newest_page(&ledger(), None, 2);
        assert_eq!(first, ["2026-03-05/e", "2026-03-04/d"]);
        let mut grown = vec![("2026-03-06", "f")];
        grown.extend(ledger());
        let (second, _) = newest_page(&grown, cursor.as_deref(), 2);
        assert_eq!(second, ["2026-03-03/c", "2026-03-02/b"]);
    }

    /// And the other way, which an offset answers by skipping a row, and a total across
    /// pages comes out wrong with nothing to show it.
    #[test]
    fn a_row_that_leaves_between_pages_skips_nothing() {
        let (_, cursor) = newest_page(&ledger(), None, 2);
        let mut fewer = ledger();
        fewer.retain(|(_, id)| *id != "e");
        let (second, _) = newest_page(&fewer, cursor.as_deref(), 2);
        assert_eq!(second, ["2026-03-03/c", "2026-03-02/b"]);
    }

    #[test]
    fn the_row_a_cursor_stands_on_may_leave_too() {
        let (_, cursor) = newest_page(&ledger(), None, 2);
        let mut fewer = ledger();
        fewer.retain(|(_, id)| *id != "d");
        let (second, _) = newest_page(&fewer, cursor.as_deref(), 2);
        assert_eq!(second, ["2026-03-03/c", "2026-03-02/b"]);
    }

    /// A tool sorts here, so its rows and its cursors cannot disagree.
    #[test]
    fn rows_are_sorted_by_key_in_the_order_asked_for() {
        let mut shuffled = ledger();
        shuffled.swap(0, 3);
        shuffled.swap(1, 4);
        let (items, _) = newest_page(&shuffled, None, 5);
        assert_eq!(items[0], "2026-03-05/e");
        assert_eq!(items[4], "2026-03-01/a");

        let page = PageRequest::new(&args(Some(1), None), &())
            .unwrap()
            .page_by_position(keyed(&shuffled), Order::Ascending, "symbol")
            .unwrap();
        assert_eq!(page.items, ["2026-03-01/a"]);
        let next = PageRequest::new(&args(Some(1), page.info.next_cursor.as_deref()), &())
            .unwrap()
            .page_by_position(keyed(&shuffled), Order::Ascending, "symbol")
            .unwrap();
        assert_eq!(next.items, ["2026-03-02/b"]);
    }

    #[test]
    fn an_undated_row_comes_last_when_newest_is_first() {
        let rows = vec![
            (" z".to_string(), "undated"),
            ("2026-03-01 a".to_string(), "dated"),
        ];
        let page = PageRequest::new(&args(None, None), &())
            .unwrap()
            .page_by_position(rows, Order::Descending, "x")
            .unwrap();
        assert_eq!(page.items, ["dated", "undated"]);
    }

    /// Two rows with nothing to tell them apart, split by a page boundary: neither is
    /// lost, and they keep the order they came in.
    #[test]
    fn rows_sharing_a_key_are_told_apart_by_count() {
        let rows = || {
            vec![
                ("2026-03-02 same".to_string(), "first"),
                ("2026-03-02 same".to_string(), "second"),
                ("2026-03-02 same".to_string(), "third"),
                ("2026-03-01 a".to_string(), "older"),
            ]
        };
        let mut seen = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let page = PageRequest::new(&args(Some(1), cursor.as_deref()), &())
                .unwrap()
                .page_by_position(rows(), Order::Descending, "x")
                .unwrap();
            seen.extend(page.items);
            match page.info.next_cursor {
                Some(c) => cursor = Some(c),
                None => break,
            }
        }
        assert_eq!(seen, ["first", "second", "third", "older"]);
    }

    /// A count that outran the rows with that key must not spill into the next key.
    #[test]
    fn a_count_past_the_rows_sharing_a_key_stops_at_the_next_key() {
        let fp = fingerprint(&()).unwrap();
        let forged = encode_position_cursor(
            0,
            fp,
            &Position {
                key: "2026-03-03 c".into(),
                repeat: usize::MAX,
            },
        );
        let (items, _) = newest_page(&ledger(), Some(&forged), 1);
        assert_eq!(items, ["2026-03-02/b"]);
    }

    #[test]
    fn a_key_with_a_colon_or_past_the_limit_still_finds_its_place() {
        let long = "x".repeat(MAX_KEY_BYTES + 50);
        let rows = || {
            vec![
                (format!("2026-03-03T10:00:00 {long}a"), "c"),
                (format!("2026-03-02T10:00:00 {long}b"), "b"),
                ("2026-03-01T10:00:00".to_string(), "a"),
            ]
        };
        let page = PageRequest::new(&args(Some(1), None), &())
            .unwrap()
            .page_by_position(rows(), Order::Descending, "x")
            .unwrap();
        assert_eq!(page.items, ["c"]);
        let cursor = page.info.next_cursor.unwrap();
        assert!(cursor.len() <= MAX_CURSOR_CHARS, "{}", cursor.len());
        let page = PageRequest::new(&args(Some(1), Some(&cursor)), &())
            .unwrap()
            .page_by_position(rows(), Order::Descending, "x")
            .unwrap();
        assert_eq!(page.items, ["b"]);
    }

    #[test]
    fn a_long_key_is_cut_on_a_character_boundary() {
        let mut key = "é".repeat(MAX_KEY_BYTES);
        cut_key(&mut key);
        assert!(key.len() <= MAX_KEY_BYTES);
        assert!(key.chars().all(|c| c == 'é'));
    }

    #[test]
    fn a_position_cursor_stays_bound_to_its_filters() {
        let page = PageRequest::new(&args(Some(1), None), &json!({"a": 1}))
            .unwrap()
            .page_by_position(keyed(&ledger()), Order::Descending, "a")
            .unwrap();
        let cursor = page.info.next_cursor.unwrap();
        assert!(!cursor.contains(':'), "opaque: {cursor}");
        let e = PageRequest::new(&args(None, Some(&cursor)), &json!({"a": 2})).unwrap_err();
        assert!(e.message().contains("different filters"), "{e}");
    }

    /// A tool has one kind of cursor; the other kind reaching `page` means it was not
    /// this tool's, and counting from its estimate would page the wrong rows.
    #[test]
    fn a_count_of_rows_page_refuses_a_position_cursor() {
        let (_, cursor) = newest_page(&ledger(), None, 1);
        let r = PageRequest::new(&args(None, cursor.as_deref()), &()).unwrap();
        let e = r.page(vec![1, 2, 3], "x").unwrap_err();
        assert!(e.message().contains("next_cursor exactly"), "{e}");
    }

    /// A client held a cursor from before its tool moved to positions.
    #[test]
    fn a_position_page_honours_a_cursor_from_before_the_move() {
        let older = PageRequest::new(&args(Some(2), None), &())
            .unwrap()
            .page(vec![1, 2, 3, 4, 5], "x")
            .unwrap();
        let cursor = older.info.next_cursor.unwrap();
        let (items, next) = newest_page(&ledger(), Some(&cursor), 2);
        assert_eq!(items, ["2026-03-03/c", "2026-03-02/b"]);
        let (rest, _) = newest_page(&ledger(), next.as_deref(), 2);
        assert_eq!(rest, ["2026-03-01/a"]);
    }

    /// Hand-built cursors come back from clients, as with the offsets above.
    #[test]
    fn a_forged_position_cursor_is_refused_or_harmless() {
        let fp = fingerprint(&()).unwrap();
        for forged in [
            to_hex(&format!("c2:0:{fp:016x}")),
            to_hex(&format!("c2:0:{fp:016x}:x:key")),
            to_hex(&format!("c2:0:{fp:016x}:0")),
            to_hex(&format!("c2:{}:{fp:016x}:0:key", MAX_OFFSET + 1)),
            to_hex(&format!("c3:0:{fp:016x}:0:key")),
            to_hex(&format!("c1:0:{fp:016x}:extra")),
            to_hex(&format!(
                "c2:0:{fp:016x}:0:{}",
                "k".repeat(MAX_KEY_BYTES + 1)
            )),
            "ab".repeat(MAX_CURSOR_CHARS),
        ] {
            let e = PageRequest::new(&args(None, Some(&forged)), &()).unwrap_err();
            assert!(e.message().contains("next_cursor exactly"), "{forged}: {e}");
        }
        let past_the_end = encode_position_cursor(
            0,
            fp,
            &Position {
                key: "0000".into(),
                repeat: 0,
            },
        );
        let (items, next) = newest_page(&ledger(), Some(&past_the_end), 2);
        assert!(items.is_empty());
        assert!(next.is_none());
    }

    #[test]
    fn a_position_page_over_budget_stops_early_and_continues_where_it_stopped() {
        let rows = || -> Vec<(String, String)> {
            (0..50)
                .map(|i| (format!("{i:04}"), format!("{i:0>90}")))
                .collect()
        };
        let r = PageRequest::new(&args(Some(50), None), &())
            .unwrap()
            .with_budget(ENVELOPE_RESERVE + 1_000);
        let page = r
            .page_by_position(rows(), Order::Ascending, "since/until")
            .unwrap();
        assert!(page.info.truncated);
        let kept = page.items.len();
        assert!(kept < 50 && kept > 0);
        let cursor = page.info.next_cursor.unwrap();
        let next = PageRequest::new(&args(Some(1), Some(&cursor)), &())
            .unwrap()
            .page_by_position(rows(), Order::Ascending, "since/until")
            .unwrap();
        assert_eq!(next.items, [format!("{kept:0>90}")]);
    }

    /// A tool that reads only `wanted()` rows, with rows arriving since the last page: the
    /// extra row is gone, and the list must not end for that.
    #[test]
    fn a_short_read_continues_when_upstream_has_more() {
        let (_, cursor) = newest_page(&ledger(), None, 2);
        let r = PageRequest::new(&args(Some(2), cursor.as_deref()), &()).unwrap();
        assert_eq!(r.wanted(), 5);
        // Two new rows above: the five read now end at where the page should start + 1.
        let read = vec![
            ("2026-03-07", "h"),
            ("2026-03-06", "g"),
            ("2026-03-05", "e"),
            ("2026-03-04", "d"),
            ("2026-03-03", "c"),
        ];
        let page = r
            .page_by_position_with_more(keyed(&read), true, Order::Descending, "x")
            .unwrap();
        assert_eq!(page.items, ["2026-03-03/c"]);
        let next = page.info.next_cursor.expect("the list goes on");
        let r = PageRequest::new(&args(Some(2), Some(&next)), &()).unwrap();
        assert_eq!(r.wanted(), 8);
    }

    /// Enough new rows that the read never reached the position: an empty page, and a
    /// cursor that asks for a longer read, so the next call gets further.
    #[test]
    fn a_read_short_of_the_position_asks_for_more_next_time() {
        let (_, cursor) = newest_page(&ledger(), None, 2);
        let r = PageRequest::new(&args(Some(2), cursor.as_deref()), &()).unwrap();
        let read = vec![
            ("2026-03-11", "m"),
            ("2026-03-10", "l"),
            ("2026-03-09", "k"),
            ("2026-03-08", "j"),
            ("2026-03-07", "i"),
        ];
        let page = r
            .page_by_position_with_more(keyed(&read), true, Order::Descending, "x")
            .unwrap();
        assert!(page.items.is_empty());
        let next = page.info.next_cursor.expect("the list goes on");
        let r = PageRequest::new(&args(Some(2), Some(&next)), &()).unwrap();
        assert_eq!(r.wanted(), 8);
        let (items, _) = newest_page(&ledger(), Some(&next), 2);
        assert_eq!(items, ["2026-03-03/c", "2026-03-02/b"], "the position held");
    }

    #[test]
    fn response_format_defaults_to_concise() {
        assert_eq!(ListArgs::default().format(), ResponseFormat::Concise);
        let a: ListArgs = serde_json::from_value(json!({"response_format": "detailed"})).unwrap();
        assert!(a.format().is_detailed());
        assert!(serde_json::from_value::<ListArgs>(json!({"response_format": "full"})).is_err());
    }
}
