//! The shared `utility/v1` toolset, offline: the records come from
//! `tests/fixtures/` (see its README), and a log shows what reached the source.

use std::cell::RefCell;
use std::rc::Rc;

use pk_cli_core::{block_on, CliError, Money};
use pk_cli_mcp::conformance::{assert_conforms, assert_fits, call, definitions_size};
use pk_cli_mcp::{call_tool, instructions, tools_list, Server, Toolset};
use pk_cli_utility::mcp::{UtilityAccount, UtilitySource, UtilityTools};
use pk_cli_utility::{Payment, Statement, UsagePeriod, UtilitySummary};
use serde_json::{json, Value};

fn fixture<T: serde::de::DeserializeOwned>(name: &str) -> T {
    let path = format!("{}/tests/fixtures/{name}.json", env!("CARGO_MANIFEST_DIR"));
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

type Log = Rc<RefCell<Vec<String>>>;

struct Fixtures {
    log: Log,
    accounts: Vec<UtilityAccount>,
    summary: UtilitySummary,
    payments: Vec<Payment>,
    /// The error a source method (by name) fails with.
    fail: fn(&str) -> Option<CliError>,
}

impl Fixtures {
    fn new() -> Self {
        Self {
            log: Log::default(),
            accounts: fixture("accounts"),
            summary: fixture("summary"),
            payments: fixture("payments"),
            fail: |_| None,
        }
    }

    fn enter(&self, method: &str, detail: String) -> Result<(), CliError> {
        self.log
            .borrow_mut()
            .push(format!("{method} {detail}").trim().to_string());
        (self.fail)(method).map_or(Ok(()), Err)
    }
}

impl UtilitySource for Fixtures {
    async fn accounts(&self) -> Result<Vec<UtilityAccount>, CliError> {
        self.enter("accounts", String::new())?;
        Ok(self.accounts.clone())
    }

    async fn summary(&self, account: Option<&str>) -> Result<UtilitySummary, CliError> {
        self.enter("summary", format!("{account:?}"))?;
        Ok(self.summary.clone())
    }

    async fn statements(&self, account: Option<&str>) -> Result<Vec<Statement>, CliError> {
        self.enter("statements", format!("{account:?}"))?;
        Ok(fixture("statements"))
    }

    async fn payments(
        &self,
        account: Option<&str>,
        since: Option<&str>,
    ) -> Result<Vec<Payment>, CliError> {
        self.enter("payments", format!("{account:?} {since:?}"))?;
        Ok(self.payments.clone())
    }

    async fn usage(&self, account: Option<&str>) -> Result<Vec<UsagePeriod>, CliError> {
        self.enter("usage", format!("{account:?}"))?;
        Ok(fixture("usage"))
    }
}

fn server() -> Server {
    Server::new("util", "0.0.0", "Example Utility", "util")
}

fn over(source: Fixtures) -> (UtilityTools<Fixtures>, Log) {
    let log = source.log.clone();
    (
        UtilityTools::new(server(), source).metering("electricity"),
        log,
    )
}

fn tools() -> (UtilityTools<Fixtures>, Log) {
    over(Fixtures::new())
}

fn names<S: UtilitySource>(t: &UtilityTools<S>) -> Vec<String> {
    t.tools().into_iter().map(|d| d.name).collect()
}

/// A successful call's result, held to the tool's output schema and the budget.
fn ok<S: UtilitySource>(t: &UtilityTools<S>, tool: &str, args: Value) -> Value {
    let result = call(t, tool, args);
    assert_fits(t, tool, &result).clone()
}

/// A failed call's text.
fn err<S: UtilitySource>(t: &UtilityTools<S>, tool: &str, args: Value) -> String {
    let result = call(t, tool, args);
    assert!(
        result.is_error(),
        "{tool} should have failed: {}",
        result.text()
    );
    result.text().to_string()
}

fn dates(page: &Value, key: &str) -> Vec<String> {
    page["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i[key].as_str().unwrap_or("").to_string())
        .collect()
}

#[test]
fn every_shape_of_the_toolset_conforms() {
    assert_conforms(&tools().0);
    assert_conforms(&UtilityTools::new(server(), Fixtures::new()));
    assert_conforms(&tools().0.without_usage());
    assert_conforms(&tools().0.without_payments().without_usage());
    assert_conforms(&tools().0.single_account());
}

#[test]
fn the_definitions_stay_under_the_budget_with_the_longest_family_names() {
    let t = UtilityTools::new(
        Server::new("lrfl", "0.0.0", "Loxahatchee River District", "loxahatchee"),
        Fixtures::new(),
    )
    .metering("metered water use");
    assert_conforms(&t);
    assert!(definitions_size(&t) < 8_000, "{}", definitions_size(&t));
}

#[test]
fn listing_the_tools_reaches_no_source() {
    let (t, log) = tools();
    assert_eq!(
        names(&t),
        ["util_balance", "util_bills", "util_payments", "util_usage"]
    );
    let _ = tools_list(&t);
    let _ = instructions(&t);
    assert!(log.borrow().is_empty(), "{:?}", log.borrow());
}

#[test]
fn every_tool_is_read_only() {
    for tool in tools().0.tools() {
        assert!(tool.annotations.read_only, "{}", tool.name);
        assert!(!tool.annotations.destructive, "{}", tool.name);
    }
}

#[test]
fn balance_reads_the_default_account_and_names_the_others() {
    let (t, log) = tools();
    let out = ok(&t, "util_balance", json!({}));
    assert_eq!(
        out,
        json!({
            "balance": "12.34",
            "due_date": "2026-08-01",
            "autopay": true,
            "account": "100 Example Street",
            "accounts": [{"name": "100 Example Street"}, {"name": "200 Example Street"}]
        })
    );
    assert_eq!(*log.borrow(), ["accounts", "summary None"]);
}

#[test]
fn only_detailed_sends_account_numbers() {
    let (t, _) = tools();
    let out = ok(&t, "util_balance", json!({"response_format": "detailed"}));
    assert_eq!(out["account"], "100 Example Street");
    assert_eq!(out["account_id"], "1234567890");
    assert_eq!(out["accounts"][1]["id"], "2345678901");
}

#[test]
fn an_account_with_no_name_shows_only_the_end_of_its_number() {
    let mut source = Fixtures::new();
    source.accounts.clear();
    let (t, _) = over(source);
    let out = ok(&t, "util_balance", json!({}));
    assert_eq!(out["account"], "ending 7890");
    assert!(out.get("account_id").is_none(), "{out}");
}

#[test]
fn a_listed_account_with_no_name_keeps_the_number_it_is_asked_for_by() {
    let mut source = Fixtures::new();
    source.accounts[1].name = None;
    let (t, _) = over(source);
    let out = ok(&t, "util_balance", json!({}));
    assert_eq!(
        out["accounts"],
        json!([{"name": "100 Example Street"}, {"id": "2345678901"}])
    );
}

#[test]
fn balance_resolves_an_address_to_the_account_id() {
    let (t, log) = tools();
    ok(&t, "util_balance", json!({"account": "200 example"}));
    assert_eq!(*log.borrow(), ["accounts", "summary Some(\"2345678901\")"]);
}

#[test]
fn a_login_with_one_account_lists_no_others() {
    let mut source = Fixtures::new();
    source.accounts.truncate(1);
    let (t, _) = over(source);
    let out = ok(&t, "util_balance", json!({}));
    assert!(out.get("accounts").is_none(), "{out}");
    assert!(out.get("accounts_unavailable").is_none(), "{out}");
}

#[test]
fn an_unreadable_account_list_keeps_the_balance_and_says_the_list_is_missing() {
    let mut source = Fixtures::new();
    source.fail = |m| (m == "accounts").then(|| CliError::Upstream("the list failed".into()));
    let (t, _) = over(source);
    let out = ok(&t, "util_balance", json!({}));
    assert_eq!(out["balance"], "12.34");
    assert_eq!(out["accounts_unavailable"], true);
    assert!(err(&t, "util_balance", json!({"account": "100"})).contains("the list failed"));
}

#[test]
fn an_auth_failure_on_the_list_alone_is_also_flagged_rather_than_read_as_one_account() {
    let mut source = Fixtures::new();
    source.fail = |m| (m == "accounts").then(|| CliError::Auth("signed out".into()));
    let (t, _) = over(source);
    let out = ok(&t, "util_balance", json!({}));
    assert_eq!(out["balance"], "12.34");
    assert_eq!(out["accounts_unavailable"], true);
}

#[test]
fn an_account_nothing_matches_stops_before_the_read_and_says_what_to_do() {
    let (t, log) = tools();
    let text = err(&t, "util_bills", json!({"account": "nowhere"}));
    assert!(text.starts_with("not_found:"), "{text}");
    assert!(text.contains("util_balance with no account"), "{text}");
    assert_eq!(*log.borrow(), ["accounts"]);
}

#[test]
fn an_ambiguous_account_names_the_candidates_rather_than_picking_one() {
    let (t, log) = tools();
    let text = err(&t, "util_balance", json!({"account": "Example Street"}));
    assert!(text.contains("100 Example Street"), "{text}");
    assert!(text.contains("200 Example Street"), "{text}");
    assert!(!text.contains("1234567890"), "{text}");
    assert!(!text.contains("2345678901"), "{text}");
    assert!(text.contains("ending 7890"), "{text}");
    assert!(text.contains("response_format"), "{text}");
    assert_eq!(*log.borrow(), ["accounts"]);
}

#[test]
fn accounts_sharing_a_name_keep_the_numbers_that_tell_them_apart() {
    let mut source = Fixtures::new();
    source.accounts[1].name = source.accounts[0].name.clone();
    let (t, _) = over(source);
    let out = ok(&t, "util_balance", json!({}));
    assert_eq!(out["accounts"][0]["id"], "1234567890");
    assert_eq!(out["accounts"][1]["id"], "2345678901");
}

#[test]
fn a_provider_with_no_list_takes_the_argument_as_the_id() {
    let mut source = Fixtures::new();
    source.accounts.clear();
    let (t, log) = over(source);
    ok(&t, "util_usage", json!({"account": "0000000000"}));
    assert_eq!(*log.borrow(), ["accounts", "usage Some(\"0000000000\")"]);
}

#[test]
fn with_no_list_an_argument_that_cannot_be_a_number_never_reaches_the_source() {
    let mut source = Fixtures::new();
    source.accounts.clear();
    let (t, log) = over(source);
    let long = "1".repeat(65);
    for account in [
        "../other/9",
        "1?all=true",
        "12/34",
        "12%2F34",
        "a..b",
        "100 Example Street",
        long.as_str(),
    ] {
        let text = err(&t, "util_bills", json!({"account": account}));
        assert!(text.starts_with("usage:"), "{account}: {text}");
    }
    assert!(
        log.borrow().iter().all(|c| c == "accounts"),
        "{:?}",
        log.borrow()
    );
    ok(&t, "util_bills", json!({"account": "12-34_5.6"}));
}

#[test]
fn bills_are_concise_by_default_and_detailed_adds_the_id() {
    let (t, _) = tools();
    let page = ok(&t, "util_bills", json!({}));
    assert_eq!(page["returned"], 4);
    assert!(page.get("next_cursor").is_none());
    assert_eq!(
        page["items"][0],
        json!({"date": "2026-06-15", "amount": "12.34", "due_date": "2026-07-05", "paid": false})
    );
    let page = ok(&t, "util_bills", json!({"response_format": "detailed"}));
    assert_eq!(page["items"][0]["id"], "2026-06-15");
}

#[test]
fn bills_filter_by_date_and_keep_a_row_with_no_date() {
    let (t, _) = tools();
    let page = ok(
        &t,
        "util_bills",
        json!({"since": "2026-05-01", "until": "2026-05-31"}),
    );
    assert_eq!(dates(&page, "date"), ["2026-05-15", ""]);
    assert_eq!(page["items"][1]["amount"], "45.67");
}

fn payment(date: &str, confirmation: Option<&str>) -> Payment {
    Payment {
        date: date.into(),
        amount: Money {
            amount: "10.00".into(),
            currency: "USD".into(),
        },
        method: None,
        status: None,
        confirmation: confirmation.map(String::from),
    }
}

/// A payment posting between pages must not show one twice, which would double a total.
#[test]
fn a_payment_that_arrives_between_pages_repeats_nothing() {
    let mut source = Fixtures::new();
    source.payments = vec![
        payment("2026-03-02", Some("B")),
        payment("2026-03-01", Some("A")),
    ];
    let (t, _) = over(source);
    let first = ok(&t, "util_payments", json!({"limit": 1}));
    assert_eq!(dates(&first, "date"), ["2026-03-02"]);
    let cursor = first["next_cursor"].as_str().unwrap().to_string();

    let mut source = Fixtures::new();
    source.payments = vec![
        payment("2026-03-03", Some("C")),
        payment("2026-03-02", Some("B")),
        payment("2026-03-01", Some("A")),
    ];
    let (t, _) = over(source);
    let second = ok(&t, "util_payments", json!({"limit": 1, "cursor": cursor}));
    assert_eq!(dates(&second, "date"), ["2026-03-01"]);
    assert!(second.get("next_cursor").is_none());
}

/// A confirmation number is not always there: the amount tells that day's payments apart,
/// and two the same are counted, so neither is lost at a page boundary.
#[test]
fn payments_with_nothing_to_tell_them_apart_are_all_seen() {
    let mut source = Fixtures::new();
    source.payments = vec![
        payment("2026-03-01", None),
        payment("2026-03-01", None),
        payment("2026-02-01", None),
    ];
    let (t, _) = over(source);
    let mut seen = Vec::new();
    let mut args = json!({"limit": 1});
    loop {
        let page = ok(&t, "util_payments", args.clone());
        seen.extend(dates(&page, "date"));
        match page.get("next_cursor") {
            Some(cursor) => args["cursor"] = cursor.clone(),
            None => break,
        }
    }
    assert_eq!(seen, ["2026-03-01", "2026-03-01", "2026-02-01"]);
}

#[test]
fn a_cursor_walks_the_list_and_refuses_other_filters() {
    let (t, _) = tools();
    let first = ok(&t, "util_bills", json!({"limit": 3}));
    assert_eq!(first["returned"], 3);
    let cursor = first["next_cursor"].as_str().unwrap();
    let rest = ok(&t, "util_bills", json!({"limit": 3, "cursor": cursor}));
    assert_eq!(rest["returned"], 1);
    assert!(rest.get("next_cursor").is_none());
    assert_eq!(rest["items"][0]["amount"], "45.67");

    let text = err(
        &t,
        "util_bills",
        json!({"cursor": cursor, "since": "2026-01-01"}),
    );
    assert!(text.contains("different filters"), "{text}");
}

#[test]
fn payments_hold_method_and_confirmation_for_detailed() {
    let (t, log) = tools();
    let page = ok(&t, "util_payments", json!({"since": "2026-06-01"}));
    assert_eq!(
        page["items"],
        json!([{"date": "2026-06-20", "amount": "23.45", "status": "posted"}])
    );
    assert_eq!(
        *log.borrow(),
        ["payments None Some(\"2026-06-01\")"],
        "the source gets the bound, for a provider whose query takes one"
    );
    let page = ok(&t, "util_payments", json!({"response_format": "detailed"}));
    assert_eq!(page["items"][1]["method"], "CARD");
    assert_eq!(page["items"][1]["confirmation"], "CONF0002");
}

#[test]
fn usage_periods_overlap_the_range_by_month_and_keep_what_cannot_be_read() {
    let (t, _) = tools();
    let page = ok(
        &t,
        "util_usage",
        json!({"since": "2026-05-29", "until": "2026-06-10"}),
    );
    assert_eq!(dates(&page, "period"), ["2026-06", "Spring cycle"]);
    assert_eq!(page["items"][0]["quantity"], 111.5);
    assert_eq!(page["items"][0]["unit"], "kWh");
    assert_eq!(page["items"][0]["cost"], "12.34");

    let page = ok(&t, "util_usage", json!({"until": "2026-05-28"}));
    assert_eq!(
        dates(&page, "period"),
        ["2026-05-28", "2026-04", "Spring cycle"]
    );
}

#[test]
fn a_bad_argument_fails_before_any_provider_call() {
    let (t, log) = tools();
    for (tool, args, says) in [
        ("util_bills", json!({"limit": 0}), "limit"),
        ("util_bills", json!({"limit": 201}), "limit"),
        ("util_payments", json!({"cursor": "not-ours"}), "cursor"),
        ("util_usage", json!({"since": "06-01-2026"}), "since"),
        ("util_usage", json!({"until": "soon"}), "until"),
        (
            "util_bills",
            json!({"since": "2026-06-02", "until": "2026-06-01"}),
            "after until",
        ),
        ("util_bills", json!({"response_format": "huge"}), "huge"),
        ("util_balance", json!({"since": "2026-06-01"}), "since"),
        ("util_balance", json!({"response_format": "huge"}), "huge"),
    ] {
        let text = err(&t, tool, args);
        assert!(text.starts_with("usage:"), "{tool}: {text}");
        assert!(text.contains(says), "{tool}: {text}");
    }
    assert!(log.borrow().is_empty(), "{:?}", log.borrow());
}

#[test]
fn a_tool_the_provider_lacks_is_not_served() {
    let t = tools().0.without_payments().without_usage();
    assert_eq!(names(&t), ["util_balance", "util_bills"]);
    assert!(block_on(call_tool(&t, "util_usage", None)).is_err());
    assert!(block_on(call_tool(&t, "util_payments", None)).is_err());
}

#[test]
fn a_one_account_provider_takes_no_account_argument() {
    let (t, log) = tools();
    let t = t.single_account();
    for tool in t.tools() {
        assert!(
            !tool.input_properties().contains(&"account"),
            "{}",
            tool.name
        );
        assert!(!tool.description.contains("account or"), "{}", tool.name);
    }
    let text = err(&t, "util_bills", json!({"account": "1234567890"}));
    assert!(text.starts_with("usage:"), "{text}");
    assert!(log.borrow().is_empty());

    let out = ok(&t, "util_balance", json!({}));
    assert!(out.get("accounts").is_none(), "{out}");
    assert_eq!(*log.borrow(), ["summary None"]);
}

#[test]
fn an_auth_failure_asks_for_the_server_s_sign_in_and_only_that() {
    fn expired(_: &str) -> Option<CliError> {
        Some(CliError::Auth(
            "session expired \u{2014} run `util init` again".into(),
        ))
    }
    let mut source = Fixtures::new();
    source.fail = expired;
    let (t, _) = over(source);
    let text = err(&t, "util_usage", json!({}));
    assert!(text.starts_with("auth: session expired."), "{text}");
    assert!(text.contains("`util auth login`"), "{text}");
    assert!(!text.contains("util init"), "{text}");

    let mut source = Fixtures::new();
    source.fail = expired;
    let t = UtilityTools::new(server().sign_in_command("util auth login --stdin"), source);
    let text = err(&t, "util_bills", json!({}));
    assert!(text.contains("`util auth login --stdin`"), "{text}");
    assert!(!text.contains("util init"), "{text}");
    assert!(instructions(&t).contains("`util auth login --stdin`"));
}

#[test]
fn a_provider_outage_keeps_its_kind() {
    let mut source = Fixtures::new();
    source.fail = |_| Some(CliError::Upstream("the portal answered 503".into()));
    let (t, _) = over(source);
    let text = err(&t, "util_bills", json!({}));
    assert!(text.starts_with("upstream:"), "{text}");
    assert!(text.contains("503"), "{text}");
}

#[test]
fn a_currency_other_than_usd_stays_with_the_amount() {
    let mut source = Fixtures::new();
    source.summary.balance = Money {
        amount: "12.34".into(),
        currency: "CAD".into(),
    };
    let (t, _) = over(source);
    let out = ok(&t, "util_balance", json!({}));
    assert_eq!(out["balance"], "12.34");
    assert_eq!(out["currency"], "CAD");
}

#[test]
fn a_page_over_the_size_budget_truncates_and_the_cursor_reaches_every_row() {
    let mut source = Fixtures::new();
    // Rows sized to break the budget, generated because their content is not
    // the point.
    source.payments = (0..200)
        .map(|i| Payment {
            date: format!("2026-01-{:02}", i % 28 + 1),
            amount: Money {
                amount: format!("{i}.00"),
                currency: "USD".into(),
            },
            method: Some("EFT".into()),
            status: Some("posted".into()),
            confirmation: Some(format!("{i:0>400}")),
        })
        .collect();
    let (t, _) = over(source);
    let mut seen = 0;
    let mut pages = 0;
    let mut cursor: Option<String> = None;
    loop {
        let mut args = json!({"limit": 200, "response_format": "detailed"});
        if let Some(c) = &cursor {
            args["cursor"] = json!(c);
        }
        let page = ok(&t, "util_payments", args);
        pages += 1;
        seen += page["returned"].as_u64().unwrap();
        if page["truncated"] == true {
            assert!(page["hint"].as_str().unwrap().contains("since/until"));
        }
        match page["next_cursor"].as_str() {
            Some(c) => cursor = Some(c.to_string()),
            None => break,
        }
    }
    assert!(pages > 1, "the rows should not fit one page");
    assert_eq!(seen, 200);
}
