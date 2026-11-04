//! Output shaping: how much a list tool returns, and how the model asks for
//! more or narrows the question.
//!
//! - **Limits.** [`DEFAULT_LIMIT`] records unless the call asks for more, at
//!   most [`MAX_LIMIT`].
//! - **Cursors.** Opaque strings. A cursor records where the next page
//!   starts and a fingerprint of the query's filters, so a cursor replayed
//!   with different filters is refused rather than silently paging a
//!   different list. Where the next page starts is a count of rows
//!   ([`PageRequest`]), or, for a list whose rows can arrive or leave between
//!   pages, the last row's own position ([`PositionRequest`]), so a new row
//!   cannot repeat one and a removed row cannot skip one, or, for a list the
//!   provider itself reads on from a position, the tool's key for that position
//!   ([`ResumeRequest`]). A tool picks one when it resolves the arguments.
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
//! [`PageRequest`], [`PositionRequest`] or [`ResumeRequest`] resolves it and cuts the page.

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

/// Which way a list runs, by its rows' position keys ([`PositionRequest::page`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Order {
    /// Largest first: newest first, for an ISO date. A row with no date, sorting by nothing,
    /// comes last.
    Descending,
    /// Smallest key first: by symbol or by name.
    Ascending,
}

impl Order {
    fn cmp(self, a: &PositionKey, b: &PositionKey) -> std::cmp::Ordering {
        match self {
            Self::Ascending => a.cmp(b),
            Self::Descending => b.cmp(a),
        }
    }
}

/// Where a row belongs in a list that pages by position ([`PositionRequest`]): a value to sort
/// by, and what tells the row from others with the same one. Neither may change while the row
/// exists: a row whose key changes moves with it, and can repeat or be missed.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct PositionKey {
    sort: String,
    id: u64,
}

impl PositionKey {
    /// `sort` is what the list runs by, such as an ISO date or a symbol. It travels in the
    /// cursor, encoded and not encrypted, so it is something the concise format shows. Over
    /// [`MAX_SORT_BYTES`] it is cut to that.
    ///
    /// `id` is whatever tells rows with the same `sort` apart, such as a transaction id or a
    /// confirmation number. The cursor holds a fingerprint of it, an unkeyed hash. That keeps
    /// the id from being read off a cursor, and cursors short. It does not hide it from someone
    /// who tries candidates, and it does not need to: a cursor goes to the client that could
    /// ask for `detailed`. Rows with the same `sort` run in the order of those fingerprints, a
    /// fixed order and not a meaningful one. A timestamp in `sort` keeps a day's rows in time
    /// order, and belongs there only when some format of the tool shows it.
    pub fn new(sort: impl Into<String>, id: &str) -> Self {
        let mut sort = sort.into();
        cut_sort(&mut sort);
        Self {
            sort,
            id: fnv1a(id.as_bytes()),
        }
    }
}

/// A row's place in a list: its key, and how many rows before it share that key.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Position {
    key: PositionKey,
    repeat: usize,
}

/// The limit, checked.
fn resolved_limit(args: &ListArgs) -> Result<usize, ToolError> {
    let limit = args.limit.unwrap_or(DEFAULT_LIMIT);
    if !(1..=MAX_LIMIT).contains(&limit) {
        return Err(ToolError::usage(format!(
            "limit must be between 1 and {MAX_LIMIT}; got {limit}"
        )));
    }
    Ok(limit as usize)
}

/// The call's cursor, checked against the query's filters. `None` for the first page.
fn resolved_cursor(args: &ListArgs, fingerprint: u64) -> Result<Option<Cursor>, ToolError> {
    match args.cursor.as_deref().map(str::trim) {
        None | Some("") => Ok(None),
        Some(c) => {
            let (cursor, fp) = decode_cursor(c).ok_or_else(foreign_cursor)?;
            if fp != fingerprint {
                return Err(ToolError::usage(
                    "cursor belongs to a query with different filters; repeat the filters \
                     of the call that returned it, or omit cursor to start over",
                ));
            }
            Ok(Some(cursor))
        }
    }
}

/// A resolved page request, counted in rows: how many, from where, under what budget. For a
/// list that holds still between pages; [`PositionRequest`] is for one that does not.
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
        let limit = resolved_limit(args)?;
        let fingerprint = fingerprint(filters)?;
        let offset = match resolved_cursor(args, fingerprint)? {
            None => 0,
            Some(Cursor::Count(offset)) => offset,
            Some(Cursor::At(_) | Cursor::Resume(_)) => return Err(foreign_cursor()),
        };
        Ok(Self {
            limit,
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
        let rows = matched.into_iter().skip(self.offset);
        cut(
            self.limit,
            self.budget,
            0,
            rows,
            available,
            more_upstream,
            narrow,
            // `offset` is bounded by MAX_OFFSET on decode, so the sum is small.
            |kept| encode_cursor(self.offset + kept, self.fingerprint),
        )
    }
}

/// A resolved page request for a list whose rows can arrive, leave or change between pages: the
/// next cursor holds the last row's position rather than a count of rows.
///
/// A count of rows would repeat one when a new row arrives above the page's end, and skip one when
/// a row above it leaves. A position does neither, and a total added up across pages stays right.
/// Rows that arrive above a position already passed are not seen until the list is read again from
/// the start.
///
/// A tool using this reads every record matching the filters on every page, so it has no
/// `wanted()`. A read of part of the list could not keep the promise above: an unread row could
/// sort above a cursor already past it and never be returned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PositionRequest {
    limit: usize,
    after: After,
    fingerprint: u64,
    budget: usize,
}

/// Where a [`PositionRequest`]'s page starts.
#[derive(Debug, Clone, PartialEq, Eq)]
enum After {
    Start,
    /// A cursor from before this tool moved to positions, held across an upgrade.
    Count(usize),
    Row(Position),
}

impl PositionRequest {
    /// As [`PageRequest::new`]. A cursor from when this tool paged by a count of rows is honoured
    /// as that count.
    pub fn new<F: Serialize + ?Sized>(args: &ListArgs, filters: &F) -> Result<Self, ToolError> {
        let limit = resolved_limit(args)?;
        let fingerprint = fingerprint(filters)?;
        let after = match resolved_cursor(args, fingerprint)? {
            None => After::Start,
            Some(Cursor::Count(offset)) => After::Count(offset),
            Some(Cursor::At(position)) => After::Row(position),
            Some(Cursor::Resume(_)) => return Err(foreign_cursor()),
        };
        Ok(Self {
            limit,
            after,
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

    /// Cut this page out of `keyed`: **every** record matching the filters, paired with its
    /// [`PositionKey`]. `narrow` is as in [`PageRequest::page`].
    ///
    /// The rows are sorted here by key in `order`, stably, so a tool cannot return them in an
    /// order its cursor disagrees with. Rows sharing a key keep the order they came in, and are
    /// told apart by count.
    pub fn page<T: Serialize>(
        &self,
        mut keyed: Vec<(PositionKey, T)>,
        order: Order,
        narrow: &str,
    ) -> Result<Page<T>, ToolError> {
        keyed.sort_by(|a, b| order.cmp(&a.0, &b.0));
        let (keys, rows): (Vec<PositionKey>, Vec<T>) = keyed.into_iter().unzip();
        let start = match &self.after {
            After::Start => 0,
            After::Count(offset) => (*offset).min(keys.len()),
            After::Row(at) => position_after(&keys, at, order),
        };
        let available = keys.len() - start;
        cut(
            self.limit,
            self.budget,
            0,
            rows.into_iter().skip(start),
            available,
            false,
            narrow,
            // Only when rows are left after those kept, and `cut` keeps at least one of them or
            // turns a first row over budget into an error.
            |kept| {
                encode_position_cursor(
                    self.fingerprint,
                    &position_of(&keys, start + kept - 1, order),
                )
            },
        )
    }
}

/// A resolved page request for a list the **provider** reads on from a position, whose rows can
/// arrive or leave between pages. The next cursor holds the tool's own key for where to read on.
///
/// [`PositionRequest`] needs every matching row on every page, which a long list cannot afford,
/// and a count of rows re-reads everything before the page and can skip a row. This does neither:
/// a page costs what the provider charges for one page, however deep it is.
///
/// **A key must resume immediately after its row.** A row's own id does, for a provider that reads
/// on with `before=<id>`. A provider's cursor marking the end of a whole page of its rows does
/// not, since a page cut by its limit or the size budget would skip the rest of that page. So
/// for that provider a key holds the cursor that fetched the row's page and what the row is, and
/// the tool re-reads that page and resumes just after the row. A count into the page would not
/// do, since a row arriving or leaving there moves the rest. If the row itself has gone, the
/// place is lost: the tool says so and the caller starts over, rather than guess.
///
/// A key travels in the cursor encoded and not encrypted, through the client and into the model's
/// transcript, so it is something the client may already see. It also comes back from the client,
/// which can hand-build a cursor, so a tool checks [`Self::resume`]'s value is the shape it issued
/// before sending it anywhere.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResumeRequest {
    limit: usize,
    resume: Option<String>,
    fingerprint: u64,
    budget: usize,
}

impl ResumeRequest {
    /// As [`PageRequest::new`]. A cursor of another kind is refused before anything is fetched.
    pub fn new<F: Serialize + ?Sized>(args: &ListArgs, filters: &F) -> Result<Self, ToolError> {
        let limit = resolved_limit(args)?;
        let fingerprint = fingerprint(filters)?;
        let resume = match resolved_cursor(args, fingerprint)? {
            None => None,
            Some(Cursor::Resume(key)) => Some(key),
            Some(Cursor::Count(_) | Cursor::At(_)) => return Err(foreign_cursor()),
        };
        Ok(Self {
            limit,
            resume,
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

    /// The key to read on from. `None` on the first page.
    pub fn resume(&self) -> Option<&str> {
        self.resume.as_deref()
    }

    /// Cut this page out of `keyed`: the rows read from where this page starts, in the list's
    /// order, each paired with the key that resumes immediately after it. Read one past
    /// [`Self::limit`] where the provider allows, so a page knows whether more follow.
    ///
    /// `rest` is the key that resumes after the last of `keyed`, when the provider has more
    /// beyond them. It is how a page with no rows moves on, and a `rest` equal to where this page
    /// started is an error, not a cursor that would read the same page forever. `narrow` is as in
    /// [`PageRequest::page`].
    ///
    /// An empty key, or one over [`MAX_RESUME_BYTES`], fails rather than making a cursor the next
    /// call would refuse.
    pub fn page<T: Serialize>(
        &self,
        keyed: Vec<(String, T)>,
        rest: Option<String>,
        narrow: &str,
    ) -> Result<Page<T>, ToolError> {
        if rest.is_some() && rest.as_deref() == self.resume() {
            return Err(ToolError::upstream(
                "the provider gave the position it was asked for as the next one, so the list \
                 cannot be read further",
            ));
        }
        let available = keyed.len();
        let (keys, rows): (Vec<String>, Vec<T>) = keyed.into_iter().unzip();
        // A cursor's hex is twice its key, and the reserve only holds a short one.
        let longest = keys.iter().chain(&rest).map(String::len).max().unwrap_or(0);
        let more_upstream = rest.is_some();
        let mut unusable = false;
        let page = cut(
            self.limit,
            self.budget,
            longest.saturating_mul(2),
            rows.into_iter(),
            available,
            more_upstream,
            narrow,
            |kept| {
                let key = match kept.checked_sub(1) {
                    Some(last) if kept < available => keys[last].as_str(),
                    _ => rest.as_deref().unwrap_or_default(),
                };
                unusable = key.is_empty() || key.len() > MAX_RESUME_BYTES;
                encode_resume_cursor(self.fingerprint, key)
            },
        )?;
        if unusable {
            return Err(ToolError::upstream(
                "the position for the next page is empty or too long to hold in a cursor",
            ));
        }
        Ok(page)
    }
}

/// Fill a page from `rows`, the list from where this page starts, of which `available` were read;
/// `next_cursor` makes the cursor once the count kept is known. `cursor_reserve` is what a cursor
/// may take beyond [`ENVELOPE_RESERVE`].
#[allow(clippy::too_many_arguments)]
fn cut<T: Serialize>(
    limit: usize,
    budget: usize,
    cursor_reserve: usize,
    rows: impl Iterator<Item = T>,
    available: usize,
    more_upstream: bool,
    narrow: &str,
    next_cursor: impl FnOnce(usize) -> String,
) -> Result<Page<T>, ToolError> {
    let item_budget = budget
        .saturating_sub(ENVELOPE_RESERVE)
        .saturating_sub(cursor_reserve);
    let mut items = Vec::new();
    let mut used = 2; // the array's brackets
    for item in rows.take(limit) {
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
            "a single record is larger than the {budget}-character result budget; call again \
             with response_format=concise, or narrow with {narrow}"
        )));
    }
    let truncated = kept < limit.min(available);
    let more = kept < available || more_upstream;
    let next_cursor = more.then(|| next_cursor(kept));
    let hint = if truncated {
        Some(format!(
            "Stopped at {kept} records to stay under the {budget}-character result budget. Pass \
             next_cursor as cursor for the rest, or narrow with {narrow} or \
             response_format=concise."
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

/// FNV-1a over the filters' compact JSON. Not a security boundary (a cursor
/// grants nothing a call could not ask for directly); it only catches a
/// cursor replayed against a different query. Filters that cannot be
/// serialized are an error, not a shared constant fingerprint that would let
/// any cursor through.
fn fingerprint<F: Serialize + ?Sized>(filters: &F) -> Result<u64, ToolError> {
    let bytes = serde_json::to_vec(filters)
        .map_err(|e| ToolError::other(format!("serializing the list filters: {e}")))?;
    Ok(fnv1a(&bytes))
}

fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, b| {
        (hash ^ u64::from(*b)).wrapping_mul(0x0100_0000_01b3)
    })
}

fn foreign_cursor() -> ToolError {
    ToolError::usage(
        "cursor is not one this tool returned; pass a result's next_cursor exactly as \
         received, or omit cursor to start from the first page",
    )
}

/// The longest key a [`ResumeRequest`] cursor holds. A page makes room for its own.
pub const MAX_RESUME_BYTES: usize = 1_024;

/// The most of a position's sort value a cursor holds. A sort value is a date or a symbol; this
/// bounds a cursor when a provider's value is not what it looks like.
pub const MAX_SORT_BYTES: usize = 200;

/// Cut at a character boundary, for every row and the cursor alike, so a long value still
/// compares the same on the next page.
fn cut_sort(sort: &mut String) {
    if sort.len() > MAX_SORT_BYTES {
        let end = (0..=MAX_SORT_BYTES)
            .rev()
            .find(|i| sort.is_char_boundary(*i))
            .unwrap_or(0);
        sort.truncate(end);
    }
}

/// How many rows of `keys`, sorted by `order`, come at or before `at`.
fn position_after(keys: &[PositionKey], at: &Position, order: Order) -> usize {
    let first = keys.partition_point(|k| order.cmp(k, &at.key).is_lt());
    let past = keys.partition_point(|k| order.cmp(k, &at.key).is_le());
    first.saturating_add(at.repeat).saturating_add(1).min(past)
}

/// The position of the row at `index` of `keys`, sorted by `order`.
fn position_of(keys: &[PositionKey], index: usize, order: Order) -> Position {
    let key = &keys[index];
    Position {
        key: key.clone(),
        repeat: index - keys.partition_point(|k| order.cmp(k, key).is_lt()),
    }
}

const CURSOR_VERSION: &str = "c1";
const POSITION_CURSOR_VERSION: &str = "c2";
const RESUME_CURSOR_VERSION: &str = "c3";

/// The largest offset a cursor may carry. Cursors come back from the client,
/// which can hand-build one, and no real list pages this far.
const MAX_OFFSET: usize = 10_000_000;

/// The hex of the longest cursor: a key to read on from, and room for the rest.
const MAX_CURSOR_CHARS: usize = (MAX_RESUME_BYTES + 64) * 2;

/// Where a cursor says the next page starts.
enum Cursor {
    Count(usize),
    At(Position),
    Resume(String),
}

fn to_hex(plain: &str) -> String {
    plain.bytes().map(|b| format!("{b:02x}")).collect()
}

/// Hex of `c1:<offset>:<fingerprint>`: opaque to a reader, cheap to check.
fn encode_cursor(offset: usize, fingerprint: u64) -> String {
    to_hex(&format!("{CURSOR_VERSION}:{offset}:{fingerprint:016x}"))
}

/// Hex of `c2:<fingerprint>:<repeat>:<id hash>:<sort>`, the sort value last because it may hold
/// a colon.
fn encode_position_cursor(fingerprint: u64, position: &Position) -> String {
    to_hex(&format!(
        "{POSITION_CURSOR_VERSION}:{fingerprint:016x}:{}:{:016x}:{}",
        position.repeat, position.key.id, position.key.sort
    ))
}

/// Hex of `c3:<fingerprint>:<key>`, the key last because it may hold a colon.
fn encode_resume_cursor(fingerprint: u64, key: &str) -> String {
    to_hex(&format!("{RESUME_CURSOR_VERSION}:{fingerprint:016x}:{key}"))
}

/// The cursor and the fingerprint of the filters it was issued for.
fn decode_cursor(cursor: &str) -> Option<(Cursor, u64)> {
    if !cursor.len().is_multiple_of(2) || cursor.len() > MAX_CURSOR_CHARS {
        return None;
    }
    let bytes: Vec<u8> = (0..cursor.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(cursor.get(i..i + 2)?, 16).ok())
        .collect::<Option<_>>()?;
    let plain = String::from_utf8(bytes).ok()?;
    let (version, rest) = plain.split_once(':')?;
    match version {
        CURSOR_VERSION => {
            let (offset, fingerprint) = rest.split_once(':')?;
            let offset: usize = offset.parse().ok()?;
            (offset <= MAX_OFFSET).then_some(())?;
            let fingerprint = u64::from_str_radix(fingerprint, 16).ok()?;
            Some((Cursor::Count(offset), fingerprint))
        }
        POSITION_CURSOR_VERSION => {
            let mut parts = rest.splitn(4, ':');
            let fingerprint = u64::from_str_radix(parts.next()?, 16).ok()?;
            let repeat = parts.next()?.parse().ok()?;
            let id = u64::from_str_radix(parts.next()?, 16).ok()?;
            let sort = parts.next()?;
            (sort.len() <= MAX_SORT_BYTES).then_some(())?;
            let key = PositionKey {
                sort: sort.to_string(),
                id,
            };
            Some((Cursor::At(Position { key, repeat }), fingerprint))
        }
        RESUME_CURSOR_VERSION => {
            let (fingerprint, key) = rest.split_once(':')?;
            let fingerprint = u64::from_str_radix(fingerprint, 16).ok()?;
            (!key.is_empty() && key.len() <= MAX_RESUME_BYTES).then_some(())?;
            Some((Cursor::Resume(key.to_string()), fingerprint))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Up to `read` of `rows` from after `key`, each paired with its own value as its key, as a
    /// provider's `before=<id>` reads them.
    fn from_after(rows: &[u32], key: Option<&str>, read: usize) -> Vec<(String, u32)> {
        let bound = key.map(|k| k.parse::<u32>().unwrap());
        rows.iter()
            .copied()
            .filter(|r| bound.is_none_or(|b| *r < b))
            .take(read)
            .map(|r| (r.to_string(), r))
            .collect()
    }

    #[test]
    fn a_resume_cursor_walks_a_list_without_gaps_or_repeats() {
        let all: Vec<u32> = (0..60).rev().collect();
        let filters = json!({"channel": "example"});
        let mut seen = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let r = ResumeRequest::new(&args(Some(25), cursor.as_deref()), &filters).unwrap();
            let keyed = from_after(&all, r.resume(), r.limit() + 1);
            let page = r.page(keyed, None, "since").unwrap();
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
    fn a_row_that_leaves_or_arrives_between_pages_loses_nothing() {
        let all: Vec<u32> = (10..20).rev().collect();
        let first = ResumeRequest::new(&args(Some(3), None), &()).unwrap();
        let page = first
            .page(from_after(&all, first.resume(), 4), None, "since")
            .unwrap();
        assert_eq!(page.items, [19, 18, 17]);

        // One read is deleted, and one arrives above the page.
        let moved: Vec<u32> = [99]
            .into_iter()
            .chain(all.iter().copied().filter(|r| *r != 18))
            .collect();
        let next =
            ResumeRequest::new(&args(Some(3), page.info.next_cursor.as_deref()), &()).unwrap();
        assert_eq!(next.resume(), Some("17"));
        let keyed = from_after(&moved, next.resume(), 4);
        assert_eq!(next.page(keyed, None, "since").unwrap().items, [16, 15, 14]);
    }

    #[test]
    fn a_resume_page_cut_by_the_budget_reads_on_from_the_last_row_kept() {
        let keyed: Vec<(String, String)> =
            (0..4).map(|i| (format!("k{i}"), "x".repeat(40))).collect();
        let r = ResumeRequest::new(&args(Some(4), None), &())
            .unwrap()
            .with_budget(ENVELOPE_RESERVE + 100);
        let page = r.page(keyed, None, "since").unwrap();
        assert_eq!(page.items.len(), 2);
        assert!(page.info.truncated);
        let next = ResumeRequest::new(&args(Some(4), page.info.next_cursor.as_deref()), &());
        assert_eq!(next.unwrap().resume(), Some("k1"));
    }

    /// Two provider pages reached by the cursor before each. A row's key is the cursor that
    /// fetched its page and the row itself, so the next call resumes just after that row.
    fn read_by_page_cursor(
        pages: &[Vec<u32>],
        cursor: Option<&str>,
    ) -> Result<Page<u32>, ToolError> {
        let r = ResumeRequest::new(&args(Some(3), cursor), &())?;
        let (mut at, after) = match r.resume().map(|k| k.split_once('|').unwrap()) {
            Some((page, row)) => (page.parse::<usize>().unwrap(), row.parse::<u32>().ok()),
            None => (0, None),
        };
        let mut keyed = Vec::new();
        let mut after = after;
        while keyed.len() <= r.limit() && at < pages.len() {
            let rows = &pages[at];
            let start = match after.take() {
                None => 0,
                Some(row) => {
                    rows.iter().position(|r| *r == row).ok_or_else(|| {
                        ToolError::usage("the list changed where this read had reached")
                    })? + 1
                }
            };
            for row in &rows[start..] {
                keyed.push((format!("{at}|{row}"), *row));
            }
            at += 1;
        }
        let rest = (at < pages.len()).then(|| format!("{at}|"));
        r.page(keyed, rest, "since")
    }

    #[test]
    fn a_page_level_cursor_loses_no_rows_to_a_cut_page() {
        let pages = [(0..4).collect::<Vec<u32>>(), (4..8).collect()];
        let mut seen = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let page = read_by_page_cursor(&pages, cursor.as_deref()).unwrap();
            seen.extend(page.items);
            match page.info.next_cursor {
                Some(c) => cursor = Some(c),
                None => break,
            }
        }
        assert_eq!(seen, (0..8).collect::<Vec<u32>>());
    }

    #[test]
    fn a_page_level_cursor_survives_a_row_moving_in_the_page_it_re_reads() {
        let pages = [vec![0, 1, 2, 3, 4, 5], vec![6, 7]];
        let first = read_by_page_cursor(&pages, None).unwrap();
        assert_eq!(first.items, [0, 1, 2]);
        let cursor = first.info.next_cursor;

        // A row arrives at the top, and one already returned leaves.
        let moved = [vec![99, 0, 2, 3, 4, 5], vec![6, 7]];
        let next = read_by_page_cursor(&moved, cursor.as_deref()).unwrap();
        assert_eq!(next.items, [3, 4, 5]);
    }

    #[test]
    fn a_page_level_cursor_says_when_its_own_row_has_gone() {
        let pages = [vec![0, 1, 2, 3, 4, 5], vec![6, 7]];
        let first = read_by_page_cursor(&pages, None).unwrap();
        let moved = [vec![0, 1, 3, 4, 5], vec![6, 7]];
        let e = read_by_page_cursor(&moved, first.info.next_cursor.as_deref()).unwrap_err();
        assert!(e.message().contains("changed"), "{e}");
    }

    #[test]
    fn a_resume_cursor_keeps_a_key_with_colons_and_is_bound_to_its_filters() {
        let keyed = vec![("a:b:c".to_string(), 1), ("d".to_string(), 2)];
        let r = ResumeRequest::new(&args(Some(1), None), &json!({"state": "open"})).unwrap();
        let cursor = r.page(keyed, None, "state").unwrap().info.next_cursor;
        let again =
            ResumeRequest::new(&args(Some(1), cursor.as_deref()), &json!({"state": "open"}));
        assert_eq!(again.unwrap().resume(), Some("a:b:c"));
        let other = ResumeRequest::new(
            &args(Some(1), cursor.as_deref()),
            &json!({"state": "closed"}),
        );
        assert!(other.unwrap_err().message().contains("different filters"));
    }

    #[test]
    fn each_request_refuses_a_cursor_of_another_kind() {
        let resume = encode_resume_cursor(fingerprint(&()).unwrap(), "key");
        let e = PageRequest::new(&args(None, Some(&resume)), &()).unwrap_err();
        assert!(e.message().contains("not one this tool returned"), "{e}");
        let e = PositionRequest::new(&args(None, Some(&resume)), &()).unwrap_err();
        assert!(e.message().contains("not one this tool returned"), "{e}");

        let count = encode_cursor(5, fingerprint(&()).unwrap());
        let e = ResumeRequest::new(&args(None, Some(&count)), &()).unwrap_err();
        assert!(e.message().contains("not one this tool returned"), "{e}");
    }

    #[test]
    fn a_key_a_cursor_cannot_hold_fails_only_when_it_would_be_used() {
        let long = "k".repeat(MAX_RESUME_BYTES + 1);
        let r = ResumeRequest::new(&args(Some(1), None), &()).unwrap();
        // The last page makes no cursor.
        r.page(vec![(long.clone(), 1)], None, "since").unwrap();
        let two = |key: &str| vec![(key.to_string(), 1), ("next".to_string(), 2)];
        for bad in [long.as_str(), ""] {
            let e = r.page(two(bad), None, "since").unwrap_err();
            assert!(e.message().contains("too long"), "{e}");
        }

        // The longest key a page can make, the next call can read.
        let longest = "k".repeat(MAX_RESUME_BYTES);
        let cursor = r
            .page(two(&longest), None, "since")
            .unwrap()
            .info
            .next_cursor;
        let next = ResumeRequest::new(&args(Some(1), cursor.as_deref()), &()).unwrap();
        assert_eq!(next.resume(), Some(longest.as_str()));
    }

    /// The longest key on a page filled to the budget still fits the size the spec promises.
    #[test]
    fn a_full_page_with_the_longest_key_stays_under_the_budget() {
        let key = "k".repeat(MAX_RESUME_BYTES);
        let row = "x".repeat(1_000);
        let keyed: Vec<(String, String)> = (0..200).map(|_| (key.clone(), row.clone())).collect();
        let r = ResumeRequest::new(&args(Some(200), None), &json!({"a": 1})).unwrap();
        let page = r.page(keyed, None, "a narrowing argument").unwrap();
        assert!(page.info.truncated);
        assert!(page.info.next_cursor.is_some());
        let size = serde_json::to_string(&page).unwrap().chars().count();
        assert!(size < SIZE_BUDGET, "{size}");
    }

    /// A provider can answer an empty page and still have more.
    #[test]
    fn a_page_with_no_rows_moves_on_by_the_position_the_provider_gave() {
        let r = ResumeRequest::new(&args(Some(5), None), &()).unwrap();
        let page = r
            .page(Vec::<(String, u32)>::new(), Some("p2".into()), "since")
            .unwrap();
        assert!(page.items.is_empty());
        let next = ResumeRequest::new(&args(Some(5), page.info.next_cursor.as_deref()), &());
        let next = next.unwrap();
        assert_eq!(next.resume(), Some("p2"));

        // The same position again would read the same page forever.
        let e = next
            .page(Vec::<(String, u32)>::new(), Some("p2".into()), "since")
            .unwrap_err();
        assert!(e.message().contains("cannot be read further"), "{e}");
    }

    #[test]
    fn a_page_of_every_row_read_with_more_upstream_reads_on_by_the_rest() {
        let r = ResumeRequest::new(&args(Some(5), None), &()).unwrap();
        let keyed = vec![("a".to_string(), 1), ("b".to_string(), 2)];
        let page = r.page(keyed, Some("c".into()), "since").unwrap();
        assert_eq!(page.items, [1, 2]);
        let next = ResumeRequest::new(&args(Some(5), page.info.next_cursor.as_deref()), &());
        assert_eq!(next.unwrap().resume(), Some("c"));
    }

    #[test]
    fn a_hand_built_cursor_with_an_empty_key_is_refused() {
        let empty = encode_resume_cursor(fingerprint(&()).unwrap(), "");
        let e = ResumeRequest::new(&args(None, Some(&empty)), &()).unwrap_err();
        assert!(e.message().contains("not one this tool returned"), "{e}");
    }

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
    fn keyed(rows: &[(&str, &str)]) -> Vec<(PositionKey, String)> {
        rows.iter()
            .map(|(date, id)| (PositionKey::new(*date, id), format!("{date}/{id}")))
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
        let page = PositionRequest::new(&args(Some(limit), cursor), &())
            .unwrap()
            .page(keyed(rows), Order::Descending, "since/until")
            .unwrap();
        (page.items, page.info.next_cursor)
    }

    /// The plain text of a cursor.
    fn decoded(cursor: &str) -> String {
        let bytes: Vec<u8> = (0..cursor.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&cursor[i..i + 2], 16).unwrap())
            .collect();
        String::from_utf8(bytes).unwrap()
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

    /// A busy day, split by a page boundary: a row of that day arriving between pages, on
    /// either side of where the first page stopped, repeats nothing and loses nothing that
    /// was there.
    #[test]
    fn a_day_split_by_a_page_boundary_holds_when_it_gains_a_row() {
        let day = [
            ("2026-03-02", "p"),
            ("2026-03-02", "q"),
            ("2026-03-02", "r"),
            ("2026-03-02", "s"),
            ("2026-03-01", "older"),
        ];
        let (first, cursor) = newest_page(&day, None, 2);
        let mut grown = day.to_vec();
        grown.insert(0, ("2026-03-02", "new"));
        let mut seen = first.clone();
        let mut cursor = cursor;
        while let Some(c) = cursor {
            let (items, next) = newest_page(&grown, Some(&c), 2);
            seen.extend(items);
            cursor = next;
        }
        let mut unique = seen.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), seen.len(), "a row repeated: {seen:?}");
        for (date, id) in day {
            let row = format!("{date}/{id}");
            assert!(seen.contains(&row), "{row} lost: {seen:?}");
        }
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

        let page = PositionRequest::new(&args(Some(1), None), &())
            .unwrap()
            .page(keyed(&shuffled), Order::Ascending, "symbol")
            .unwrap();
        assert_eq!(page.items, ["2026-03-01/a"]);
        let next = PositionRequest::new(&args(Some(1), page.info.next_cursor.as_deref()), &())
            .unwrap()
            .page(keyed(&shuffled), Order::Ascending, "symbol")
            .unwrap();
        assert_eq!(next.items, ["2026-03-02/b"]);
    }

    #[test]
    fn an_undated_row_comes_last_when_newest_is_first() {
        let rows = vec![
            (PositionKey::new("", "z"), "undated"),
            (PositionKey::new("2026-03-01", "a"), "dated"),
        ];
        let page = PositionRequest::new(&args(None, None), &())
            .unwrap()
            .page(rows, Order::Descending, "x")
            .unwrap();
        assert_eq!(page.items, ["dated", "undated"]);
    }

    /// Two rows with nothing to tell them apart, split by a page boundary: neither is
    /// lost, and they keep the order they came in.
    #[test]
    fn rows_sharing_a_key_are_told_apart_by_count() {
        let same = || PositionKey::new("2026-03-02", "same");
        let rows = || {
            vec![
                (same(), "first"),
                (same(), "second"),
                (same(), "third"),
                (PositionKey::new("2026-03-01", "a"), "older"),
            ]
        };
        let mut seen = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let page = PositionRequest::new(&args(Some(1), cursor.as_deref()), &())
                .unwrap()
                .page(rows(), Order::Descending, "x")
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
            fp,
            &Position {
                key: PositionKey::new("2026-03-03", "c"),
                repeat: usize::MAX,
            },
        );
        let (items, _) = newest_page(&ledger(), Some(&forged), 1);
        assert_eq!(items, ["2026-03-02/b"]);
    }

    /// A concise result holds an id back, and a cursor is plain hex a client can read: it
    /// should not be readable off it.
    #[test]
    fn a_cursor_holds_the_sort_value_and_a_fingerprint_of_the_id() {
        let (_, cursor) = newest_page(
            &[("2026-03-02", "CONF-DUMMY"), ("2026-03-01", "a")],
            None,
            1,
        );
        let plain = decoded(&cursor.unwrap());
        assert!(plain.ends_with(":2026-03-02"), "{plain}");
        assert!(!plain.contains("CONF-DUMMY"), "{plain}");
    }

    #[test]
    fn a_sort_value_with_a_colon_or_past_the_limit_still_finds_its_place() {
        let long = "x".repeat(MAX_SORT_BYTES + 50);
        let rows = || {
            vec![
                (
                    PositionKey::new(format!("2026-03-03T10:00:00 {long}"), "c"),
                    "c",
                ),
                (
                    PositionKey::new(format!("2026-03-02T10:00:00 {long}"), "b"),
                    "b",
                ),
                (PositionKey::new("2026-03-01T10:00:00", "a"), "a"),
            ]
        };
        let page = PositionRequest::new(&args(Some(1), None), &())
            .unwrap()
            .page(rows(), Order::Descending, "x")
            .unwrap();
        assert_eq!(page.items, ["c"]);
        let cursor = page.info.next_cursor.unwrap();
        assert!(cursor.len() <= MAX_CURSOR_CHARS, "{}", cursor.len());
        let page = PositionRequest::new(&args(Some(1), Some(&cursor)), &())
            .unwrap()
            .page(rows(), Order::Descending, "x")
            .unwrap();
        assert_eq!(page.items, ["b"]);
    }

    #[test]
    fn a_long_sort_value_is_cut_on_a_character_boundary() {
        let key = PositionKey::new("é".repeat(MAX_SORT_BYTES), "id");
        assert!(key.sort.len() <= MAX_SORT_BYTES);
        assert!(key.sort.chars().all(|c| c == 'é'));
    }

    #[test]
    fn a_position_cursor_stays_bound_to_its_filters() {
        let page = PositionRequest::new(&args(Some(1), None), &json!({"a": 1}))
            .unwrap()
            .page(keyed(&ledger()), Order::Descending, "a")
            .unwrap();
        let cursor = page.info.next_cursor.unwrap();
        assert!(!cursor.contains(':'), "opaque: {cursor}");
        let e = PositionRequest::new(&args(None, Some(&cursor)), &json!({"a": 2})).unwrap_err();
        assert!(e.message().contains("different filters"), "{e}");
    }

    /// A tool picks its kind of cursor when it resolves the arguments. A position cursor
    /// reaching a count-of-rows tool was not this tool's, and is refused before anything is
    /// fetched, so a client cannot change how much a tool reads.
    #[test]
    fn a_count_of_rows_tool_refuses_a_position_cursor_before_any_read() {
        let (_, cursor) = newest_page(&ledger(), None, 1);
        let e = PageRequest::new(&args(None, cursor.as_deref()), &()).unwrap_err();
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
        let id = "0000000000000001";
        for forged in [
            to_hex(&format!("c2:{fp:016x}")),
            to_hex(&format!("c2:{fp:016x}:x:{id}:sort")),
            to_hex(&format!("c2:{fp:016x}:0:nothex:sort")),
            to_hex(&format!("c2:{fp:016x}:0:{id}")),
            to_hex(&format!("c2:nothex:0:{id}:sort")),
            to_hex(&format!("c3:{fp:016x}:0:{id}:sort")),
            to_hex(&format!("c1:0:{fp:016x}:extra")),
            to_hex(&format!(
                "c2:{fp:016x}:0:{id}:{}",
                "k".repeat(MAX_SORT_BYTES + 1)
            )),
            "ab".repeat(MAX_CURSOR_CHARS),
        ] {
            let e = PositionRequest::new(&args(None, Some(&forged)), &()).unwrap_err();
            assert!(e.message().contains("next_cursor exactly"), "{forged}: {e}");
        }
        let past_the_end = encode_position_cursor(
            fp,
            &Position {
                key: PositionKey::new("0000", "a"),
                repeat: 0,
            },
        );
        let (items, next) = newest_page(&ledger(), Some(&past_the_end), 2);
        assert!(items.is_empty());
        assert!(next.is_none());
    }

    #[test]
    fn a_position_page_over_budget_stops_early_and_continues_where_it_stopped() {
        let rows = || -> Vec<(PositionKey, String)> {
            (0..50)
                .map(|i| (PositionKey::new(format!("{i:04}"), ""), format!("{i:0>90}")))
                .collect()
        };
        let r = PositionRequest::new(&args(Some(50), None), &())
            .unwrap()
            .with_budget(ENVELOPE_RESERVE + 1_000);
        let page = r.page(rows(), Order::Ascending, "since/until").unwrap();
        assert!(page.info.truncated);
        let kept = page.items.len();
        assert!(kept < 50 && kept > 0);
        let cursor = page.info.next_cursor.unwrap();
        let next = PositionRequest::new(&args(Some(1), Some(&cursor)), &())
            .unwrap()
            .page(rows(), Order::Ascending, "since/until")
            .unwrap();
        assert_eq!(next.items, [format!("{kept:0>90}")]);
    }

    #[test]
    fn one_row_over_budget_on_a_position_page_is_a_usage_error() {
        let r = PositionRequest::new(&args(None, None), &())
            .unwrap()
            .with_budget(ENVELOPE_RESERVE + 10);
        let rows = vec![(PositionKey::new("2026-03-01", "a"), "x".repeat(100))];
        let e = r.page(rows, Order::Descending, "type").unwrap_err();
        assert!(e.message().contains("response_format=concise"), "{e}");
    }

    #[test]
    fn response_format_defaults_to_concise() {
        assert_eq!(ListArgs::default().format(), ResponseFormat::Concise);
        let a: ListArgs = serde_json::from_value(json!({"response_format": "detailed"})).unwrap();
        assert!(a.format().is_detailed());
        assert!(serde_json::from_value::<ListArgs>(json!({"response_format": "full"})).is_err());
    }
}
