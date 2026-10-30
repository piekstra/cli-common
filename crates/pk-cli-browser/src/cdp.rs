//! A minimal blocking Chrome DevTools Protocol client: one websocket to the
//! browser target, requests correlated by id, page commands sent through
//! flattened target sessions (`sessionId`).
//!
//! Connections are short-lived (one per poll), so a stuck page or a closed
//! window costs one poll, never the whole wait. The one exception is the
//! watch (`watch.rs`), which stays open between polls to read the network
//! events of the pages it is attached to, and so keeps the events a call
//! reads past instead of dropping them.

use std::collections::VecDeque;
use std::io;
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tungstenite::handshake::HandshakeError;
use tungstenite::{Message, WebSocket};

/// How long one socket read blocks before the caller checks its deadline
/// and the cancel flag again.
const READ_SLICE: Duration = Duration::from_millis(250);
/// Bound on the TCP connect and the websocket handshake to the debug port.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
/// Events a connection that keeps them holds before dropping the newest.
const MAX_QUEUED_EVENTS: usize = 10_000;

/// What a CDP call can fail with. None of these carry a page value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CdpError {
    /// No reply within the call's deadline.
    Timeout(String),
    /// The cancel flag was raised while waiting.
    Canceled,
    /// The socket closed or failed.
    Transport(String),
    /// Chrome answered with a protocol error.
    Protocol { code: i64, message: String },
}

impl std::fmt::Display for CdpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CdpError::Timeout(method) => write!(f, "{method} got no answer in time"),
            CdpError::Canceled => f.write_str("canceled"),
            CdpError::Transport(e) => write!(f, "debug port connection failed: {e}"),
            CdpError::Protocol { code, message } => write!(f, "{message} ({code})"),
        }
    }
}

/// The message channel a [`Cdp`] runs over: the browser websocket in
/// production, an in-memory fake browser in tests.
pub(crate) trait Transport {
    fn send(&mut self, text: &str) -> Result<(), String>;
    /// The next text message; `Ok(None)` when none arrived within one read
    /// slice.
    fn recv(&mut self) -> Result<Option<String>, String>;
}

/// The browser-target websocket on Chrome's loopback debug port.
pub(crate) struct WsTransport {
    socket: WebSocket<TcpStream>,
}

impl WsTransport {
    /// Connect to `ws://127.0.0.1:<port><path>`, with the connect and the
    /// handshake bounded so an unresponsive port fails one poll, not the
    /// whole wait.
    pub fn connect(port: u16, path: &str) -> Result<Self, String> {
        let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
        let stream = TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT)
            .map_err(|e| format!("connecting to 127.0.0.1:{port}: {e}"))?;
        stream
            .set_read_timeout(Some(READ_SLICE))
            .and_then(|()| stream.set_write_timeout(Some(CONNECT_TIMEOUT)))
            .map_err(|e| format!("configuring the debug port socket: {e}"))?;
        let url = format!("ws://127.0.0.1:{port}{path}");
        let deadline = Instant::now() + CONNECT_TIMEOUT;
        let mut attempt = tungstenite::client(url.as_str(), stream);
        loop {
            match attempt {
                Ok((socket, _response)) => return Ok(WsTransport { socket }),
                Err(HandshakeError::Interrupted(mid)) if Instant::now() < deadline => {
                    attempt = mid.handshake();
                }
                Err(HandshakeError::Interrupted(_)) => {
                    return Err("the debug port did not finish the websocket handshake".into())
                }
                Err(HandshakeError::Failure(e)) => return Err(format!("websocket handshake: {e}")),
            }
        }
    }
}

impl Transport for WsTransport {
    fn send(&mut self, text: &str) -> Result<(), String> {
        self.socket
            .send(Message::text(text))
            .map_err(|e| e.to_string())
    }

    fn recv(&mut self) -> Result<Option<String>, String> {
        loop {
            match self.socket.read() {
                Ok(Message::Text(text)) => return Ok(Some(text.as_str().to_owned())),
                Ok(Message::Close(_)) => return Err("Chrome closed the connection".into()),
                // Pings are answered by tungstenite; binary frames are not CDP.
                Ok(_) => continue,
                Err(e) if is_read_timeout(&e) => return Ok(None),
                Err(e) => return Err(e.to_string()),
            }
        }
    }
}

impl Drop for WsTransport {
    fn drop(&mut self) {
        let _ = self.socket.close(None);
        let _ = self.socket.flush();
    }
}

/// Whether a tungstenite error is only the socket's read timeout (keep
/// waiting) rather than a closed or broken connection.
pub(crate) fn is_read_timeout(err: &tungstenite::Error) -> bool {
    matches!(
        err,
        tungstenite::Error::Io(e)
            if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut)
    )
}

/// A CDP client over one [`Transport`].
pub(crate) struct Cdp<T: Transport> {
    transport: T,
    next_id: u64,
    call_timeout: Duration,
    canceled: fn() -> bool,
    /// Events read past while a call waited for its reply, when the
    /// connection keeps them ([`Cdp::keeping_events`]).
    events: Option<VecDeque<Value>>,
}

impl<T: Transport> Cdp<T> {
    pub fn new(transport: T, call_timeout: Duration, canceled: fn() -> bool) -> Self {
        Cdp {
            transport,
            next_id: 0,
            call_timeout,
            canceled,
            events: None,
        }
    }

    /// Keep the events a call reads past, for [`Cdp::next_event`], instead
    /// of dropping them.
    pub fn keeping_events(mut self) -> Self {
        self.events = Some(VecDeque::new());
        self
    }

    /// The next event: one a call read past, else the next message on the
    /// connection. `Ok(None)` when nothing arrived within one read slice,
    /// or when what arrived is not an event (a late reply to a call that
    /// timed out).
    pub fn next_event(&mut self) -> Result<Option<Value>, CdpError> {
        if let Some(event) = self.events.as_mut().and_then(VecDeque::pop_front) {
            return Ok(Some(event));
        }
        if (self.canceled)() {
            return Err(CdpError::Canceled);
        }
        Ok(self
            .transport
            .recv()
            .map_err(CdpError::Transport)?
            .and_then(|text| parse_event(&text)))
    }

    #[cfg(test)]
    pub fn transport(&self) -> &T {
        &self.transport
    }

    /// Send `method` and read past events and other replies to its result.
    pub fn call(
        &mut self,
        method: &str,
        params: Value,
        session: Option<&str>,
    ) -> Result<Value, CdpError> {
        self.next_id += 1;
        let id = self.next_id;
        self.transport
            .send(&request(id, method, params, session))
            .map_err(CdpError::Transport)?;
        let deadline = Instant::now() + self.call_timeout;
        loop {
            if (self.canceled)() {
                return Err(CdpError::Canceled);
            }
            if let Some(text) = self.transport.recv().map_err(CdpError::Transport)? {
                match parse_reply(&text, id) {
                    Reply::Result(result) => return Ok(result),
                    Reply::Error { code, message } => {
                        return Err(CdpError::Protocol { code, message })
                    }
                    Reply::Other => {
                        if let Some(queue) = self.events.as_mut() {
                            if queue.len() < MAX_QUEUED_EVENTS {
                                queue.extend(parse_event(&text));
                            }
                        }
                    }
                }
            }
            if Instant::now() >= deadline {
                return Err(CdpError::Timeout(method.to_string()));
            }
        }
    }
}

/// One CDP request as sent on the wire.
pub(crate) fn request(id: u64, method: &str, params: Value, session: Option<&str>) -> String {
    let mut msg = json!({ "id": id, "method": method });
    if !params.is_null() {
        msg["params"] = params;
    }
    if let Some(session) = session {
        msg["sessionId"] = Value::String(session.to_string());
    }
    msg.to_string()
}

/// What one incoming message means to the call waiting on `id`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Reply {
    /// Our result (`{}` when Chrome sent none).
    Result(Value),
    /// Our call failed.
    Error { code: i64, message: String },
    /// An event, another call's reply, or something unparseable: skip it.
    Other,
}

/// An event (a message with a `method` and no `id`), parsed; `None` for a
/// reply or anything unparseable.
pub(crate) fn parse_event(text: &str) -> Option<Value> {
    serde_json::from_str::<Value>(text)
        .ok()
        .filter(|v| v.get("method").is_some() && v.get("id").is_none())
}

pub(crate) fn parse_reply(text: &str, id: u64) -> Reply {
    let Ok(v) = serde_json::from_str::<Value>(text) else {
        return Reply::Other;
    };
    if v.get("id").and_then(Value::as_u64) != Some(id) {
        return Reply::Other;
    }
    if let Some(err) = v.get("error") {
        return Reply::Error {
            code: err.get("code").and_then(Value::as_i64).unwrap_or(0),
            message: err
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("unknown protocol error")
                .to_string(),
        };
    }
    Reply::Result(v.get("result").cloned().unwrap_or_else(|| json!({})))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    #[test]
    fn requests_carry_id_method_and_optional_params_and_session() {
        let v: Value =
            serde_json::from_str(&request(7, "Target.getTargets", Value::Null, None)).unwrap();
        assert_eq!(v, json!({"id": 7, "method": "Target.getTargets"}));
        let v: Value = serde_json::from_str(&request(
            8,
            "Runtime.evaluate",
            json!({"expression": "1"}),
            Some("S1"),
        ))
        .unwrap();
        assert_eq!(
            v,
            json!({"id": 8, "method": "Runtime.evaluate",
                   "params": {"expression": "1"}, "sessionId": "S1"})
        );
    }

    #[test]
    fn replies_are_matched_to_their_id() {
        let cases: &[(&str, Reply)] = &[
            (
                r#"{"id":3,"result":{"a":1}}"#,
                Reply::Result(json!({"a": 1})),
            ),
            (r#"{"id":3}"#, Reply::Result(json!({}))),
            (
                r#"{"id":3,"error":{"code":-32000,"message":"No target with given id found"}}"#,
                Reply::Error {
                    code: -32000,
                    message: "No target with given id found".into(),
                },
            ),
            (
                r#"{"id":3,"error":{}}"#,
                Reply::Error {
                    code: 0,
                    message: "unknown protocol error".into(),
                },
            ),
            // Another call's reply, an event, a session event, junk.
            (r#"{"id":4,"result":{}}"#, Reply::Other),
            (
                r#"{"method":"Target.targetCreated","params":{}}"#,
                Reply::Other,
            ),
            (
                r#"{"method":"Runtime.executionContextCreated","sessionId":"S"}"#,
                Reply::Other,
            ),
            (r#"{"id":"3","result":{}}"#, Reply::Other),
            ("not json", Reply::Other),
            ("", Reply::Other),
        ];
        for (text, want) in cases {
            assert_eq!(&parse_reply(text, 3), want, "{text}");
        }
    }

    #[test]
    fn read_timeouts_are_told_apart_from_broken_sockets() {
        let io = |kind| tungstenite::Error::Io(io::Error::from(kind));
        assert!(is_read_timeout(&io(io::ErrorKind::WouldBlock)));
        assert!(is_read_timeout(&io(io::ErrorKind::TimedOut)));
        assert!(!is_read_timeout(&io(io::ErrorKind::ConnectionReset)));
        assert!(!is_read_timeout(&io(io::ErrorKind::UnexpectedEof)));
        assert!(!is_read_timeout(&tungstenite::Error::ConnectionClosed));
    }

    /// Replays scripted incoming messages; records what was sent.
    struct Script {
        incoming: VecDeque<Result<Option<String>, String>>,
        sent: Vec<String>,
    }

    impl Transport for Script {
        fn send(&mut self, text: &str) -> Result<(), String> {
            self.sent.push(text.to_string());
            Ok(())
        }
        fn recv(&mut self) -> Result<Option<String>, String> {
            self.incoming.pop_front().unwrap_or(Ok(None))
        }
    }

    fn script(incoming: Vec<Result<Option<String>, String>>) -> Script {
        Script {
            incoming: incoming.into(),
            sent: Vec::new(),
        }
    }

    fn never() -> bool {
        false
    }

    fn always() -> bool {
        true
    }

    #[test]
    fn a_call_reads_past_events_quiet_slices_and_other_replies() {
        let mut cdp = Cdp::new(
            script(vec![
                Ok(Some(
                    r#"{"method":"Target.targetCreated","params":{}}"#.into(),
                )),
                Ok(None),
                Ok(Some(r#"{"id":99,"result":{}}"#.into())),
                Ok(Some(r#"{"id":1,"result":{"ok":true}}"#.into())),
            ]),
            Duration::from_secs(5),
            never,
        );
        let result = cdp.call("Browser.getVersion", Value::Null, None).unwrap();
        assert_eq!(result, json!({"ok": true}));
        assert_eq!(cdp.transport.sent.len(), 1);
        // The next call uses the next id.
        cdp.transport.incoming = vec![Ok(Some(r#"{"id":2,"result":{}}"#.into()))].into();
        cdp.call("Target.getTargets", Value::Null, None).unwrap();
        assert!(cdp.transport.sent[1].contains(r#""id":2"#));
    }

    #[test]
    fn a_call_reports_protocol_errors_timeouts_closes_and_cancels() {
        let mut cdp = Cdp::new(
            script(vec![Ok(Some(
                r#"{"id":1,"error":{"code":-32000,"message":"No session"}}"#.into(),
            ))]),
            Duration::from_secs(5),
            never,
        );
        assert_eq!(
            cdp.call("Runtime.evaluate", Value::Null, Some("S")),
            Err(CdpError::Protocol {
                code: -32000,
                message: "No session".into()
            })
        );

        let mut cdp = Cdp::new(script(vec![]), Duration::ZERO, never);
        assert_eq!(
            cdp.call("Target.getTargets", Value::Null, None),
            Err(CdpError::Timeout("Target.getTargets".into()))
        );

        let mut cdp = Cdp::new(
            script(vec![Err("Chrome closed the connection".into())]),
            Duration::from_secs(5),
            never,
        );
        assert_eq!(
            cdp.call("Target.getTargets", Value::Null, None),
            Err(CdpError::Transport("Chrome closed the connection".into()))
        );

        let mut cdp = Cdp::new(script(vec![]), Duration::from_secs(5), always);
        assert_eq!(
            cdp.call("Target.getTargets", Value::Null, None),
            Err(CdpError::Canceled)
        );
    }

    #[test]
    fn only_events_are_parsed_as_events() {
        assert_eq!(
            parse_event(r#"{"method":"Network.requestWillBeSent","sessionId":"S","params":{}}"#),
            Some(json!({"method": "Network.requestWillBeSent", "sessionId": "S", "params": {}}))
        );
        for text in [
            r#"{"id":1,"result":{}}"#,
            r#"{"id":1,"method":"x"}"#,
            "{}",
            "junk",
            "",
        ] {
            assert_eq!(parse_event(text), None, "{text}");
        }
    }

    #[test]
    fn a_connection_that_keeps_events_hands_them_over_in_order() {
        let incoming = vec![
            Ok(Some(r#"{"method":"A"}"#.into())),
            Ok(Some(r#"{"id":5,"result":{}}"#.into())),
            Ok(Some(r#"{"method":"B"}"#.into())),
            Ok(Some(r#"{"id":1,"result":{}}"#.into())),
            Ok(Some(r#"{"method":"C"}"#.into())),
            Ok(Some(r#"{"id":9,"result":{}}"#.into())),
        ];
        let mut cdp =
            Cdp::new(script(incoming.clone()), Duration::from_secs(5), never).keeping_events();
        cdp.call("Target.getTargets", Value::Null, None).unwrap();
        let method = |e: Option<Value>| e.map(|e| e["method"].as_str().unwrap().to_string());
        // A and B were read past by the call; C is read from the wire; a
        // stray reply reads as nothing; then the quiet slice.
        assert_eq!(method(cdp.next_event().unwrap()).as_deref(), Some("A"));
        assert_eq!(method(cdp.next_event().unwrap()).as_deref(), Some("B"));
        assert_eq!(method(cdp.next_event().unwrap()).as_deref(), Some("C"));
        assert_eq!(cdp.next_event().unwrap(), None);
        assert_eq!(cdp.next_event().unwrap(), None);

        // A connection that does not keep them drops them, as before.
        let mut cdp = Cdp::new(script(incoming), Duration::from_secs(5), never);
        cdp.call("Target.getTargets", Value::Null, None).unwrap();
        assert_eq!(method(cdp.next_event().unwrap()).as_deref(), Some("C"));

        // A cancel is noticed before reading.
        let mut cdp = Cdp::new(script(vec![]), Duration::from_secs(5), always).keeping_events();
        assert_eq!(cdp.next_event(), Err(CdpError::Canceled));
    }

    /// The real websocket path against a loopback server standing in for
    /// Chrome's debug port: the handshake, a reply held back past one read
    /// slice (the read-timeout path), and an event ahead of the reply.
    #[test]
    fn the_websocket_transport_talks_to_a_loopback_debug_port() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut ws = tungstenite::accept(stream).unwrap();
            let request = match ws.read().unwrap() {
                Message::Text(t) => t.as_str().to_owned(),
                other => panic!("unexpected {other:?}"),
            };
            let request: Value = serde_json::from_str(&request).unwrap();
            assert_eq!(request["method"], "Browser.getVersion");
            std::thread::sleep(READ_SLICE + Duration::from_millis(200));
            ws.send(Message::text(r#"{"method":"Target.targetCreated"}"#))
                .unwrap();
            let reply = json!({"id": request["id"], "result": {"product": "Chrome/0"}});
            ws.send(Message::text(reply.to_string())).unwrap();
            // Wait for the client's close so the reply is not cut off.
            while ws.read().is_ok() {}
        });
        let transport = WsTransport::connect(port, "/devtools/browser/test").unwrap();
        let mut cdp = Cdp::new(transport, Duration::from_secs(5), never);
        let result = cdp.call("Browser.getVersion", Value::Null, None).unwrap();
        assert_eq!(result["product"], "Chrome/0");
        drop(cdp);
        server.join().unwrap();
    }

    #[test]
    fn connecting_to_a_closed_port_fails_fast() {
        // Bind then drop, so the port is (almost certainly) closed.
        let port = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let started = Instant::now();
        assert!(WsTransport::connect(port, "/devtools/browser/x").is_err());
        assert!(started.elapsed() < CONNECT_TIMEOUT + Duration::from_secs(1));
    }
}
