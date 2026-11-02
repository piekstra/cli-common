//! [`HostTransport`]: the host's `fetch` and WebCrypto behind
//! `pk_cli_core::transport::Transport`.

use pk_cli_core::transport::{HttpRequest, HttpResponse, Transport};
use pk_cli_core::CliError;
use serde_json::json;

use super::runtime::{HostOp, STATUS_FAILED, STATUS_UNSUPPORTED};

/// The transport a CLI's provider client uses inside the Worker: requests go
/// to the host's `fetch`, ES256 signatures to its WebCrypto. Stateless; the
/// host keeps imported keys between calls.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct HostTransport;

impl Transport for HostTransport {
    async fn send(&self, request: HttpRequest) -> Result<HttpResponse, CliError> {
        let wire = json!({
            "method": request.method,
            "url": request.url,
            "headers": request.headers,
            "body": request.body,
        });
        let wire = serde_json::to_vec(&wire)
            .map_err(|e| CliError::Other(format!("encoding the request: {e}")))?;
        let (status, data) = HostOp::fetch(&wire).await;
        let body = String::from_utf8_lossy(&data).into_owned();
        match u16::try_from(status) {
            Ok(status @ 100..=599) => Ok(HttpResponse { status, body }),
            _ if status == STATUS_FAILED => Err(CliError::Upstream(format!(
                "the request to {} failed: {body}",
                origin(&request.url)
            ))),
            _ => Err(CliError::Other(format!(
                "the host answered a fetch with status {status}"
            ))),
        }
    }

    async fn sign_es256(&self, key: &[u8], message: &[u8]) -> Option<Result<Vec<u8>, CliError>> {
        let (status, data) = HostOp::sign_es256(key, message).await;
        match status {
            0 if data.len() == 64 => Some(Ok(data)),
            0 => Some(Err(CliError::Other(format!(
                "the host's ES256 signature was {} bytes, not 64",
                data.len()
            )))),
            STATUS_UNSUPPORTED => None,
            _ => Some(Err(CliError::Other(format!(
                "the host could not sign: {}",
                String::from_utf8_lossy(&data)
            )))),
        }
    }
}

/// Scheme and host of a URL, for an error message: the path and query can
/// carry ids the message has no need to repeat.
fn origin(url: &str) -> &str {
    let after_scheme = url.find("://").map_or(0, |i| i + 3);
    url[after_scheme..]
        .find('/')
        .map_or(url, |i| &url[..after_scheme + i])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_error_names_the_origin_only() {
        assert_eq!(
            origin("https://api.example.com/v2/a/b?c=d"),
            "https://api.example.com"
        );
        assert_eq!(origin("https://api.example.com"), "https://api.example.com");
        assert_eq!(origin("not a url"), "not a url");
    }
}
