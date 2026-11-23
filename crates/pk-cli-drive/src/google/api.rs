//! One authorized Drive request, retried the way Google asks: exponential
//! backoff with jitter on 429, on 403 with a rate-limit reason and on 5xx,
//! `Retry-After` honoured, an expired access token renewed once on 401, and
//! everything bounded by the remote's budget.

use std::time::{Duration, Instant};

use pk_cli_core::{CliError, Reason};
use pk_cli_secrets::Secret;
use reqwest::blocking::{Client, RequestBuilder};
use reqwest::header::HeaderMap;

use super::endpoints::Endpoints;
use super::error::GoogleError;

/// Where an [`Api`] gets its access token. The account behind a Drive
/// profile, or a fixed token in a test.
pub(crate) trait Tokens {
    /// A token believed current: the stored one, renewed first when it is
    /// about to expire.
    fn access(&self) -> Result<Secret, CliError>;
    /// A new token after Drive refused `refused` (401). Renews only when
    /// `refused` is still the stored one, so two processes renew once.
    fn renew(&self, refused: &Secret) -> Result<Secret, CliError>;
    /// The command that signs the profile in again, for remedies.
    fn relogin(&self) -> String;
}

/// How often and how patiently a call is retried.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Retry {
    /// Attempts in all, the first included.
    pub attempts: u32,
    /// The first wait; each next one doubles, up to `cap`, plus up to
    /// `base` of jitter.
    pub base: Duration,
    pub cap: Duration,
}

impl Retry {
    /// Google's advice for the Drive API: 1 s, 2 s, 4 s … up to 32 s, with
    /// jitter.
    pub const DEFAULT: Retry = Retry {
        attempts: 7,
        base: Duration::from_secs(1),
        cap: Duration::from_secs(32),
    };

    /// No waiting, for tests against the fake server.
    pub const IMMEDIATE: Retry = Retry {
        attempts: 7,
        base: Duration::ZERO,
        cap: Duration::ZERO,
    };

    fn delay(&self, retry: u32, retry_after: Option<Duration>) -> Duration {
        if let Some(asked) = retry_after {
            return asked.min(self.cap.max(Duration::from_secs(1)) * 2);
        }
        let exp = self
            .base
            .saturating_mul(1u32 << retry.min(16))
            .min(self.cap);
        exp + jitter(self.base)
    }
}

fn jitter(up_to: Duration) -> Duration {
    if up_to.is_zero() {
        return Duration::ZERO;
    }
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    up_to.mul_f64(f64::from(nanos % 1000) / 1000.0)
}

/// A whole answer: status, headers and body. Bodies are bounded by what the
/// callers ask for (metadata, a listing page, a download range), never a
/// whole large file.
pub(crate) struct Answer {
    pub status: u16,
    pub headers: HeaderMap,
    pub body: Vec<u8>,
}

impl Answer {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
    }

    pub fn json<T: serde::de::DeserializeOwned>(&self, what: &str) -> Result<T, CliError> {
        serde_json::from_slice(&self.body).map_err(|e| {
            CliError::Upstream(format!(
                "{what}: Google Drive sent an answer that does not read: {e}"
            ))
        })
    }
}

/// The reason of the error a non-idempotent call gives when it cannot tell
/// whether Drive acted on it (a timeout, a 5xx), so the caller looks before
/// it tries again. See [`is_ambiguous`].
const AMBIGUOUS: Reason = Reason::Provider("drive_ambiguous");

/// Did a non-idempotent call fail without saying whether it happened?
pub(crate) fn is_ambiguous(err: &CliError) -> bool {
    err.reason() == Some(AMBIGUOUS)
}

/// A failure that waiting might cure: Drive unreachable, a 5xx or a rate
/// limit after their retries, or an answer that never came. Not a refusal
/// (auth, permission, quota, a bad request, a missing file or session).
pub(crate) fn is_transient(err: &CliError) -> bool {
    err.exit_code() == 5 && matches!(err.reason(), None | Some(AMBIGUOUS))
}

/// The Drive API for one account.
pub(crate) struct Api {
    http: Client,
    pub endpoints: Endpoints,
    tokens: Box<dyn Tokens>,
    deadline: Option<Instant>,
    pub retry: Retry,
}

/// The longest one request may take, whatever the budget left.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

impl Api {
    pub fn new(
        bin: &str,
        version: &str,
        endpoints: Endpoints,
        tokens: Box<dyn Tokens>,
    ) -> Result<Self, CliError> {
        let http = Client::builder()
            .user_agent(format!("{bin}/{version} (pk-cli-drive)"))
            .connect_timeout(Duration::from_secs(20))
            .timeout(None)
            .redirect(pk_cli_core::transport::secure_redirects())
            .build()
            .map_err(|e| CliError::Other(format!("failed to build the HTTP client: {e}")))?;
        Ok(Api {
            http,
            endpoints,
            tokens,
            deadline: None,
            retry: Retry::DEFAULT,
        })
    }

    pub fn with_deadline(mut self, deadline: Instant) -> Self {
        self.deadline = Some(deadline);
        self
    }

    pub fn relogin(&self) -> String {
        self.tokens.relogin()
    }

    /// One call. `build` makes the request afresh for each attempt (the
    /// bearer token is added here); `accept` lists the non-2xx statuses that
    /// are answers rather than failures (404 for a lookup, 308 for an
    /// unfinished upload). `idempotent: false` (a create) is retried only on
    /// refusals that prove nothing happened (429, rate limits, a connection
    /// that never opened); any other failure comes back at once as an
    /// [`is_ambiguous`] error for the caller to reconcile.
    pub fn call(
        &self,
        what: &str,
        idempotent: bool,
        accept: &[u16],
        build: &dyn Fn(&Client) -> RequestBuilder,
    ) -> Result<Answer, CliError> {
        let mut token = self.tokens.access()?;
        let mut renewed = false;
        let mut retry = 0;
        loop {
            let timeout = self.request_timeout(what)?;
            let sent = build(&self.http)
                .bearer_auth(token.expose())
                .timeout(timeout)
                .send();
            let failure = match sent {
                Ok(resp) => {
                    let status = resp.status().as_u16();
                    let headers = resp.headers().clone();
                    let body = match resp.bytes() {
                        Ok(b) => b.to_vec(),
                        Err(e) => {
                            let err = transport_error(what, &e);
                            if !idempotent {
                                return Err(err.because(AMBIGUOUS));
                            }
                            self.wait(what, &mut retry, None, err)?;
                            continue;
                        }
                    };
                    if (200..300).contains(&status) || accept.contains(&status) {
                        return Ok(Answer {
                            status,
                            headers,
                            body,
                        });
                    }
                    let refusal = GoogleError::parse(status, &body);
                    if status == 401 && !renewed {
                        renewed = true;
                        token = self.tokens.renew(&token)?;
                        continue;
                    }
                    let retry_after = pk_cli_http::retry_after(&headers);
                    if refusal.is_rate_limit() {
                        let err = refusal.into_cli(what, &self.relogin());
                        self.wait(what, &mut retry, retry_after, err)?;
                        continue;
                    }
                    if status >= 500 {
                        let err = refusal.into_cli(what, &self.relogin());
                        if !idempotent {
                            return Err(err.because(AMBIGUOUS));
                        }
                        self.wait(what, &mut retry, retry_after, err)?;
                        continue;
                    }
                    return Err(refusal.into_cli(what, &self.relogin()));
                }
                Err(e) => e,
            };
            let err = transport_error(what, &failure);
            if !idempotent && !failure.is_connect() {
                return Err(err.because(AMBIGUOUS));
            }
            self.wait(what, &mut retry, None, err)?;
        }
    }

    fn request_timeout(&self, what: &str) -> Result<Duration, CliError> {
        match self.deadline {
            None => Ok(REQUEST_TIMEOUT),
            Some(deadline) => {
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    return Err(CliError::Upstream(format!(
                        "{what}: the command's time for Google Drive ran out"
                    )));
                }
                Ok(left.min(REQUEST_TIMEOUT))
            }
        }
    }

    /// Wait before retry `*retry + 1`, or give up with `err` when the
    /// attempts or the budget are spent.
    fn wait(
        &self,
        what: &str,
        retry: &mut u32,
        retry_after: Option<Duration>,
        err: CliError,
    ) -> Result<(), CliError> {
        if *retry + 1 >= self.retry.attempts {
            return Err(err);
        }
        let delay = self.retry.delay(*retry, retry_after);
        if let Some(deadline) = self.deadline {
            if Instant::now() + delay >= deadline {
                return Err(err);
            }
        }
        *retry += 1;
        if !delay.is_zero() {
            eprintln!(
                "note: {what}: {}; retrying in {:.0?} (attempt {} of {})",
                err.message(),
                delay,
                *retry + 1,
                self.retry.attempts
            );
            std::thread::sleep(delay);
        }
        Ok(())
    }
}

fn transport_error(what: &str, e: &reqwest::Error) -> CliError {
    // `without_url`: the query can carry a file id or an upload session.
    let shown = if let Some(source) = std::error::Error::source(e) {
        format!(
            "{}: {source}",
            e.to_string().split(" for url").next().unwrap_or("error")
        )
    } else {
        e.to_string()
            .split(" for url")
            .next()
            .unwrap_or("error")
            .to_string()
    };
    CliError::Upstream(format!(
        "{what}: the request to Google Drive failed: {shown}"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delays_double_to_the_cap_and_honour_retry_after() {
        let r = Retry {
            attempts: 7,
            base: Duration::from_secs(1),
            cap: Duration::from_secs(32),
        };
        for (n, floor) in [(0u32, 1u64), (1, 2), (2, 4), (5, 32), (9, 32)] {
            let d = r.delay(n, None);
            assert!(d >= Duration::from_secs(floor), "{n}: {d:?}");
            assert!(d < Duration::from_secs(floor + 1), "{n}: {d:?}");
        }
        assert_eq!(
            r.delay(0, Some(Duration::from_secs(7))),
            Duration::from_secs(7)
        );
        assert_eq!(
            r.delay(0, Some(Duration::from_secs(900))),
            Duration::from_secs(64)
        );
        assert_eq!(Retry::IMMEDIATE.delay(3, None), Duration::ZERO);
    }
}
