//! `example-cli mcp --http` end to end: the real binary on 127.0.0.1, driven
//! with raw HTTP the way the connector's Worker reaches it through a tunnel.
//! The token comes from the environment, so no test opens the keychain.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

const TOKEN: &str = "a7c1e9f0b2d4a6c8e0f1a3b5c7d9e1f2a4b6c8d0e2f4a6b8c0d2e4f6a8b0c2d4";
const TOKEN_VAR: &str = "EXAMPLE_CLI_MCP_TOKEN";

fn bin() -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_example-cli"));
    c.env_remove(TOKEN_VAR);
    c
}

/// A port nothing listens on right now.
fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

struct Server {
    child: Child,
    port: u16,
}

impl Server {
    fn start(extra: &[&str]) -> Self {
        let port = free_port();
        let child = bin()
            .args(["mcp", "--http", "--port", &port.to_string()])
            .args(extra)
            .env(TOKEN_VAR, TOKEN)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("example-cli starts");
        let s = Self { child, port };
        let deadline = Instant::now() + Duration::from_secs(20);
        while TcpStream::connect(s.addr()).is_err() {
            assert!(Instant::now() < deadline, "the server did not listen");
            std::thread::sleep(Duration::from_millis(50));
        }
        s
    }

    fn addr(&self) -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], self.port))
    }

    /// One request on a fresh connection: the status, the headers (names
    /// lower-case) and the body.
    fn raw(&self, method: &str, path: &str, headers: &[(&str, &str)], body: &str) -> Reply {
        let mut stream = TcpStream::connect(self.addr()).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(20)))
            .unwrap();
        let mut req = format!("{method} {path} HTTP/1.1\r\nConnection: close\r\n");
        for (name, value) in headers {
            req.push_str(&format!("{name}: {value}\r\n"));
        }
        req.push_str(&format!("Content-Length: {}\r\n\r\n{body}", body.len()));
        stream.write_all(req.as_bytes()).unwrap();
        let mut raw = String::new();
        stream.read_to_string(&mut raw).unwrap();
        let (head, body) = raw.split_once("\r\n\r\n").expect("a response head");
        let mut lines = head.lines();
        let status: u16 = lines
            .next()
            .unwrap()
            .split(' ')
            .nth(1)
            .unwrap()
            .parse()
            .unwrap();
        let headers = lines
            .filter_map(|l| l.split_once(':'))
            .map(|(n, v)| (n.trim().to_ascii_lowercase(), v.trim().to_string()))
            .collect();
        Reply {
            status,
            headers,
            body: body.to_string(),
        }
    }

    /// A JSON-RPC request as the Worker sends it: loopback Host, the token.
    fn rpc(&self, id: u64, method: &str, params: Value) -> (u16, Value) {
        let host = format!("127.0.0.1:{}", self.port);
        let auth = format!("Bearer {TOKEN}");
        let body = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        let reply = self.raw(
            "POST",
            "/mcp",
            &[
                ("Host", &host),
                ("Authorization", &auth),
                ("Content-Type", "application/json"),
                ("Accept", "application/json, text/event-stream"),
                ("MCP-Protocol-Version", "2025-11-25"),
            ],
            &body.to_string(),
        );
        let json = serde_json::from_str(reply.body.trim()).unwrap_or(Value::Null);
        (reply.status, json)
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct Reply {
    status: u16,
    headers: Vec<(String, String)>,
    body: String,
}

impl Reply {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }
}

#[test]
fn the_worker_lists_and_calls_the_read_only_tools() {
    let s = Server::start(&[]);
    let (status, init) = s.rpc(
        1,
        "initialize",
        json!({
            "protocolVersion": "2025-11-25",
            "capabilities": {},
            "clientInfo": {"name": "connector", "version": "0"}
        }),
    );
    assert_eq!(status, 200, "{init}");
    assert_eq!(init["result"]["serverInfo"]["name"], "example-cli");
    let instructions = init["result"]["instructions"].as_str().unwrap();
    assert!(
        !instructions.contains("change data"),
        "no write tool is served, so the instructions say nothing of writes: {instructions}"
    );

    // Stateless: no session, and no initialize needed first.
    let (status, list) = s.rpc(2, "tools/list", json!({}));
    assert_eq!(status, 200, "{list}");
    let names: Vec<&str> = list["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["example_devices"], "the write tool is not listed");

    let (_, call) = s.rpc(
        3,
        "tools/call",
        json!({"name": "example_devices", "arguments": {"room": "Office"}}),
    );
    assert_eq!(call["result"]["isError"], false, "{call}");
    assert_eq!(call["result"]["structuredContent"]["returned"], 2);

    let (_, write) = s.rpc(
        4,
        "tools/call",
        json!({"name": "example_device_rename", "arguments": {"device": "Lamp", "name": "x"}}),
    );
    assert_eq!(
        write["error"]["code"], -32602,
        "a write tool is unknown, not merely unlisted: {write}"
    );
}

#[test]
fn allow_writes_serves_the_write_tools_too() {
    let s = Server::start(&["--allow-writes"]);
    let (_, list) = s.rpc(1, "tools/list", json!({}));
    let names: Vec<&str> = list["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["example_devices", "example_device_rename"]);
}

#[test]
fn requests_without_the_token_or_from_elsewhere_are_refused() {
    let s = Server::start(&[]);
    let host = format!("127.0.0.1:{}", s.port);
    let good = format!("Bearer {TOKEN}");
    let wrong = format!("Bearer {}", TOKEN.replace('a', "b"));
    let list = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#;
    let json = ("Content-Type", "application/json");
    let accept = ("Accept", "application/json, text/event-stream");

    let r = s.raw("POST", "/mcp", &[("Host", &host), json, accept], list);
    assert_eq!(r.status, 401, "no token: {}", r.body);
    assert_eq!(r.header("www-authenticate"), Some("Bearer"));
    assert!(!r.body.contains("tools"), "{}", r.body);

    let r = s.raw(
        "POST",
        "/mcp",
        &[("Host", &host), ("Authorization", &wrong), json, accept],
        list,
    );
    assert_eq!(r.status, 401, "wrong token: {}", r.body);

    // DNS rebinding: a page on another name that resolves to 127.0.0.1.
    let r = s.raw(
        "POST",
        "/mcp",
        &[
            ("Host", "rebind.example"),
            ("Authorization", &good),
            json,
            accept,
        ],
        list,
    );
    assert_eq!(r.status, 403, "foreign Host: {}", r.body);

    let r = s.raw(
        "POST",
        "/mcp",
        &[
            ("Host", &host),
            ("Origin", "https://evil.example"),
            ("Authorization", &good),
            json,
            accept,
        ],
        list,
    );
    assert_eq!(r.status, 403, "an Origin: {}", r.body);

    // No CORS: a preflight is just a method this server does not answer.
    let r = s.raw(
        "OPTIONS",
        "/mcp",
        &[
            ("Host", &host),
            ("Origin", "https://evil.example"),
            ("Access-Control-Request-Method", "POST"),
        ],
        "",
    );
    assert_eq!(r.status, 403);
    let no_cors = |r: &Reply| {
        assert!(
            !r.headers
                .iter()
                .any(|(n, _)| n.starts_with("access-control-")),
            "{:?}",
            r.headers
        )
    };
    no_cors(&r);
    for method in ["GET", "OPTIONS", "DELETE"] {
        let r = s.raw(
            method,
            "/mcp",
            &[("Host", &host), ("Authorization", &good)],
            "",
        );
        assert_eq!(r.status, 405, "{method}");
        assert_eq!(r.header("allow"), Some("POST"));
        no_cors(&r);
    }
    let r = s.raw(
        "POST",
        "/",
        &[("Host", &host), ("Authorization", &good)],
        list,
    );
    assert_eq!(r.status, 404);
}

#[test]
fn it_listens_on_loopback_only() {
    let s = Server::start(&[]);
    // Every non-loopback address of this machine that can be found cheaply:
    // connecting to the port there must fail.
    if let Ok(socket) = std::net::UdpSocket::bind("0.0.0.0:0") {
        if socket.connect("192.0.2.1:9").is_ok() {
            if let Ok(local) = socket.local_addr() {
                if !local.ip().is_loopback() && !local.ip().is_unspecified() {
                    let there = SocketAddr::new(local.ip(), s.port);
                    assert!(
                        TcpStream::connect_timeout(&there, Duration::from_secs(2)).is_err(),
                        "the server answered on {there}"
                    );
                }
            }
        }
    }
}

#[test]
fn it_refuses_to_start_without_a_usable_token() {
    for token in ["", "too-short"] {
        let out = bin()
            .args(["mcp", "--http", "--port", &free_port().to_string()])
            .env(TOKEN_VAR, token)
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(3), "{token:?}");
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(err.contains(TOKEN_VAR), "{err}");
    }
}

#[test]
fn bad_flags_fail_before_any_token_is_read() {
    // --http without a port, and port 0: usage errors, exit 2, whatever the
    // token.
    for args in [
        vec!["mcp", "--http"],
        vec!["mcp", "--http", "--port", "0"],
        vec!["mcp", "--port", "8731"],
        vec!["mcp", "--allow-writes"],
        vec!["mcp", "--http", "--list-tools", "--port", "8731"],
        vec!["mcp", "--http", "--new-token", "--port", "8731"],
    ] {
        let out = bin().args(&args).output().unwrap();
        assert_eq!(out.status.code(), Some(2), "{args:?}");
    }
}
