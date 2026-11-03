use std::io::{Read, Write};
use std::sync::{Arc, Mutex};

use pk_cli_arcgis::{quote, Layer, Query};

const PAGE_SHORT: &str = include_str!("fixtures/page_short.json");
const PAGE_LAST: &str = include_str!("fixtures/page_last.json");
const BAD_QUERY: &str = include_str!("fixtures/error_bad_query.json");
const COUNT: &str = include_str!("fixtures/count.json");

/// A layer whose server answers from `route`, and the request paths it saw.
fn layer(route: impl Fn(&str) -> String + Send + 'static) -> (Layer, Arc<Mutex<Vec<String>>>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/FeatureServer/0/", listener.local_addr().unwrap());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(mut conn) = conn else { return };
            let mut buf = [0u8; 8192];
            let n = conn.read(&mut buf).unwrap_or(0);
            let head = String::from_utf8_lossy(&buf[..n]).to_string();
            let path = head.split_whitespace().nth(1).unwrap_or("").to_string();
            let body = route(&path);
            log.lock().unwrap().push(path);
            let reply = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = conn.write_all(reply.as_bytes());
        }
    });
    let http = reqwest::blocking::Client::new();
    (Layer::new(http, url), seen)
}

fn pages(path: &str) -> String {
    if path.contains("resultOffset=0") {
        PAGE_SHORT.to_string()
    } else {
        PAGE_LAST.to_string()
    }
}

#[test]
fn a_short_page_with_more_to_follow_advances_by_the_rows_received() {
    let (layer, seen) = layer(pages);
    let rows = layer.rows(&Query::new("1=1", "PARCEL").limit(100)).unwrap();
    let parcels: Vec<&str> = rows.iter().map(|r| r["PARCEL"].as_str().unwrap()).collect();
    assert_eq!(parcels, ["A1", "A2", "A3"]);
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 2);
    assert!(seen[1].contains("resultOffset=2"), "{}", seen[1]);
}

#[test]
fn the_limit_ends_paging_and_shapes_the_request() {
    let (layer, seen) = layer(pages);
    let query = Query::new("CITY='X'", "PARCEL").fields("PARCEL").limit(2);
    assert_eq!(layer.rows(&query).unwrap().len(), 2);
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    for part in [
        "/FeatureServer/0/query?",
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
fn the_page_size_caps_each_request() {
    let (layer, seen) = layer(pages);
    layer
        .page_size(2)
        .rows(&Query::new("1=1", "PARCEL"))
        .unwrap();
    assert!(seen.lock().unwrap()[0].contains("resultRecordCount=2"));
}

#[test]
fn an_error_in_a_200_body_keeps_its_exit_code() {
    let (bad_query, _) = layer(|_| BAD_QUERY.to_string());
    let err = bad_query
        .rows(&Query::new("nonsense", "PARCEL"))
        .unwrap_err();
    assert_eq!(err.exit_code(), 2);
    assert!(err.to_string().contains("Unable to perform query"), "{err}");

    let with_code = |code: i64| {
        let body = format!(r#"{{"error":{{"code":{code},"message":"no"}}}}"#);
        let (layer, _) = layer(move |_| body.clone());
        layer.count("1=1").unwrap_err().exit_code()
    };
    assert_eq!(with_code(404), 4);
    assert_eq!(with_code(500), 5);
}

#[test]
fn a_changed_response_fails_instead_of_reading_as_no_rows() {
    let (layer, _) = layer(|_| r#"{"rows":[]}"#.to_string());
    let err = layer.rows(&Query::new("1=1", "PARCEL")).unwrap_err();
    assert_eq!(err.exit_code(), 5);
    assert_eq!(layer.count("1=1").unwrap_err().exit_code(), 5);
}

#[test]
fn an_empty_result_is_valid_but_more_to_follow_with_no_rows_is_not() {
    let (empty, _) = layer(|_| r#"{"features":[]}"#.to_string());
    assert!(empty.rows(&Query::new("1=1", "PARCEL")).unwrap().is_empty());
    let (stuck, _) = layer(|_| r#"{"features":[],"exceededTransferLimit":true}"#.to_string());
    assert_eq!(
        stuck
            .rows(&Query::new("1=1", "PARCEL"))
            .unwrap_err()
            .exit_code(),
        5
    );
}

#[test]
fn count_reads_the_total() {
    let (layer, seen) = layer(|_| COUNT.to_string());
    assert_eq!(layer.count("CITY='X'").unwrap(), 837);
    assert!(seen.lock().unwrap()[0].contains("returnCountOnly=true"));
}

#[test]
fn quote_keeps_input_inside_the_literal() {
    assert_eq!(quote("MAIN ST"), "'MAIN ST'");
    assert_eq!(quote("O'BRIEN"), "'O''BRIEN'");
    assert_eq!(quote("X' OR '1'='1"), "'X'' OR ''1''=''1'");
}
