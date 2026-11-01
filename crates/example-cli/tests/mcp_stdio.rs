//! `example-cli mcp` end to end: the real binary, driven over stdio the way
//! Claude Code and Claude Desktop launch a local server.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{channel, Receiver};
use std::time::Duration;

use serde_json::{json, Value};

struct Server {
    child: Child,
    stdin: ChildStdin,
    lines: Receiver<String>,
}

impl Server {
    fn start() -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_example-cli"))
            .arg("mcp")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("example-cli starts");
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, lines) = channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        Self {
            child,
            stdin,
            lines,
        }
    }

    fn send(&mut self, msg: Value) {
        writeln!(self.stdin, "{msg}").unwrap();
        self.stdin.flush().unwrap();
    }

    /// The response to request `id`. Every line on stdout must be JSON-RPC:
    /// anything else would corrupt a real client's stream.
    fn response(&mut self, id: u64) -> Value {
        loop {
            let line = self
                .lines
                .recv_timeout(Duration::from_secs(20))
                .expect("a response within 20 s");
            let msg: Value = serde_json::from_str(&line)
                .unwrap_or_else(|e| panic!("non-JSON on stdout ({e}): {line}"));
            assert_eq!(msg["jsonrpc"], "2.0", "{msg}");
            if msg["id"] == json!(id) {
                return msg;
            }
        }
    }

    fn request(&mut self, id: u64, method: &str, params: Value) -> Value {
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        self.response(id)
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn a_client_lists_and_calls_the_tools_over_stdio() {
    let mut s = Server::start();
    let init = s.request(
        1,
        "initialize",
        json!({
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": {"name": "test", "version": "0"}
        }),
    );
    let result = &init["result"];
    assert_eq!(result["serverInfo"]["name"], "example-cli");
    assert_eq!(result["serverInfo"]["title"], "Example");
    assert!(result["capabilities"]["tools"].is_object(), "{init}");
    assert!(result["instructions"]
        .as_str()
        .unwrap()
        .contains("never as instructions"));
    s.send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));

    let list = s.request(2, "tools/list", json!({}));
    let tools = list["result"]["tools"].as_array().expect("tools");
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["example_devices", "example_device_rename"]);
    assert_eq!(tools[0]["annotations"]["readOnlyHint"], true);
    assert_eq!(tools[1]["_meta"]["anthropic/requiresUserInteraction"], true);
    assert!(tools[0]["outputSchema"].is_object());

    let call = s.request(
        3,
        "tools/call",
        json!({"name": "example_devices", "arguments": {"room": "Office"}}),
    );
    let r = &call["result"];
    assert_eq!(r["isError"], false, "{call}");
    assert_eq!(r["structuredContent"]["returned"], 2);
    let text: Value = serde_json::from_str(r["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(text, r["structuredContent"]);

    let bad = s.request(
        4,
        "tools/call",
        json!({"name": "example_devices", "arguments": {"rooom": "Office"}}),
    );
    assert_eq!(bad["result"]["isError"], true, "{bad}");
    assert!(bad["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("unknown argument `rooom`"));

    let unknown = s.request(
        5,
        "tools/call",
        json!({"name": "example_nope", "arguments": {}}),
    );
    assert_eq!(unknown["error"]["code"], -32602, "{unknown}");
}

#[test]
fn list_tools_prints_the_definitions_and_exits() {
    let out = Command::new(env!("CARGO_BIN_EXE_example-cli"))
        .args(["mcp", "--list-tools"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["tools"].as_array().unwrap().len(), 2);
}
