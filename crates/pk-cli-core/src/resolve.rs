//! Resolving a user-supplied reference — `<bin> devices get <REF>` — to one
//! record, when the user may have typed a display name, an id, a sloppily
//! cased version of either, or just enough of a name to be unambiguous.
//!
//! Every family CLI with a `get <REF>` or `move <REF>` grew its own copy of
//! this ladder, and the copies disagreed in exactly the places that matter
//! for a mutation target: one preferred ids over names, one silently took the
//! first of several substring matches. [`pick`] is the one ladder, with the
//! two rails that make it safe to use for the argument of a write:
//!
//! - **Tiers are tried in a fixed order** — exact name, exact id
//!   (case-insensitive), case-insensitive name, unique partial name — and the
//!   first tier with any hit decides. Exact name goes first because a user
//!   who typed a display name means that record, even if another record's id
//!   happens to equal the same string. Ids are compared case-insensitively
//!   because vendors render the same MAC as `AA:BB` in one place and `aabb`
//!   in another.
//! - **More than one hit at any tier is an error naming the candidates**, ids
//!   included (two records sharing an id is a provider bug, not a reason to
//!   guess). A silent first pick never happens; the user narrows the reference
//!   or uses the id.
//!
//! Both failure shapes are [`CliError::NotFound`] (exit 4): nothing *uniquely*
//! matched, and the message carries the candidates so the retry is obvious.

use crate::CliError;

/// Resolve `query` against `items` by the ladder in the module docs.
///
/// `ids_of` returns every id a record answers to (a vendor id and its bare
/// MAC, say); `name_of` its display name; `what` names the record kind for
/// messages (`"device"`, `"room"`). Surrounding whitespace on the query is
/// ignored. An empty query is a usage error rather than a substring that
/// matches everything.
pub fn pick<'a, T>(
    items: &'a [T],
    query: &str,
    ids_of: impl Fn(&T) -> Vec<String>,
    name_of: impl Fn(&T) -> &str,
    what: &str,
) -> Result<&'a T, CliError> {
    let q = query.trim();
    if q.is_empty() {
        return Err(CliError::Usage(format!("{what} reference is empty")));
    }
    let ambiguous = |hits: &[&T]| {
        CliError::NotFound(format!(
            "`{q}` matches more than one {what}: {}",
            hits.iter()
                .map(|x| format!("{} ({})", name_of(x), ids_of(x).join("/")))
                .collect::<Vec<_>>()
                .join("; ")
        ))
    };
    // A tier decides when it has any hit: one is the answer, several is the
    // ambiguity error. None falls through to the next tier.
    let decide = |hits: Vec<&'a T>| -> Option<Result<&'a T, CliError>> {
        match hits.len() {
            0 => None,
            1 => Some(Ok(hits[0])),
            _ => Some(Err(ambiguous(&hits))),
        }
    };
    if let Some(r) = decide(items.iter().filter(|x| name_of(x) == q).collect()) {
        return r;
    }
    let ql = q.to_lowercase();
    if let Some(r) = decide(
        items
            .iter()
            .filter(|x| ids_of(x).iter().any(|i| i.to_lowercase() == ql))
            .collect(),
    ) {
        return r;
    }
    if let Some(r) = decide(
        items
            .iter()
            .filter(|x| name_of(x).to_lowercase() == ql)
            .collect(),
    ) {
        return r;
    }
    match decide(
        items
            .iter()
            .filter(|x| name_of(x).to_lowercase().contains(&ql))
            .collect(),
    ) {
        Some(r) => r,
        None => Err(CliError::NotFound(format!("no {what} matching `{q}`"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct Dev {
        ids: Vec<&'static str>,
        name: &'static str,
    }

    fn dev(ids: &[&'static str], name: &'static str) -> Dev {
        Dev {
            ids: ids.to_vec(),
            name,
        }
    }

    fn find<'a>(items: &'a [Dev], q: &str) -> Result<&'a Dev, CliError> {
        pick(
            items,
            q,
            |d| d.ids.iter().map(|s| s.to_string()).collect(),
            |d| d.name,
            "device",
        )
    }

    fn fixture() -> Vec<Dev> {
        vec![
            dev(&["H6076_AA:BB:CC", "AA:BB:CC"], "Office Lamp"),
            dev(&["KP115_11:22:33", "11:22:33"], "Desk Plug"),
            dev(&["H6159_44:55:66", "44:55:66"], "Kitchen Strip"),
        ]
    }

    #[test]
    fn tier_1_exact_name() {
        let items = fixture();
        assert_eq!(find(&items, "Desk Plug").unwrap().name, "Desk Plug");
    }

    #[test]
    fn tier_2_exact_id_is_case_insensitive() {
        let items = fixture();
        assert_eq!(find(&items, "KP115_11:22:33").unwrap().name, "Desk Plug");
        assert_eq!(find(&items, "kp115_11:22:33").unwrap().name, "Desk Plug");
        // Any of a record's ids resolves it, not just the first.
        assert_eq!(find(&items, "aa:bb:cc").unwrap().name, "Office Lamp");
    }

    #[test]
    fn tier_3_case_insensitive_name() {
        let items = fixture();
        assert_eq!(find(&items, "office lamp").unwrap().name, "Office Lamp");
        assert_eq!(find(&items, "KITCHEN STRIP").unwrap().name, "Kitchen Strip");
    }

    #[test]
    fn tier_4_unique_partial_name() {
        let items = fixture();
        assert_eq!(find(&items, "desk").unwrap().name, "Desk Plug");
        assert_eq!(find(&items, "STRIP").unwrap().name, "Kitchen Strip");
    }

    #[test]
    fn query_is_trimmed() {
        let items = fixture();
        assert_eq!(find(&items, "  Desk Plug\n").unwrap().name, "Desk Plug");
    }

    #[test]
    fn exact_name_beats_an_id_of_another_record() {
        // A record literally named after another record's id: the name tier
        // runs first, so the user gets what they typed.
        let items = vec![dev(&["A1"], "B2"), dev(&["B2"], "Other")];
        assert_eq!(find(&items, "B2").unwrap().ids, vec!["A1"]);
    }

    #[test]
    fn an_exact_case_insensitive_name_beats_a_partial_elsewhere() {
        // "lamp" is a whole name here and a substring of "Floor Lamp"; the
        // exact tier decides before the partial tier is consulted.
        let items = vec![dev(&["1"], "Floor Lamp"), dev(&["2"], "Lamp")];
        assert_eq!(find(&items, "LAMP").unwrap().ids, vec!["2"]);
    }

    #[test]
    fn zero_hits_is_not_found_naming_the_query() {
        let items = fixture();
        let err = find(&items, "toaster").unwrap_err();
        assert!(matches!(err, CliError::NotFound(_)));
        assert_eq!(err.exit_code(), 4);
        assert!(err.to_string().contains("`toaster`"), "{err}");
        assert!(find(&[], "anything").is_err());
    }

    #[test]
    fn ambiguity_by_name_names_the_candidates() {
        // Two "Island Light"s — an exact-name tie is still a tie.
        let items = vec![
            dev(&["L1"], "Island Light"),
            dev(&["L2"], "Island Light"),
            dev(&["L3"], "Porch"),
        ];
        let err = find(&items, "Island Light").unwrap_err();
        assert!(matches!(err, CliError::NotFound(_)));
        let msg = err.to_string();
        assert!(msg.contains("more than one device"), "{msg}");
        assert!(msg.contains("Island Light (L1)"), "{msg}");
        assert!(msg.contains("Island Light (L2)"), "{msg}");
        assert!(!msg.contains("Porch"), "{msg}");
    }

    #[test]
    fn ambiguity_by_id_is_an_error_not_a_first_pick() {
        // Two records answering to the same id (case-folded): a guess here
        // would target the wrong device for a write.
        let items = vec![dev(&["ab:cd"], "One"), dev(&["AB:CD"], "Two")];
        let err = find(&items, "ab:cd").unwrap_err();
        assert!(matches!(err, CliError::NotFound(_)));
        let msg = err.to_string();
        assert!(
            msg.contains("One (ab:cd)") && msg.contains("Two (AB:CD)"),
            "{msg}"
        );
    }

    #[test]
    fn ambiguity_by_partial_name_names_the_candidates() {
        let items = fixture();
        let err = find(&items, "p").unwrap_err(); // lamP, Plug, striP
        assert!(matches!(err, CliError::NotFound(_)));
        let msg = err.to_string();
        assert!(
            msg.contains("Office Lamp") && msg.contains("Desk Plug"),
            "{msg}"
        );
    }

    #[test]
    fn candidate_labels_join_every_id() {
        let items = vec![dev(&["X_1", "1"], "Twin"), dev(&["X_2", "2"], "Twin")];
        let msg = find(&items, "twin").unwrap_err().to_string();
        assert!(msg.contains("Twin (X_1/1)"), "{msg}");
    }

    #[test]
    fn empty_query_is_a_usage_error_not_match_everything() {
        let only = vec![dev(&["1"], "Solo")];
        // With one item, "" would substring-match it and silently resolve.
        let err = find(&only, "   ").unwrap_err();
        assert!(matches!(err, CliError::Usage(_)));
    }
}
