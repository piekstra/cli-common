//! A fake Google Drive on 127.0.0.1, for tests (feature `fake-google`).
//!
//! It serves the slice of the Drive API v3 and Google's OAuth endpoints the
//! Google backend uses, over real HTTP, holding synthetic files in memory:
//! `files.list` (the query forms the backend sends: `name`, `in parents`,
//! `trashed`, `mimeType`, with `and`, `or` and parentheses; paged), metadata,
//! `alt=media` with `Range`, folder and metadata creates, `PATCH` (rename,
//! move, trash), multipart, media and resumable uploads, `about`, the token
//! endpoint (authorization code with PKCE S256, and refresh), revoke, and
//! an authorization endpoint that approves at once.
//!
//! Every Drive request needs the current access token. Faults can be queued
//! ([`FakeDrive::fail_next`]): a status for the next matching requests,
//! answered before or after the request takes effect, which is how the
//! retry and reconcile paths are tested.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;

use md5::{Digest, Md5};
use pk_cli_auth::oauth::s256_challenge;
use serde_json::{json, Value};

use super::endpoints::Endpoints;
use super::files::{hex, FOLDER_MIME};

/// The id of the fake's store root folder.
pub const ROOT_ID: &str = "fakeRootFolder";
/// The id of the account's My Drive (`root` in a request).
pub const MY_DRIVE: &str = "fakeMyDrive";
/// The account the fake signs in.
pub const ACCOUNT: &str = "person@example.test";
/// The client secret the fake's token endpoint accepts: the one in the
/// crate's `tests/fixtures/google/desktop-client.json`.
pub const CLIENT_SECRET: &str = "GOCSPX-EXAMPLE-not-a-real-secret";
/// The synthetic Desktop client JSON the fake accepts.
pub const CLIENT_JSON: &str = include_str!("../../tests/fixtures/google/desktop-client.json");

#[derive(Debug, Clone)]
struct FakeFile {
    id: String,
    name: String,
    mime: String,
    parents: Vec<String>,
    content: Vec<u8>,
    modified: i64,
    trashed: bool,
    /// A shortcut's target: its id and mime type.
    shortcut: Option<(String, String)>,
}

impl FakeFile {
    fn is_blob(&self) -> bool {
        !self.mime.starts_with("application/vnd.google-apps.")
    }

    fn to_json(&self) -> Value {
        let mut v = json!({
            "id": self.id,
            "name": self.name,
            "mimeType": self.mime,
            "parents": self.parents,
            "modifiedTime": pk_cli_core::dates::fmt_rfc3339(self.modified).replace('Z', ".000Z"),
        });
        if self.is_blob() {
            v["size"] = json!(self.content.len().to_string());
            v["md5Checksum"] = json!(hex(&Md5::digest(&self.content)));
        }
        if let Some((id, mime)) = &self.shortcut {
            v["shortcutDetails"] = json!({"targetId": id, "targetMimeType": mime});
        }
        v
    }
}

/// When a queued fault answers: before the request does anything, or after
/// it took effect (a timeout or a 5xx that hides a success).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum When {
    Before,
    After,
}

#[derive(Debug, Clone)]
struct Fault {
    matcher: String,
    status: u16,
    reason: String,
    left: usize,
    when: When,
    retry_after: Option<u64>,
    /// A file another writer adds as the fault answers: (parent, name, bytes).
    adds: Option<(String, String, Vec<u8>)>,
}

struct Session {
    target: Option<String>,
    meta: Value,
    total: u64,
    data: Vec<u8>,
}

struct Pending {
    challenge: String,
    redirect: String,
    scope: String,
}

struct State {
    files: BTreeMap<String, FakeFile>,
    next: u64,
    clock: i64,
    access: String,
    refresh: String,
    granted: Option<String>,
    email: String,
    limit: Option<u64>,
    faults: VecDeque<Fault>,
    log: Vec<String>,
    sessions: HashMap<String, Session>,
    codes: HashMap<String, Pending>,
    page_size: usize,
    revoked: Vec<String>,
    foreign_sessions: bool,
    corrupt_md5: usize,
    /// Files whose next download comes with one byte changed.
    damaged: std::collections::HashSet<String>,
    /// How long each Drive request waits before it is answered.
    latency: std::time::Duration,
    in_flight: usize,
    peak_in_flight: usize,
}

impl State {
    fn id(&mut self, prefix: &str) -> String {
        self.next += 1;
        format!("{prefix}{:06}", self.next)
    }

    fn tick(&mut self) -> i64 {
        self.clock += 1;
        self.clock
    }
}

/// The fake server. Stops when dropped.
pub struct FakeDrive {
    addr: SocketAddr,
    state: Arc<Mutex<State>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl FakeDrive {
    /// A fake with an empty root folder ([`ROOT_ID`]) in [`ACCOUNT`]'s Drive.
    pub fn start() -> FakeDrive {
        let listener = TcpListener::bind("127.0.0.1:0").expect("binding the fake Drive");
        let addr = listener.local_addr().expect("the fake's address");
        let mut files = BTreeMap::new();
        files.insert(
            ROOT_ID.to_string(),
            FakeFile {
                id: ROOT_ID.into(),
                name: "Store".into(),
                mime: FOLDER_MIME.into(),
                parents: vec![MY_DRIVE.into()],
                content: Vec::new(),
                modified: 1_767_225_600,
                trashed: false,
                shortcut: None,
            },
        );
        let state = Arc::new(Mutex::new(State {
            files,
            next: 0,
            clock: 1_767_225_600,
            access: "fake-access-0".into(),
            refresh: "fake-refresh-0".into(),
            granted: None,
            email: ACCOUNT.into(),
            limit: Some(15 * 1024 * 1024 * 1024),
            faults: VecDeque::new(),
            log: Vec::new(),
            sessions: HashMap::new(),
            codes: HashMap::new(),
            page_size: 1000,
            revoked: Vec::new(),
            foreign_sessions: false,
            corrupt_md5: 0,
            damaged: std::collections::HashSet::new(),
            latency: std::time::Duration::ZERO,
            in_flight: 0,
            peak_in_flight: 0,
        }));
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let (state, stop) = (state.clone(), stop.clone());
            std::thread::spawn(move || {
                for conn in listener.incoming() {
                    if stop.load(Ordering::SeqCst) {
                        break;
                    }
                    let Ok(conn) = conn else { continue };
                    let state = state.clone();
                    std::thread::spawn(move || serve(conn, addr, &state));
                }
            })
        };
        FakeDrive {
            addr,
            state,
            stop,
            thread: Some(thread),
        }
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// `http://127.0.0.1:<port>`.
    pub fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    pub fn endpoints(&self) -> Endpoints {
        Endpoints::local(&self.base_url()).expect("a loopback base")
    }

    /// The access token the fake accepts now.
    pub fn access_token(&self) -> String {
        self.state().access.clone()
    }

    /// Make the current access token stale: Drive answers it with 401
    /// until a refresh grant issues a new one.
    pub fn expire_access_token(&self) {
        let mut s = self.state();
        let n = s.next + 1;
        s.next = n;
        s.access = format!("fake-access-stale-{n}");
    }

    /// Refuse the refresh token from now on (`invalid_grant`).
    pub fn revoke_refresh_token(&self) {
        self.state().refresh = "fake-refresh-revoked".into();
    }

    /// Grant this scope string at sign-in instead of what was asked.
    pub fn grant_scope(&self, scope: &str) {
        self.state().granted = Some(scope.into());
    }

    /// Sign in this account instead of [`ACCOUNT`].
    pub fn sign_in_as(&self, email: &str) {
        self.state().email = email.into();
    }

    /// Name upload sessions on another host (`localhost` for `127.0.0.1`),
    /// as a forged answer would.
    pub fn foreign_upload_sessions(&self) {
        self.state().foreign_sessions = true;
    }

    /// Answer listings at most `n` files a page.
    pub fn page_size(&self, n: usize) {
        self.state().page_size = n.max(1);
    }

    /// Answer the next `times` requests whose `METHOD /path?query` contains
    /// `matcher` with `status` (and `reason` in Google's error body).
    pub fn fail_next(&self, matcher: &str, status: u16, reason: &str, times: usize, when: When) {
        self.state().faults.push_back(Fault {
            matcher: matcher.into(),
            status,
            reason: reason.into(),
            left: times,
            when,
            retry_after: None,
            adds: None,
        });
    }

    /// A 429 with `Retry-After: secs` for the next `times` matching requests.
    pub fn throttle_next(&self, matcher: &str, secs: u64, times: usize) {
        self.state().faults.push_back(Fault {
            matcher: matcher.into(),
            status: 429,
            reason: "rateLimitExceeded".into(),
            left: times,
            when: When::Before,
            retry_after: Some(secs),
            adds: None,
        });
    }

    /// [`fail_next`](Self::fail_next) once, before the request takes effect,
    /// while another writer adds `name` under `parent`.
    pub fn fail_next_while_another_adds(
        &self,
        matcher: &str,
        status: u16,
        parent: &str,
        name: &str,
        content: &[u8],
    ) {
        self.state().faults.push_back(Fault {
            matcher: matcher.into(),
            status,
            reason: "backendError".into(),
            left: 1,
            when: When::Before,
            retry_after: None,
            adds: Some((parent.into(), name.into(), content.to_vec())),
        });
    }

    /// Report a wrong md5 in the next `times` answers that carry files (an
    /// upload's answer, a listing), as a damaged transfer would show.
    pub fn corrupt_md5_next(&self, times: usize) {
        self.state().corrupt_md5 += times;
    }

    /// Change one byte of the next download of the file `id` (a whole read,
    /// or its first piece), as a transfer damaged on the way would arrive.
    /// Its md5 in a listing stays right, so the reader's check catches it.
    pub fn damage_next_download(&self, id: &str) {
        self.state().damaged.insert(id.to_string());
    }

    /// Hold every request this long before answering it, so requests a
    /// client sends side by side overlap here.
    pub fn latency(&self, each: std::time::Duration) {
        self.state().latency = each;
    }

    /// The most requests the fake was answering at once since it started or
    /// since [`FakeDrive::clear_requests`].
    pub fn peak_in_flight(&self) -> usize {
        self.state().peak_in_flight
    }

    /// Every request so far, as `METHOD /path?query` (no headers or body).
    pub fn requests(&self) -> Vec<String> {
        self.state().log.clone()
    }

    /// Forget the requests logged so far, and the peak of those in flight.
    pub fn clear_requests(&self) {
        let mut s = self.state();
        s.log.clear();
        s.peak_in_flight = s.in_flight;
    }

    /// The tokens revoked through the revoke endpoint.
    pub fn revoked(&self) -> Vec<String> {
        self.state().revoked.clone()
    }

    /// Add a folder under `parent`; its id.
    pub fn add_folder(&self, parent: &str, name: &str) -> String {
        self.add(parent, name, FOLDER_MIME, Vec::new())
    }

    /// Add a file under `parent`; its id. Another of the same name may
    /// already be there: Drive allows it.
    pub fn add_file(&self, parent: &str, name: &str, content: &[u8]) -> String {
        self.add(parent, name, "application/octet-stream", content.to_vec())
    }

    /// Add one of Google's own documents (no bytes, no md5).
    pub fn add_google_doc(&self, parent: &str, name: &str) -> String {
        self.add(
            parent,
            name,
            "application/vnd.google-apps.document",
            Vec::new(),
        )
    }

    /// Add a shortcut under `parent` to the item `target`; its id. The
    /// target's mime type is taken now, as Drive records it.
    pub fn add_shortcut(&self, parent: &str, name: &str, target: &str) -> String {
        let mime = self
            .state()
            .files
            .get(target)
            .map(|f| f.mime.clone())
            .unwrap_or_else(|| "application/octet-stream".into());
        let id = self.add(
            parent,
            name,
            "application/vnd.google-apps.shortcut",
            Vec::new(),
        );
        if let Some(f) = self.state().files.get_mut(&id) {
            f.shortcut = Some((target.to_string(), mime));
        }
        id
    }

    /// Give the item `id` another parent, as a file made before Drive's
    /// 2020 single-parent change can have.
    pub fn add_parent(&self, id: &str, parent: &str) {
        if let Some(f) = self.state().files.get_mut(id) {
            f.parents.push(parent.into());
        }
    }

    /// Move the item `id` to the trash, as another app might.
    pub fn trash(&self, id: &str) {
        if let Some(f) = self.state().files.get_mut(id) {
            f.trashed = true;
        }
    }

    fn add(&self, parent: &str, name: &str, mime: &str, content: Vec<u8>) -> String {
        let mut s = self.state();
        let id = s.id("fake");
        let modified = s.tick();
        s.files.insert(
            id.clone(),
            FakeFile {
                id: id.clone(),
                name: name.into(),
                mime: mime.into(),
                parents: vec![parent.into()],
                content,
                modified,
                trashed: false,
                shortcut: None,
            },
        );
        id
    }

    /// The id at `path` under the root (`a/b/c.json`), not trashed; `None`
    /// when missing or when a part names two items.
    pub fn find(&self, path: &str) -> Option<String> {
        let s = self.state();
        let mut id = ROOT_ID.to_string();
        for part in path.split('/').filter(|p| !p.is_empty()) {
            let found: Vec<&FakeFile> = s
                .files
                .values()
                .filter(|f| !f.trashed && f.name == part && f.parents.contains(&id))
                .collect();
            if found.len() != 1 {
                return None;
            }
            id = found[0].id.clone();
        }
        Some(id)
    }

    /// The bytes of the file at `path` under the root.
    pub fn content(&self, path: &str) -> Option<Vec<u8>> {
        let id = self.find(path)?;
        self.state().files.get(&id).map(|f| f.content.clone())
    }

    /// How many untrashed items named `name` are directly in the folder at
    /// `dir` (`""` for the root).
    pub fn count(&self, dir: &str, name: &str) -> usize {
        let Some(parent) = self.find(dir) else {
            return 0;
        };
        self.state()
            .files
            .values()
            .filter(|f| !f.trashed && f.name == name && f.parents.contains(&parent))
            .count()
    }

    /// The ids of the untrashed items named `name` directly in the folder
    /// with id `parent` ([`MY_DRIVE`] for the top of My Drive).
    pub fn children(&self, parent: &str, name: &str) -> Vec<String> {
        self.state()
            .files
            .values()
            .filter(|f| !f.trashed && f.name == name && f.parents.iter().any(|p| p == parent))
            .map(|f| f.id.clone())
            .collect()
    }

    /// Whether the item with `id` is in the trash.
    pub fn is_trashed(&self, id: &str) -> bool {
        self.state().files.get(id).is_some_and(|f| f.trashed)
    }
}

impl Drop for FakeDrive {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(self.addr);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

// ── HTTP ────────────────────────────────────────────────────────────────

struct Request {
    method: String,
    path: String,
    query: Vec<(String, String)>,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Request {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    fn param(&self, name: &str) -> Option<&str> {
        self.query
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }
}

struct Response {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Response {
    fn json(status: u16, v: Value) -> Response {
        Response {
            status,
            headers: vec![("Content-Type".into(), "application/json".into())],
            body: v.to_string().into_bytes(),
        }
    }

    fn error(status: u16, reason: &str, message: &str) -> Response {
        Response::json(
            status,
            json!({"error": {"code": status, "message": message,
                "errors": [{"message": message, "domain": "global", "reason": reason}]}}),
        )
    }

    fn empty(status: u16) -> Response {
        Response {
            status,
            headers: Vec::new(),
            body: Vec::new(),
        }
    }

    fn header(mut self, name: &str, value: impl Into<String>) -> Response {
        self.headers.push((name.into(), value.into()));
        self
    }
}

fn serve(conn: TcpStream, addr: SocketAddr, state: &Mutex<State>) {
    let Ok(read_half) = conn.try_clone() else {
        return;
    };
    let mut reader = BufReader::new(read_half);
    let mut writer = conn;
    while let Some(req) = read_request(&mut reader) {
        let latency = {
            let mut s = state.lock().unwrap_or_else(|p| p.into_inner());
            s.in_flight += 1;
            s.peak_in_flight = s.peak_in_flight.max(s.in_flight);
            s.latency
        };
        if !latency.is_zero() {
            std::thread::sleep(latency);
        }
        let resp = handle(&req, addr, state);
        state.lock().unwrap_or_else(|p| p.into_inner()).in_flight -= 1;
        let reason = match resp.status {
            200 => "OK",
            204 => "No Content",
            206 => "Partial Content",
            302 => "Found",
            308 => "Resume Incomplete",
            _ => "Status",
        };
        let mut head = format!(
            "HTTP/1.1 {} {reason}\r\nContent-Length: {}\r\n",
            resp.status,
            resp.body.len()
        );
        for (n, v) in &resp.headers {
            head.push_str(&format!("{n}: {v}\r\n"));
        }
        head.push_str("\r\n");
        if writer.write_all(head.as_bytes()).is_err() || writer.write_all(&resp.body).is_err() {
            return;
        }
        let _ = writer.flush();
    }
}

fn read_request(reader: &mut BufReader<TcpStream>) -> Option<Request> {
    let mut line = String::new();
    if reader.read_line(&mut line).ok()? == 0 {
        return None;
    }
    let mut parts = line.split_whitespace();
    let method = parts.next()?.to_string();
    let target = parts.next()?.to_string();
    let mut headers: Vec<(String, String)> = Vec::new();
    loop {
        let mut h = String::new();
        if reader.read_line(&mut h).ok()? == 0 {
            return None;
        }
        let h = h.trim_end();
        if h.is_empty() {
            break;
        }
        if let Some((n, v)) = h.split_once(':') {
            headers.push((n.trim().to_string(), v.trim().to_string()));
        }
    }
    let get = |name: &str| {
        headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.clone())
    };
    let mut body = Vec::new();
    if get("transfer-encoding").is_some_and(|v| v.eq_ignore_ascii_case("chunked")) {
        loop {
            let mut size = String::new();
            reader.read_line(&mut size).ok()?;
            let n = usize::from_str_radix(size.trim(), 16).ok()?;
            let mut chunk = vec![0; n + 2];
            reader.read_exact(&mut chunk).ok()?;
            if n == 0 {
                break;
            }
            body.extend_from_slice(&chunk[..n]);
        }
    } else if let Some(len) = get("content-length").and_then(|v| v.parse::<usize>().ok()) {
        body = vec![0; len];
        reader.read_exact(&mut body).ok()?;
    }
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p.to_string(), parse_query(q)),
        None => (target, Vec::new()),
    };
    Some(Request {
        method,
        path,
        query,
        headers,
        body,
    })
}

fn parse_query(q: &str) -> Vec<(String, String)> {
    q.split('&')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            (decode(k), decode(v))
        })
        .collect()
}

fn decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < bytes.len() => {
                let byte = std::str::from_utf8(&bytes[i + 1..i + 3])
                    .ok()
                    .and_then(|h| u8::from_str_radix(h, 16).ok());
                match byte {
                    Some(b) => {
                        out.push(b);
                        i += 2;
                    }
                    None => out.push(b'%'),
                }
            }
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn handle(req: &Request, addr: SocketAddr, state: &Mutex<State>) -> Response {
    let line = {
        let q: Vec<String> = req.query.iter().map(|(k, v)| format!("{k}={v}")).collect();
        if q.is_empty() {
            format!("{} {}", req.method, req.path)
        } else {
            format!("{} {}?{}", req.method, req.path, q.join("&"))
        }
    };
    let mut s = state.lock().unwrap_or_else(|p| p.into_inner());
    s.log.push(line.clone());
    let fault = s
        .faults
        .iter_mut()
        .find(|f| f.left > 0 && line.contains(&f.matcher))
        .map(|f| {
            f.left -= 1;
            f.clone()
        });
    let failed = |f: &Fault| {
        let mut r = Response::error(f.status, &f.reason, "injected by the fake");
        if let Some(secs) = f.retry_after {
            r = r.header("Retry-After", secs.to_string());
        }
        r
    };
    if let Some((parent, name, content)) = fault.as_ref().and_then(|f| f.adds.clone()) {
        let id = s.id("fake");
        let modified = s.tick();
        s.files.insert(
            id.clone(),
            FakeFile {
                id,
                name,
                mime: "application/octet-stream".into(),
                parents: vec![parent],
                content,
                modified,
                trashed: false,
                shortcut: None,
            },
        );
    }
    if let Some(f) = fault.as_ref().filter(|f| f.when == When::Before) {
        return failed(f);
    }
    let mut resp = route(req, addr, &mut s);
    if s.corrupt_md5 > 0 && corrupt(&mut resp) {
        s.corrupt_md5 -= 1;
    }
    if req.method == "GET" && req.query.iter().any(|(k, v)| k == "alt" && v == "media") {
        let id = req.path.rsplit('/').next().unwrap_or_default().to_string();
        if !resp.body.is_empty() && s.damaged.remove(&id) {
            resp.body[0] ^= 0xff;
        }
    }
    match fault {
        Some(f) => failed(&f),
        None => resp,
    }
}

/// Flip the md5 in an answer's file (or its listing's files); whether there
/// was one.
fn corrupt(resp: &mut Response) -> bool {
    let Ok(mut v) = serde_json::from_slice::<Value>(&resp.body) else {
        return false;
    };
    let mut done = false;
    let mut flip = |f: &mut Value| {
        if let Some(m) = f.get_mut("md5Checksum") {
            *m = json!("00000000000000000000000000000000");
            done = true;
        }
    };
    if let Some(files) = v.get_mut("files").and_then(Value::as_array_mut) {
        files.iter_mut().for_each(&mut flip);
    } else {
        flip(&mut v);
    }
    if done {
        resp.body = v.to_string().into_bytes();
    }
    done
}

fn route(req: &Request, addr: SocketAddr, s: &mut State) -> Response {
    let path = req.path.as_str();
    match (req.method.as_str(), path) {
        ("GET", "/o/oauth2/v2/auth") => return authorize(req, s),
        ("POST", "/token") => return token(req, s),
        ("POST", "/revoke") => {
            let form = parse_query(&String::from_utf8_lossy(&req.body));
            let token = form
                .iter()
                .find(|(k, _)| k == "token")
                .map(|(_, v)| v.clone());
            return match token {
                Some(t) if t == s.refresh || t == s.access => {
                    s.revoked.push(t);
                    s.refresh = "fake-refresh-revoked".into();
                    Response::json(200, json!({}))
                }
                _ => Response::json(400, json!({"error": "invalid_token"})),
            };
        }
        _ => {}
    }
    let bearer = req
        .header("authorization")
        .and_then(|a| a.strip_prefix("Bearer "))
        .unwrap_or("");
    if bearer != s.access {
        return Response::error(401, "authError", "Invalid Credentials");
    }
    if let Some(rest) = path.strip_prefix("/upload/drive/v3/files") {
        return upload(req, rest.trim_start_matches('/'), addr, s);
    }
    if path == "/drive/v3/about" {
        let usage: u64 = s.files.values().map(|f| f.content.len() as u64).sum();
        let mut quota = json!({"usage": usage.to_string(), "usageInDrive": usage.to_string(), "usageInDriveTrash": "0"});
        if let Some(limit) = s.limit {
            quota["limit"] = json!(limit.to_string());
        }
        return Response::json(
            200,
            json!({"user": {"emailAddress": s.email, "displayName": "Example Person"}, "storageQuota": quota}),
        );
    }
    if path == "/drive/v3/files" {
        return match req.method.as_str() {
            "GET" => list(req, s),
            "POST" => {
                let meta: Value = serde_json::from_slice(&req.body).unwrap_or(Value::Null);
                create(s, &meta, Vec::new())
            }
            _ => Response::error(405, "badMethod", "method"),
        };
    }
    if let Some(id) = path.strip_prefix("/drive/v3/files/") {
        // `root` is My Drive's alias wherever a file id goes. Drive answers
        // it with My Drive's own id, which is also what its children name
        // in `parents`.
        if id == "root" || id == MY_DRIVE {
            return match req.method.as_str() {
                "GET" if req.param("alt") == Some("media") => {
                    Response::error(403, "fileNotDownloadable", "My Drive is a folder")
                }
                "GET" => Response::json(
                    200,
                    json!({"id": MY_DRIVE, "name": "My Drive", "mimeType": FOLDER_MIME}),
                ),
                _ => Response::error(403, "insufficientFilePermissions", "My Drive"),
            };
        }
        let Some(file) = s
            .files
            .get(id)
            .filter(|f| !f.trashed || req.method == "PATCH")
            .cloned()
        else {
            return Response::error(404, "notFound", &format!("File not found: {id}."));
        };
        return match req.method.as_str() {
            "GET" if req.param("alt") == Some("media") => media(req, &file),
            "GET" => Response::json(200, file.to_json()),
            "PATCH" => {
                let meta: Value = serde_json::from_slice(&req.body).unwrap_or(json!({}));
                let tick = s.tick();
                let f = s.files.get_mut(id).expect("present");
                if let Some(name) = meta.get("name").and_then(Value::as_str) {
                    f.name = name.into();
                }
                if meta.get("trashed").and_then(Value::as_bool) == Some(true) {
                    f.trashed = true;
                }
                if let (Some(add), Some(remove)) =
                    (req.param("addParents"), req.param("removeParents"))
                {
                    let my_drive = |p: &str| {
                        if p == "root" {
                            MY_DRIVE.to_string()
                        } else {
                            p.to_string()
                        }
                    };
                    let remove = my_drive(remove);
                    f.parents.retain(|p| *p != remove);
                    f.parents.push(my_drive(add));
                }
                f.modified = tick;
                Response::json(200, f.to_json())
            }
            "DELETE" => {
                s.files.remove(id);
                Response::empty(204)
            }
            _ => Response::error(405, "badMethod", "method"),
        };
    }
    Response::error(404, "notFound", "no such endpoint")
}

fn media(req: &Request, f: &FakeFile) -> Response {
    if !f.is_blob() {
        return Response::error(
            403,
            "fileNotDownloadable",
            "Only files with binary content can be downloaded.",
        );
    }
    let last = f.content.len().saturating_sub(1);
    if let Some(range) = req.header("range").and_then(|r| r.strip_prefix("bytes=")) {
        let (a, b) = range.split_once('-').unwrap_or((range, ""));
        let a: usize = a.parse().unwrap_or(0);
        let b: usize = b.parse().map_or(last, |b: usize| b.min(last));
        let body = f.content.get(a..=b).unwrap_or_default().to_vec();
        return Response {
            status: 206,
            headers: vec![(
                "Content-Range".into(),
                format!("bytes {a}-{b}/{}", f.content.len()),
            )],
            body,
        };
    }
    Response {
        status: 200,
        headers: vec![("Content-Type".into(), "application/octet-stream".into())],
        body: f.content.clone(),
    }
}

fn create(s: &mut State, meta: &Value, content: Vec<u8>) -> Response {
    let name = meta
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("Untitled")
        .to_string();
    let mime = meta
        .get("mimeType")
        .and_then(Value::as_str)
        .unwrap_or("application/octet-stream")
        .to_string();
    let parents: Vec<String> = meta
        .get("parents")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_else(|| vec![MY_DRIVE.into()])
        .into_iter()
        .map(|p| if p == "root" { MY_DRIVE.into() } else { p })
        .collect();
    if let Some(p) = parents.first() {
        let is_folder = s
            .files
            .get(p)
            .is_some_and(|f| f.mime == FOLDER_MIME && !f.trashed);
        if p != MY_DRIVE && !is_folder {
            return Response::error(404, "notFound", &format!("File not found: {p}."));
        }
    }
    let id = s.id("fake");
    let modified = s.tick();
    let f = FakeFile {
        id: id.clone(),
        name,
        mime,
        parents,
        content,
        modified,
        trashed: false,
        shortcut: None,
    };
    let v = f.to_json();
    s.files.insert(id, f);
    Response::json(200, v)
}

fn upload(req: &Request, rest: &str, addr: SocketAddr, s: &mut State) -> Response {
    if let Some(sid) = req.param("upload_id") {
        return resumable_put(req, sid, s);
    }
    let kind = req.param("uploadType").unwrap_or("");
    let target = (!rest.is_empty()).then(|| rest.to_string());
    if let Some(t) = &target {
        if !s.files.get(t).is_some_and(|f| !f.trashed) {
            return Response::error(404, "notFound", &format!("File not found: {t}."));
        }
    }
    match kind {
        "media" => {
            let Some(id) = target else {
                return Response::error(400, "badRequest", "media upload needs a file id here");
            };
            let tick = s.tick();
            let f = s.files.get_mut(&id).expect("checked");
            f.content = req.body.clone();
            f.modified = tick;
            Response::json(200, f.to_json())
        }
        "multipart" => {
            let ct = req.header("content-type").unwrap_or("");
            let Some(boundary) = ct.split("boundary=").nth(1) else {
                return Response::error(400, "badRequest", "no boundary");
            };
            let Some((meta, content)) = split_multipart(&req.body, boundary.trim()) else {
                return Response::error(400, "badRequest", "bad multipart body");
            };
            match target {
                Some(id) => {
                    let tick = s.tick();
                    let f = s.files.get_mut(&id).expect("checked");
                    f.content = content;
                    f.modified = tick;
                    Response::json(200, f.to_json())
                }
                None => create(s, &meta, content),
            }
        }
        "resumable" => {
            let total: u64 = req
                .header("x-upload-content-length")
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
            let meta: Value = serde_json::from_slice(&req.body).unwrap_or(json!({}));
            let sid = s.id("session");
            s.sessions.insert(
                sid.clone(),
                Session {
                    target,
                    meta,
                    total,
                    data: Vec::new(),
                },
            );
            let host = if s.foreign_sessions {
                format!("localhost:{}", addr.port())
            } else {
                addr.to_string()
            };
            Response::json(200, json!({})).header(
                "Location",
                format!("http://{host}/upload/drive/v3/files?uploadType=resumable&upload_id={sid}"),
            )
        }
        _ => Response::error(400, "badRequest", "unknown uploadType"),
    }
}

fn resumable_put(req: &Request, sid: &str, s: &mut State) -> Response {
    let Some(session) = s.sessions.get_mut(sid) else {
        return Response::error(404, "notFound", "no such upload session");
    };
    let range = req
        .header("content-range")
        .unwrap_or("")
        .trim_start_matches("bytes ")
        .to_string();
    let incomplete = |held: usize| {
        let r = Response::empty(308);
        if held == 0 {
            r
        } else {
            r.header("Range", format!("bytes=0-{}", held - 1))
        }
    };
    if range.starts_with("*/") {
        let held = session.data.len();
        if held as u64 >= session.total {
            return finish(sid, s);
        }
        return incomplete(held);
    }
    let span = range.split('/').next().unwrap_or("");
    let start: usize = span
        .split('-')
        .next()
        .and_then(|a| a.parse().ok())
        .unwrap_or(0);
    if start != session.data.len() {
        return incomplete(session.data.len());
    }
    session.data.extend_from_slice(&req.body);
    if (session.data.len() as u64) < session.total {
        return incomplete(session.data.len());
    }
    finish(sid, s)
}

fn finish(sid: &str, s: &mut State) -> Response {
    let session = s.sessions.remove(sid).expect("present");
    match session.target {
        Some(id) => {
            let tick = s.tick();
            let Some(f) = s.files.get_mut(&id) else {
                return Response::error(404, "notFound", "the file went away");
            };
            f.content = session.data;
            f.modified = tick;
            Response::json(200, f.to_json())
        }
        None => create(s, &session.meta, session.data),
    }
}

fn split_multipart(body: &[u8], boundary: &str) -> Option<(Value, Vec<u8>)> {
    let delim = format!("--{boundary}");
    let find = |needle: &[u8], from: usize| {
        body.get(from..)?
            .windows(needle.len())
            .position(|w| w == needle)
            .map(|p| p + from)
    };
    let first = find(delim.as_bytes(), 0)?;
    let meta_start = find(b"\r\n\r\n", first)? + 4;
    let second = find(delim.as_bytes(), meta_start)?;
    let meta: Value = serde_json::from_slice(body.get(meta_start..second.checked_sub(2)?)?).ok()?;
    let content_start = find(b"\r\n\r\n", second)? + 4;
    let end = find(format!("\r\n--{boundary}--").as_bytes(), content_start)?;
    Some((meta, body[content_start..end].to_vec()))
}

fn list(req: &Request, s: &State) -> Response {
    let q = req.param("q").unwrap_or("");
    let Some(expr) = Parser::new(q).parse() else {
        return Response::error(400, "invalid", &format!("Invalid Value: q ({q})"));
    };
    let matching: Vec<&FakeFile> = s.files.values().filter(|f| expr.eval(f)).collect();
    let page_size = req
        .param("pageSize")
        .and_then(|p| p.parse::<usize>().ok())
        .unwrap_or(100)
        .min(s.page_size);
    let offset: usize = req
        .param("pageToken")
        .and_then(|t| t.parse().ok())
        .unwrap_or(0);
    let page: Vec<Value> = matching
        .iter()
        .skip(offset)
        .take(page_size)
        .map(|f| f.to_json())
        .collect();
    let mut v = json!({"files": page});
    if offset + page_size < matching.len() {
        v["nextPageToken"] = json!((offset + page_size).to_string());
    }
    Response::json(200, v)
}

fn authorize(req: &Request, s: &mut State) -> Response {
    let (Some(redirect), Some(state), Some(challenge)) = (
        req.param("redirect_uri"),
        req.param("state"),
        req.param("code_challenge"),
    ) else {
        return Response::error(400, "invalid_request", "missing parameters");
    };
    if req.param("code_challenge_method") != Some("S256") {
        return Response::error(400, "invalid_request", "PKCE S256 required");
    }
    let code = s.id("code");
    let scope = s
        .granted
        .clone()
        .unwrap_or_else(|| req.param("scope").unwrap_or("").to_string());
    s.codes.insert(
        code.clone(),
        Pending {
            challenge: challenge.into(),
            redirect: redirect.into(),
            scope,
        },
    );
    let sep = if redirect.contains('?') { '&' } else { '?' };
    Response::empty(302).header(
        "Location",
        format!(
            "{redirect}{sep}code={code}&state={}",
            pk_cli_core::transport::percent_encode(state)
        ),
    )
}

fn token(req: &Request, s: &mut State) -> Response {
    let form: HashMap<String, String> = parse_query(&String::from_utf8_lossy(&req.body))
        .into_iter()
        .collect();
    if form.get("client_secret").map(String::as_str) != Some(CLIENT_SECRET) {
        return Response::json(
            401,
            json!({"error": "invalid_client", "error_description": "The OAuth client was not found."}),
        );
    }
    let bad_grant = |why: &str| {
        Response::json(
            400,
            json!({"error": "invalid_grant", "error_description": why}),
        )
    };
    match form.get("grant_type").map(String::as_str) {
        Some("authorization_code") => {
            let code = form.get("code").cloned().unwrap_or_default();
            let Some(p) = s.codes.remove(&code) else {
                return bad_grant("Bad Request");
            };
            let verifier = form.get("code_verifier").cloned().unwrap_or_default();
            if s256_challenge(&verifier) != p.challenge
                || form.get("redirect_uri") != Some(&p.redirect)
            {
                return bad_grant("Bad Request");
            }
            let n = s.next + 1;
            s.next = n;
            s.access = format!("fake-access-{n}");
            s.refresh = format!("fake-refresh-{n}");
            Response::json(
                200,
                json!({"access_token": s.access, "refresh_token": s.refresh,
                    "expires_in": 3599, "scope": p.scope, "token_type": "Bearer"}),
            )
        }
        Some("refresh_token") => {
            if form.get("refresh_token") != Some(&s.refresh) {
                return bad_grant("Token has been expired or revoked.");
            }
            let n = s.next + 1;
            s.next = n;
            s.access = format!("fake-access-{n}");
            Response::json(
                200,
                json!({"access_token": s.access, "expires_in": 3599,
                    "scope": "https://www.googleapis.com/auth/drive.file", "token_type": "Bearer"}),
            )
        }
        _ => Response::json(400, json!({"error": "unsupported_grant_type"})),
    }
}

// ── the query language, as much of it as the backend sends ─────────────

enum Expr {
    And(Box<Expr>, Box<Expr>),
    Or(Box<Expr>, Box<Expr>),
    Name(String),
    InParents(String),
    Trashed(bool),
    Mime(bool, String),
}

impl Expr {
    fn eval(&self, f: &FakeFile) -> bool {
        match self {
            Expr::And(a, b) => a.eval(f) && b.eval(f),
            Expr::Or(a, b) => a.eval(f) || b.eval(f),
            Expr::Name(n) => &f.name == n,
            Expr::InParents(p) if p == "root" => f.parents.iter().any(|x| x == MY_DRIVE),
            Expr::InParents(p) => f.parents.contains(p),
            Expr::Trashed(t) => f.trashed == *t,
            Expr::Mime(eq, m) => (&f.mime == m) == *eq,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Lit(String),
    Word(String),
    Op(&'static str),
    Open,
    Close,
}

struct Parser {
    toks: Vec<Tok>,
    at: usize,
}

impl Parser {
    fn new(q: &str) -> Parser {
        let mut toks = Vec::new();
        let mut chars = q.chars().peekable();
        while let Some(&c) = chars.peek() {
            match c {
                '(' => {
                    chars.next();
                    toks.push(Tok::Open);
                }
                ')' => {
                    chars.next();
                    toks.push(Tok::Close);
                }
                '\'' => {
                    chars.next();
                    let mut lit = String::new();
                    while let Some(c) = chars.next() {
                        match c {
                            '\\' => lit.extend(chars.next()),
                            '\'' => break,
                            c => lit.push(c),
                        }
                    }
                    toks.push(Tok::Lit(lit));
                }
                '=' => {
                    chars.next();
                    toks.push(Tok::Op("="));
                }
                '!' => {
                    chars.next();
                    chars.next();
                    toks.push(Tok::Op("!="));
                }
                c if c.is_alphanumeric() => {
                    let mut w = String::new();
                    while let Some(&c) = chars.peek().filter(|c| c.is_alphanumeric()) {
                        w.push(c);
                        chars.next();
                    }
                    toks.push(Tok::Word(w));
                }
                _ => {
                    chars.next();
                }
            }
        }
        Parser { toks, at: 0 }
    }

    fn parse(mut self) -> Option<Expr> {
        if self.toks.is_empty() {
            return Some(Expr::Trashed(false));
        }
        let e = self.or()?;
        (self.at == self.toks.len()).then_some(e)
    }

    fn next(&mut self) -> Option<Tok> {
        let t = self.toks.get(self.at).cloned();
        self.at += 1;
        t
    }

    fn peek_word(&self, w: &str) -> bool {
        matches!(self.toks.get(self.at), Some(Tok::Word(x)) if x == w)
    }

    fn or(&mut self) -> Option<Expr> {
        let mut e = self.and()?;
        while self.peek_word("or") {
            self.at += 1;
            e = Expr::Or(Box::new(e), Box::new(self.and()?));
        }
        Some(e)
    }

    fn and(&mut self) -> Option<Expr> {
        let mut e = self.term()?;
        while self.peek_word("and") {
            self.at += 1;
            e = Expr::And(Box::new(e), Box::new(self.term()?));
        }
        Some(e)
    }

    fn term(&mut self) -> Option<Expr> {
        match self.next()? {
            Tok::Open => {
                let e = self.or()?;
                (self.next()? == Tok::Close).then_some(e)
            }
            Tok::Lit(id) => (self.next()? == Tok::Word("in".into())
                && self.next()? == Tok::Word("parents".into()))
            .then_some(Expr::InParents(id)),
            Tok::Word(field) => {
                let Tok::Op(op) = self.next()? else {
                    return None;
                };
                match (field.as_str(), self.next()?) {
                    ("name", Tok::Lit(n)) if op == "=" => Some(Expr::Name(n)),
                    ("trashed", Tok::Word(b)) if op == "=" => Some(Expr::Trashed(b == "true")),
                    ("mimeType", Tok::Lit(m)) => Some(Expr::Mime(op == "=", m)),
                    _ => None,
                }
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_query_subset_parses_and_matches() {
        let f = FakeFile {
            id: "x".into(),
            name: "it's.pdf".into(),
            mime: "application/pdf".into(),
            parents: vec!["p1".into()],
            content: Vec::new(),
            modified: 0,
            trashed: false,
            shortcut: None,
        };
        let q = super::super::files::quote;
        let yes = [
            format!(
                "name = {} and {} in parents and trashed = false and mimeType != {}",
                q("it's.pdf"),
                q("p1"),
                q(FOLDER_MIME)
            ),
            format!(
                "({} in parents or {} in parents) and trashed = false",
                q("p0"),
                q("p1")
            ),
        ];
        for text in &yes {
            assert!(Parser::new(text).parse().expect(text).eval(&f), "{text}");
        }
        let no = format!("{} in parents and mimeType = {}", q("p1"), q(FOLDER_MIME));
        assert!(!Parser::new(&no).parse().unwrap().eval(&f));
        assert!(Parser::new("name contains 'x'").parse().is_none());
        assert_eq!(decode("a%27b+c%2F"), "a'b c/");
    }
}
