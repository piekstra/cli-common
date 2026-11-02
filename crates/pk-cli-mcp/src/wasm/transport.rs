//! [`HostTransport`]: the host's `fetch` and WebCrypto behind
//! `pk_cli_core::transport::{Transport, PlatformSigner}`.

use pk_cli_core::transport::{HttpRequest, HttpResponse, PlatformSigner, Transport};
use pk_cli_core::CliError;
use serde_json::json;

use super::runtime::{HostOp, STATUS_FAILED, STATUS_UNSUPPORTED};

/// The transport a CLI's provider client uses inside the Worker: requests go
/// to the host's `fetch` (redirects handed back, never followed; no cookies),
/// ES256 signatures to its WebCrypto. Stateless; the host keeps imported keys
/// between calls.
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
        match u16::try_from(status) {
            Ok(status @ 100..=599) => {
                let (headers, body) = decode_response(&data).map_err(|why| {
                    CliError::Other(format!("the host's answer to a fetch is malformed: {why}"))
                })?;
                let mut response = HttpResponse::new(status, body);
                response.headers = headers;
                Ok(response)
            }
            _ if status == STATUS_FAILED => Err(CliError::Upstream(format!(
                "the request to {} failed: {}",
                request.origin(),
                String::from_utf8_lossy(&data)
            ))),
            _ => Err(CliError::Other(format!(
                "the host answered a fetch with status {status}"
            ))),
        }
    }
}

impl PlatformSigner for HostTransport {
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

/// A fetch's answer as the ABI frames it: the length of the headers' JSON
/// (`u32`, little-endian), that JSON (`[[name, value], …]`), then the body.
/// The host does this in JavaScript; this is the test host's copy.
#[cfg(any(test, feature = "test-util"))]
pub(crate) fn encode_response(headers: &[(String, String)], body: &[u8]) -> Vec<u8> {
    let names = serde_json::to_vec(headers).unwrap_or_else(|_| b"[]".to_vec());
    let mut out = Vec::with_capacity(4 + names.len() + body.len());
    out.extend_from_slice(&(names.len() as u32).to_le_bytes());
    out.extend_from_slice(&names);
    out.extend_from_slice(body);
    out
}

fn decode_response(data: &[u8]) -> Result<(Vec<(String, String)>, String), String> {
    let (len, rest) = data
        .split_first_chunk::<4>()
        .ok_or("shorter than its length prefix")?;
    let len = u32::from_le_bytes(*len) as usize;
    if rest.len() < len {
        return Err(format!("headers of {len} bytes in {} left", rest.len()));
    }
    let (headers, body) = rest.split_at(len);
    let headers: Vec<(String, String)> =
        serde_json::from_slice(headers).map_err(|e| format!("headers: {e}"))?;
    Ok((headers, String::from_utf8_lossy(body).into_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_response_round_trips_through_its_framing() {
        let headers = vec![("location".to_string(), "https://h.test/next".to_string())];
        let framed = encode_response(&headers, "körper".as_bytes());
        assert_eq!(
            decode_response(&framed).unwrap(),
            (headers, "körper".to_string())
        );
        assert_eq!(
            decode_response(&encode_response(&[], b"")).unwrap(),
            (vec![], String::new())
        );
        assert!(decode_response(b"\x01\x00").is_err());
        assert!(decode_response(b"\x09\x00\x00\x00[]").is_err());
        assert!(decode_response(b"\x02\x00\x00\x00{}").is_err());
    }
}
