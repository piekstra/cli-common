//! The loopback redirect a native app receives its code on (RFC 8252 §7.3).

use std::io::{Read, Write};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use pk_cli_core::CliError;
use pk_cli_secrets::Secret;

use super::authorize::code_from_callback;

/// The redirect's path: `http://127.0.0.1:<port>/callback`.
pub const CALLBACK_PATH: &str = "/callback";

/// How long [`LoopbackListener::wait_for_code`] waits by default for a person
/// to sign in, MFA included.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(300);

/// A request head larger than this is not a browser bringing a code back.
const MAX_HEAD: usize = 16 * 1024;
/// A browser opens speculative connections that send nothing; each is read on
/// its own thread, so one cannot hold up the request that carries the code.
const READ_TIMEOUT: Duration = Duration::from_secs(10);
const POLL: Duration = Duration::from_millis(25);

/// A listener on `127.0.0.1` for one sign-in's redirect. It answers only `GET
/// /callback` with a `Host` of this listener; anything else (a favicon, a
/// probe) gets a 404 or 400 and the wait goes on.
pub struct LoopbackListener {
    listener: TcpListener,
    port: u16,
    app: String,
}

impl std::fmt::Debug for LoopbackListener {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoopbackListener")
            .field("port", &self.port)
            .field("app", &self.app)
            .finish()
    }
}

impl LoopbackListener {
    /// Listen on `127.0.0.1` on a port the system picks. `app` names the CLI on
    /// the page the browser shows.
    pub fn bind(app: &str) -> Result<Self, CliError> {
        Self::bind_port(app, 0)
    }

    /// Listen on a fixed port, for a provider that registered one redirect with
    /// a port (RFC 8252 §7.3 says a server should accept any port; not all do).
    pub fn bind_port(app: &str, port: u16) -> Result<Self, CliError> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, port)).map_err(|e| {
            CliError::Other(format!(
                "could not listen on 127.0.0.1:{port} for the sign-in's answer: {e}"
            ))
        })?;
        listener
            .set_nonblocking(true)
            .map_err(|e| CliError::Other(format!("configuring the sign-in listener: {e}")))?;
        let port = listener
            .local_addr()
            .map_err(|e| CliError::Other(format!("reading the sign-in listener's port: {e}")))?
            .port();
        Ok(LoopbackListener {
            listener,
            port,
            app: app.to_string(),
        })
    }

    /// Listen on the first of `ports` that is free, for a provider that
    /// matches the redirect exactly against a few registered ports. An empty
    /// list is a port the system picks.
    pub fn bind_first(app: &str, ports: &[u16]) -> Result<Self, CliError> {
        let mut failures = Vec::new();
        for &port in ports {
            match Self::bind_port(app, port) {
                Ok(listener) => return Ok(listener),
                Err(e) => failures.push(e.to_string()),
            }
        }
        if ports.is_empty() {
            return Self::bind(app);
        }
        Err(CliError::Other(format!(
            "no port registered for the sign-in's redirect is free: {}",
            failures.join("; ")
        )))
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// `http://127.0.0.1:<port>/callback`: the IP literal, never `localhost`,
    /// which RFC 8252 §8.3 advises against (it may resolve elsewhere).
    pub fn redirect_uri(&self) -> String {
        format!("http://127.0.0.1:{}{CALLBACK_PATH}", self.port)
    }

    /// Wait for the browser to bring the code back, and answer it with a short
    /// page saying whether to return to the terminal. The code is checked with
    /// [`code_from_callback`]. Past `timeout` this is [`CliError::Auth`]: nobody
    /// finished signing in, and signing in again is the fix.
    pub fn wait_for_code(
        &self,
        expected_state: &str,
        expected_issuer: Option<&str>,
        timeout: Duration,
    ) -> Result<Secret, CliError> {
        let deadline = Instant::now() + timeout;
        let (sender, answers) = mpsc::channel::<Result<Secret, CliError>>();
        loop {
            match answers.try_recv() {
                Ok(answer) => return answer,
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => unreachable!("this loop holds a sender"),
            }
            match self.listener.accept() {
                Ok((stream, _)) => {
                    let sender = sender.clone();
                    let (app, port) = (self.app.clone(), self.port);
                    let (state, issuer) = (
                        expected_state.to_string(),
                        expected_issuer.map(str::to_string),
                    );
                    std::thread::spawn(move || {
                        if let Some(answer) = serve(stream, &app, port, &state, issuer.as_deref()) {
                            let _ = sender.send(answer);
                        }
                    });
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    if Instant::now() >= deadline {
                        return Err(CliError::Auth(format!(
                            "no sign-in answer reached {} within {} s. Sign in again",
                            self.redirect_uri(),
                            timeout.as_secs()
                        )));
                    }
                    std::thread::sleep(POLL);
                }
                Err(e) => {
                    return Err(CliError::Other(format!(
                        "accepting the sign-in's answer: {e}"
                    )))
                }
            }
        }
    }
}

/// Serve one connection. `Some` when it was the redirect.
fn serve(
    mut stream: TcpStream,
    app: &str,
    port: u16,
    expected_state: &str,
    expected_issuer: Option<&str>,
) -> Option<Result<Secret, CliError>> {
    if stream.set_nonblocking(false).is_err()
        || stream.set_read_timeout(Some(READ_TIMEOUT)).is_err()
    {
        return None;
    }
    let Some(head) = read_head(&mut stream) else {
        respond(
            &mut stream,
            400,
            "Bad Request",
            &page(app, Outcome::NotHere),
        );
        return None;
    };
    let mut lines = head.split("\r\n");
    let mut request_line = lines.next().unwrap_or_default().split(' ');
    let (method, target) = (
        request_line.next().unwrap_or_default(),
        request_line.next().unwrap_or_default(),
    );
    let host = lines
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.trim().eq_ignore_ascii_case("host"))
        .map(|(_, value)| value.trim().to_ascii_lowercase());
    // A page elsewhere that rebinds a name to 127.0.0.1 sends its own Host.
    let ours = [format!("127.0.0.1:{port}"), format!("localhost:{port}")];
    if !host.is_some_and(|h| ours.contains(&h)) {
        respond(
            &mut stream,
            400,
            "Bad Request",
            &page(app, Outcome::NotHere),
        );
        return None;
    }
    if method != "GET" {
        respond(
            &mut stream,
            405,
            "Method Not Allowed",
            &page(app, Outcome::NotHere),
        );
        return None;
    }
    if target.split(['?', '#']).next() != Some(CALLBACK_PATH) {
        respond(&mut stream, 404, "Not Found", &page(app, Outcome::NotHere));
        return None;
    }
    let answer = code_from_callback(target, expected_state, expected_issuer);
    let outcome = if answer.is_ok() {
        Outcome::SignedIn
    } else {
        Outcome::Failed
    };
    respond(&mut stream, 200, "OK", &page(app, outcome));
    Some(answer)
}

/// The request head up to the blank line, or `None` past [`MAX_HEAD`], on a
/// read error or timeout, or when it is not text.
fn read_head(stream: &mut TcpStream) -> Option<String> {
    let mut head = Vec::with_capacity(1024);
    let mut buf = [0u8; 1024];
    while !head.windows(4).any(|w| w == b"\r\n\r\n") {
        let n = stream.read(&mut buf).ok()?;
        if n == 0 || head.len() + n > MAX_HEAD {
            return None;
        }
        head.extend_from_slice(&buf[..n]);
    }
    String::from_utf8(head).ok()
}

enum Outcome {
    SignedIn,
    Failed,
    NotHere,
}

fn page(app: &str, outcome: Outcome) -> String {
    let app = html_escape(app);
    let message = match outcome {
        Outcome::SignedIn => "signed in. You can close this tab and return to the terminal.",
        Outcome::Failed => "the sign-in did not complete. Return to the terminal for details.",
        Outcome::NotHere => "nothing here.",
    };
    format!("<!doctype html><meta charset=utf-8><title>{app}</title><p>{app}: {message}</p>")
}

/// The page holds nothing from the request, and the headers keep the address
/// (which carried the code) out of any referrer, cache or script.
fn respond(stream: &mut TcpStream, status: u16, reason: &str, body: &str) {
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\n\
         Content-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\n\
         Cache-Control: no-store\r\n\
         Referrer-Policy: no-referrer\r\n\
         Content-Security-Policy: default-src 'none'\r\n\
         X-Content-Type-Options: nosniff\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
}

fn html_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Send `request` (with `{port}` filled in) and return the response text.
    fn send(port: u16, request: &str) -> String {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream
            .write_all(request.replace("{port}", &port.to_string()).as_bytes())
            .unwrap();
        let mut out = String::new();
        let _ = stream.read_to_string(&mut out);
        out
    }

    fn get(path: &str) -> String {
        format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{{port}}\r\nAccept: text/html\r\n\r\n")
    }

    #[test]
    fn the_redirect_uri_is_the_ip_literal_with_the_bound_port() {
        let listener = LoopbackListener::bind("pk-test").unwrap();
        assert_ne!(listener.port(), 0);
        assert_eq!(
            listener.redirect_uri(),
            format!("http://127.0.0.1:{}/callback", listener.port())
        );
    }

    #[test]
    fn the_code_arrives_after_strays_and_an_idle_connection() {
        let listener = LoopbackListener::bind("pk-test").unwrap();
        let port = listener.port();
        let browser = std::thread::spawn(move || {
            // A speculative connection that never sends a byte.
            let _idle = TcpStream::connect(("127.0.0.1", port)).unwrap();
            let favicon = send(port, &get("/favicon.ico"));
            let post = send(
                port,
                "POST /callback?code=x&state=S HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n",
            );
            let foreign = send(
                port,
                "GET /callback?code=x&state=S HTTP/1.1\r\nHost: rebound.example.test\r\n\r\n",
            );
            let answer = send(port, &get("/callback?code=the-code&state=S"));
            (favicon, post, foreign, answer)
        });
        let code = listener
            .wait_for_code("S", None, Duration::from_secs(20))
            .unwrap();
        assert_eq!(code.expose(), "the-code");
        let (favicon, post, foreign, answer) = browser.join().unwrap();
        assert!(favicon.starts_with("HTTP/1.1 404"), "{favicon}");
        assert!(post.starts_with("HTTP/1.1 405"), "{post}");
        assert!(foreign.starts_with("HTTP/1.1 400"), "{foreign}");
        assert!(answer.starts_with("HTTP/1.1 200"), "{answer}");
        assert!(answer.contains("pk-test: signed in"), "{answer}");
        for header in [
            "Cache-Control: no-store",
            "Referrer-Policy: no-referrer",
            "Content-Security-Policy: default-src 'none'",
        ] {
            assert!(answer.contains(header), "{header}: {answer}");
        }
        assert!(!answer.contains("the-code"), "{answer}");
    }

    #[test]
    fn a_wrong_state_ends_the_wait_with_an_auth_error_and_a_failure_page() {
        let listener = LoopbackListener::bind("pk-test").unwrap();
        let port = listener.port();
        let browser = std::thread::spawn(move || send(port, &get("/callback?code=c&state=NOPE")));
        let err = listener
            .wait_for_code("S", None, Duration::from_secs(20))
            .unwrap_err();
        assert_eq!(err.exit_code(), 3);
        assert!(browser.join().unwrap().contains("did not complete"));
    }

    #[test]
    fn nobody_signing_in_times_out_as_auth() {
        let listener = LoopbackListener::bind("pk-test").unwrap();
        let started = Instant::now();
        let err = listener
            .wait_for_code("S", None, Duration::from_millis(100))
            .unwrap_err();
        assert_eq!(err.exit_code(), 3);
        assert!(err.to_string().contains("within 0 s"), "{err}");
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn an_oversized_head_is_refused_and_the_wait_goes_on() {
        let listener = LoopbackListener::bind("pk-test").unwrap();
        let port = listener.port();
        let browser = std::thread::spawn(move || {
            let huge = format!(
                "GET /callback?pad={} HTTP/1.1\r\nHost: 127.0.0.1:{{port}}\r\n\r\n",
                "a".repeat(MAX_HEAD)
            );
            let refused = send(port, &huge);
            let answer = send(port, &get("/callback?code=c2&state=S"));
            (refused, answer)
        });
        let code = listener
            .wait_for_code("S", None, Duration::from_secs(20))
            .unwrap();
        assert_eq!(code.expose(), "c2");
        let (refused, _) = browser.join().unwrap();
        assert!(
            refused.is_empty() || refused.starts_with("HTTP/1.1 400"),
            "{refused}"
        );
    }

    #[test]
    fn bind_first_moves_past_a_busy_port() {
        let busy = LoopbackListener::bind("pk-test").unwrap();
        let free = {
            let probe = LoopbackListener::bind("pk-test").unwrap();
            probe.port()
        };
        let listener = LoopbackListener::bind_first("pk-test", &[busy.port(), free]).unwrap();
        assert_eq!(listener.port(), free);
        let err = LoopbackListener::bind_first("pk-test", &[busy.port()]).unwrap_err();
        assert!(err.to_string().contains("no port registered"), "{err}");
        assert_ne!(
            LoopbackListener::bind_first("pk-test", &[]).unwrap().port(),
            0
        );
    }

    #[test]
    fn the_app_name_is_escaped_on_the_page() {
        assert!(page("<b>", Outcome::SignedIn).contains("&lt;b&gt;"));
    }
}
