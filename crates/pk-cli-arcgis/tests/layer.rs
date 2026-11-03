use std::cell::RefCell;

use pk_cli_arcgis::{quote, Layer, Query};
use pk_cli_core::block_on;
use pk_cli_core::transport::{HttpRequest, HttpResponse, Transport};
use pk_cli_core::CliError;

const PAGE_SHORT: &str = include_str!("fixtures/page_short.json");
const PAGE_LAST: &str = include_str!("fixtures/page_last.json");
const BAD_QUERY: &str = include_str!("fixtures/error_bad_query.json");
const COUNT: &str = include_str!("fixtures/count.json");

/// Answers each request from `route`, and keeps the URLs it was asked for.
struct Server<F> {
    route: F,
    seen: RefCell<Vec<String>>,
    agent: RefCell<Option<String>>,
}

impl<F: Fn(&str) -> (u16, String)> Transport for &Server<F> {
    async fn send(&self, request: HttpRequest) -> Result<HttpResponse, CliError> {
        let (status, body) = (self.route)(&request.url);
        *self.agent.borrow_mut() = request.header_value("user-agent").map(str::to_string);
        self.seen.borrow_mut().push(request.url);
        Ok(HttpResponse::new(status, body))
    }
}

fn server<F: Fn(&str) -> (u16, String)>(route: F) -> Server<F> {
    Server {
        route,
        seen: RefCell::new(Vec::new()),
        agent: RefCell::new(None),
    }
}

fn layer<F: Fn(&str) -> (u16, String)>(server: &Server<F>) -> Layer<&Server<F>> {
    Layer::new(server, "https://gis.example/FeatureServer/0/")
}

fn ok(body: &str) -> (u16, String) {
    (200, body.to_string())
}

fn rows<F: Fn(&str) -> (u16, String)>(
    server: &Server<F>,
) -> Result<Vec<serde_json::Value>, CliError> {
    block_on(layer(server).rows(&Query::new("1=1", "PARCEL")))
}

fn page_of(ids: &[&str], flag: &str) -> String {
    let features: Vec<String> = ids
        .iter()
        .map(|id| format!(r#"{{"attributes":{{"PARCEL":"{id}"}}}}"#))
        .collect();
    format!(r#"{{"features":[{}]{flag}}}"#, features.join(","))
}

fn pages(url: &str) -> (u16, String) {
    if url.contains("resultOffset=0") {
        ok(PAGE_SHORT)
    } else {
        ok(PAGE_LAST)
    }
}

#[test]
fn a_short_page_with_more_to_follow_advances_by_the_rows_received() {
    let server = server(pages);
    let got = rows(&server).unwrap();
    let parcels: Vec<&str> = got.iter().map(|r| r["PARCEL"].as_str().unwrap()).collect();
    assert_eq!(parcels, ["A1", "A2", "A3"]);
    let seen = server.seen.borrow();
    assert_eq!(seen.len(), 2);
    assert!(seen[1].contains("resultOffset=2"), "{}", seen[1]);
}

#[test]
fn a_full_page_without_the_flag_is_asked_past() {
    let server = server(|url| {
        if url.contains("resultOffset=0") {
            ok(&page_of(&["A1", "A2"], ""))
        } else if url.contains("resultOffset=2") {
            ok(&page_of(&["A3"], ""))
        } else {
            panic!("asked past a short page: {url}")
        }
    });
    let got = block_on(
        layer(&server)
            .page_size(2)
            .rows(&Query::new("1=1", "PARCEL")),
    )
    .unwrap();
    assert_eq!(got.len(), 3);
}

#[test]
fn a_result_ending_on_a_page_boundary_ends_on_the_empty_page() {
    let server = server(|url| {
        if url.contains("resultOffset=0") {
            ok(&page_of(&["A1", "A2"], ""))
        } else {
            ok(r#"{"features":[]}"#)
        }
    });
    let got = block_on(
        layer(&server)
            .page_size(2)
            .rows(&Query::new("1=1", "PARCEL")),
    )
    .unwrap();
    assert_eq!(got.len(), 2);
    assert_eq!(server.seen.borrow().len(), 2);
}

#[test]
fn a_layer_that_ignores_the_offset_fails_instead_of_looping() {
    let server = server(|_| ok(&page_of(&["A1", "A2"], "")));
    let err = block_on(
        layer(&server)
            .page_size(2)
            .rows(&Query::new("1=1", "PARCEL")),
    )
    .unwrap_err();
    assert_eq!(err.exit_code(), 5);
    assert_eq!(server.seen.borrow().len(), 2);
}

#[test]
fn the_limit_ends_paging_and_shapes_the_request() {
    let server = server(pages);
    let query = Query::new("CITY='X'", "PARCEL").fields("PARCEL").limit(2);
    assert_eq!(block_on(layer(&server).rows(&query)).unwrap().len(), 2);
    let seen = server.seen.borrow();
    assert_eq!(seen.len(), 1);
    assert!(
        seen[0].starts_with("https://gis.example/FeatureServer/0/query?"),
        "{}",
        seen[0]
    );
    for part in [
        "resultRecordCount=2",
        "outFields=PARCEL",
        "orderByFields=PARCEL",
        "returnGeometry=false",
        "f=json",
    ] {
        assert!(seen[0].contains(part), "{part} in {}", seen[0]);
    }
}

#[test]
fn a_server_sending_past_the_limit_is_cut_to_it() {
    let server = server(|_| ok(&page_of(&["A1", "A2", "A3"], "")));
    let query = Query::new("1=1", "PARCEL").limit(2);
    assert_eq!(block_on(layer(&server).rows(&query)).unwrap().len(), 2);
}

#[test]
fn without_a_limit_every_row_comes_back() {
    let server = server(pages);
    assert_eq!(rows(&server).unwrap().len(), 3);
    assert!(server.seen.borrow()[0].contains("resultRecordCount=1000"));
}

#[test]
fn the_page_size_caps_each_request() {
    let server = server(pages);
    block_on(
        layer(&server)
            .page_size(2)
            .rows(&Query::new("1=1", "PARCEL")),
    )
    .unwrap();
    assert!(server.seen.borrow()[0].contains("resultRecordCount=2"));
}

#[test]
fn an_error_in_a_200_body_keeps_its_exit_code() {
    let bad_query = server(|_| ok(BAD_QUERY));
    let err = rows(&bad_query).unwrap_err();
    assert_eq!(err.exit_code(), 2);
    assert!(err.to_string().contains("Unable to perform query"), "{err}");

    let exit_for = |body: &str| {
        let body = body.to_string();
        let server = server(move |_| ok(&body));
        let code = block_on(layer(&server).count("1=1"))
            .unwrap_err()
            .exit_code();
        code
    };
    assert_eq!(exit_for(r#"{"error":{"code":404,"message":"no"}}"#), 4);
    assert_eq!(exit_for(r#"{"error":{"code":500,"message":"no"}}"#), 5);
    assert_eq!(exit_for(r#"{"error":{"message":"no"}}"#), 5);
}

#[test]
fn an_error_with_no_code_does_not_invent_one() {
    let server = server(|_| ok(r#"{"error":{"message":"no"}}"#));
    let err = block_on(layer(&server).count("1=1")).unwrap_err();
    assert!(!err.to_string().contains('0'), "{err}");
}

#[test]
fn a_400_on_a_later_page_is_upstream() {
    let server = server(|url| {
        if url.contains("resultOffset=0") {
            ok(PAGE_SHORT)
        } else {
            ok(BAD_QUERY)
        }
    });
    assert_eq!(rows(&server).unwrap_err().exit_code(), 5);
}

#[test]
fn a_changed_response_fails_instead_of_reading_as_no_rows() {
    for body in [
        r#"{"rows":[]}"#,
        r#"{"features":[{"id":1}]}"#,
        r#"{"features":[{"id":1}],"exceededTransferLimit":true}"#,
        r#"{"features":[{"attributes":{"PARCEL":"A1"}}],"exceededTransferLimit":"true"}"#,
        "<html>down for maintenance</html>",
    ] {
        let body = body.to_string();
        let server = server(move |_| ok(&body));
        let err = rows(&server).unwrap_err();
        assert_eq!(err.exit_code(), 5, "{err}");
    }
    let server = server(|_| ok(r#"{"rows":[]}"#));
    let err = block_on(layer(&server).count("1=1")).unwrap_err();
    assert_eq!(err.exit_code(), 5);
}

#[test]
fn http_failures_keep_their_exit_codes() {
    let exit_for = |status: u16| {
        let server = server(move |_| (status, String::new()));
        rows(&server).unwrap_err().exit_code()
    };
    assert_eq!(exit_for(404), 4);
    assert_eq!(exit_for(503), 5);
}

#[test]
fn an_empty_result_is_valid_but_more_to_follow_with_no_rows_is_not() {
    let empty = server(|_| ok(r#"{"features":[]}"#));
    assert!(rows(&empty).unwrap().is_empty());
    let stuck = server(|_| ok(r#"{"features":[],"exceededTransferLimit":true}"#));
    assert_eq!(rows(&stuck).unwrap_err().exit_code(), 5);
}

#[test]
fn count_reads_the_total() {
    let server = server(|_| ok(COUNT));
    assert_eq!(block_on(layer(&server).count("CITY='X'")).unwrap(), 837);
    assert!(server.seen.borrow()[0].contains("returnCountOnly=true"));
}

#[test]
fn requests_carry_the_user_agent_when_one_is_set() {
    let server = server(|_| ok(COUNT));
    block_on(layer(&server).count("1=1")).unwrap();
    assert_eq!(*server.agent.borrow(), None);
    block_on(layer(&server).user_agent("slca", "0.1.0").count("1=1")).unwrap();
    assert_eq!(server.agent.borrow().as_deref(), Some("slca/0.1.0"));
}

#[test]
fn quote_keeps_input_inside_the_literal() {
    assert_eq!(quote("MAIN ST"), "'MAIN ST'");
    assert_eq!(quote("O'BRIEN"), "'O''BRIEN'");
    assert_eq!(quote("X' OR '1'='1"), "'X'' OR ''1''=''1'");
}
