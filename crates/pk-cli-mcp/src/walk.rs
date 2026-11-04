//! Reading a provider's list a page at a time: DESIGN.md §1.9, "A long read stops in time".
//!
//! `<bin> mcp --http` answers a call at [`CALL_LIMIT`](crate::CALL_LIMIT) and does not cancel it,
//! so a walk past that keeps spending the provider's patience with nobody listening. A tool that
//! walks starts a [`ReadBudget`] when the call begins and checks it before each page. A walk that
//! pages by offset or page number keeps a [`RowsSeen`] too, since a row arriving during the walk
//! moves the last row of one page onto the next.
//!
//! ```
//! use pk_cli_mcp::{Clock, ReadBudget, RowsSeen, READ_BUDGET};
//! use serde_json::Value;
//!
//! fn walk(
//!     clock: &dyn Clock,
//!     page: impl Fn(usize) -> Vec<Value>,
//! ) -> Result<Vec<Value>, pk_cli_core::CliError> {
//!     let budget = ReadBudget::start(clock, READ_BUDGET);
//!     let mut seen = RowsSeen::new();
//!     let mut rows = Vec::new();
//!     for number in 0.. {
//!         budget.check(Some("since/until"))?;
//!         let found = page(number);
//!         if found.is_empty() {
//!             break;
//!         }
//!         rows.extend(found.into_iter().filter(|row| seen.first_time(row)));
//!     }
//!     Ok(rows)
//! }
//! ```

use std::collections::HashSet;
use std::fmt;
use std::time::Duration;

use pk_cli_core::CliError;
use serde_json::Value;

use crate::deadline::{Clock, SendDeadline};

/// How long a call may keep asking a provider for more of a list. It leaves room inside
/// [`CALL_LIMIT`](crate::CALL_LIMIT) for a page already sent to finish within a client's own
/// timeout, and for the answer.
pub const READ_BUDGET: Duration = Duration::from_secs(90);

/// One call's time to read a list, on both clocks, since a machine that slept through a walk has
/// nobody waiting on it either.
#[derive(Clone, Copy)]
pub struct ReadBudget<'c> {
    started: SendDeadline<'c>,
    limit: Duration,
}

impl<'c> ReadBudget<'c> {
    /// Start the time now. Call it first thing in the call, so every list the call reads shares it.
    pub fn start(clock: &'c dyn Clock, limit: Duration) -> Self {
        Self {
            started: SendDeadline::start(clock, limit),
            limit,
        }
    }

    pub fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }

    /// Whether the time is up, for a tool that reports a partial list as incomplete rather than
    /// failing.
    pub fn is_spent(&self) -> bool {
        self.elapsed() >= self.limit
    }

    /// Call before each request of a walk. Past the limit, an error that says the list is not
    /// reported here.
    ///
    /// `narrow` names the arguments that shorten the read, and makes it a [`CliError::Usage`], so
    /// the model calls again with a smaller question. `None` is for a read the arguments cannot
    /// shorten, such as a whole list the provider has no filter for: that is
    /// [`CliError::Upstream`], whose remedy is to try again later.
    pub fn check(&self, narrow: Option<&str>) -> Result<(), CliError> {
        if !self.is_spent() {
            return Ok(());
        }
        let took = format!(
            "this read took more than {} seconds, so its list is not reported",
            self.limit.as_secs()
        );
        Err(match narrow {
            Some(narrow) => CliError::Usage(format!("{took}; narrow {narrow}")),
            None => CliError::Upstream(format!("{took}; the provider is answering slowly")),
        })
    }
}

impl fmt::Debug for ReadBudget<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ReadBudget")
            .field("elapsed", &self.elapsed())
            .field("limit", &self.limit)
            .finish()
    }
}

/// The rows an offset walk has already returned.
///
/// A row is the same row when it comes back the same in every field, so two rows that only share
/// an id stay two rows. [`Self::ignoring`] leaves out a field that differs on every read, such as
/// a link signed for the moment it was served.
#[derive(Debug, Clone, Default)]
pub struct RowsSeen {
    seen: HashSet<String>,
    ignoring: &'static [&'static str],
}

impl RowsSeen {
    pub fn new() -> Self {
        Self::default()
    }

    /// As [`Self::new`], leaving these top-level fields out of the comparison.
    pub fn ignoring(fields: &'static [&'static str]) -> Self {
        Self {
            seen: HashSet::new(),
            ignoring: fields,
        }
    }

    /// Whether this is the first time the walk has met this row.
    pub fn first_time(&mut self, row: &Value) -> bool {
        let text = match row {
            Value::Object(fields) if !self.ignoring.is_empty() => {
                let mut kept = fields.clone();
                for field in self.ignoring {
                    kept.remove(*field);
                }
                Value::Object(kept).to_string()
            }
            other => other.to_string(),
        };
        self.seen.insert(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conformance::ManualClock;
    use serde_json::json;

    const LIMIT: Duration = Duration::from_secs(90);

    #[test]
    fn a_read_inside_its_budget_goes_on() {
        let clock = ManualClock::new();
        let budget = ReadBudget::start(&clock, LIMIT);
        budget.check(Some("since/until")).unwrap();
        clock.advance(Duration::from_millis(89_999));
        budget.check(None).unwrap();
        assert!(!budget.is_spent());
    }

    #[test]
    fn a_read_the_arguments_can_shorten_is_asked_to() {
        let clock = ManualClock::new();
        let budget = ReadBudget::start(&clock, LIMIT);
        clock.advance(LIMIT);
        assert!(budget.is_spent());
        let err = budget.check(Some("since/until")).unwrap_err();
        assert!(matches!(err, CliError::Usage(_)), "{err:?}");
        let text = err.to_string();
        assert!(text.contains("more than 90 seconds"), "{text}");
        assert!(text.contains("narrow since/until"), "{text}");
    }

    #[test]
    fn a_read_the_arguments_cannot_shorten_blames_the_provider() {
        let clock = ManualClock::new();
        let budget = ReadBudget::start(&clock, LIMIT);
        clock.advance(LIMIT);
        let err = budget.check(None).unwrap_err();
        assert!(matches!(err, CliError::Upstream(_)), "{err:?}");
        assert!(!err.to_string().contains("narrow"), "{err}");
    }

    /// A Mac that slept through the walk stops the monotonic clock and not the wall clock.
    #[test]
    fn a_machine_asleep_spends_the_budget() {
        let clock = ManualClock::new();
        let budget = ReadBudget::start(&clock, LIMIT);
        clock.sleep(LIMIT);
        assert!(budget.is_spent());
    }

    #[test]
    fn a_row_a_later_page_repeats_is_met_once() {
        let mut seen = RowsSeen::new();
        let row = json!({"id": 1, "amount": "2.00"});
        assert!(seen.first_time(&row));
        assert!(!seen.first_time(&row));
    }

    #[test]
    fn two_rows_that_only_share_an_id_stay_two_rows() {
        let mut seen = RowsSeen::new();
        assert!(seen.first_time(&json!({"id": 1, "amount": "2.00"})));
        assert!(seen.first_time(&json!({"id": 1, "amount": "3.00"})));
    }

    #[test]
    fn a_field_signed_per_read_does_not_make_a_new_row() {
        let mut seen = RowsSeen::ignoring(&["attachments"]);
        assert!(seen.first_time(&json!({"id": 1, "attachments": ["a?sig=1"]})));
        assert!(!seen.first_time(&json!({"id": 1, "attachments": ["a?sig=2"]})));
        assert!(seen.first_time(&json!({"id": 2, "attachments": ["a?sig=2"]})));
    }
}
