//! Download verification and filename hygiene for portal documents.
//!
//! Two mechanisms every documents-profile CLI needs and at least one grew
//! locally first (robinhood-cli, where both were paid for with live
//! findings):
//!
//! **A download is not a document until it matches its declared type.** An
//! expired or rejected pre-signed link routinely answers `200` with an HTML
//! login page, an S3 XML error, or a DRF JSON error body — and "downloaded
//! 1,204 bytes" of an error page is exactly the lie [`verify_download`]
//! exists to prevent. Providers also ship more than PDFs: a brokerage's
//! consolidated 1099 season produces a PDF *and* a CSV export, and older
//! statements can be HTML, so the check keys off the provider's declared
//! filetype rather than assuming `%PDF`.
//!
//! **Provider strings don't get to shape paths.** Anything a provider
//! controls that ends up in a filename — a document type, a filetype, a
//! date, an id — goes through [`fs_safe`] first, so a crafted value can't
//! traverse out of the output directory or smuggle in a second extension.

use pk_cli_core::CliError;

/// Verify a downloaded body against the filetype its listing declared.
///
/// - `None` or `"pdf"` → the strict [`verify_pdf`] magic check — the right
///   default for a document tool when the provider doesn't say.
/// - `"html"` → text check only: a real HTML statement and an HTML error
///   page are indistinguishable by shape, so the honest bar is "text, and
///   not a JSON error or XML error".
/// - any other declared type (`"csv"`, …) → text check *including* HTML
///   rejection: HTML where a non-HTML type was promised is a failure page.
///
/// Text bodies must also be valid UTF-8 in the inspected head — a binary
/// blob under a text filetype is some other lie — except that a multi-byte
/// character cut by the inspection window is tolerated as a truncation
/// artifact. (`from_utf8_lossy` alone would wave genuinely binary bodies
/// through as replacement characters.)
pub fn verify_download(bytes: &[u8], filetype: Option<&str>) -> Result<(), CliError> {
    match filetype {
        None | Some("pdf") => verify_pdf(bytes),
        Some(other) => verify_text(bytes, other),
    }
}

/// The `%PDF` magic check. Split out so it is testable without a server.
pub fn verify_pdf(bytes: &[u8]) -> Result<(), CliError> {
    if bytes.starts_with(b"%PDF") {
        return Ok(());
    }
    Err(not_the_document(bytes, "a PDF"))
}

/// Text formats (CSV, HTML, …) have no magic bytes, so the check is the
/// failure shapes we know arrive instead of documents.
fn verify_text(bytes: &[u8], label: &str) -> Result<(), CliError> {
    let head_bytes = &bytes[..bytes.len().min(256)];
    let head = match std::str::from_utf8(head_bytes) {
        Ok(h) => h,
        // A multi-byte character cut by the 256-byte window is a truncation
        // artifact of the window, not binary data — keep the valid prefix.
        Err(e) if e.error_len().is_none() => {
            std::str::from_utf8(&head_bytes[..e.valid_up_to()]).unwrap_or_default()
        }
        Err(_) => return Err(not_the_document(bytes, label)),
    };
    let head = head.trim_start().to_lowercase();
    let looks_like_json_error = head.starts_with("{\"detail\"") || head.starts_with("{ \"detail\"");
    let looks_like_xml = head.starts_with("<?xml");
    let looks_like_html = head.starts_with("<!doctype") || head.starts_with("<html");
    let html_ok = label.eq_ignore_ascii_case("html");
    if looks_like_json_error || looks_like_xml || (looks_like_html && !html_ok) {
        return Err(not_the_document(bytes, label));
    }
    Ok(())
}

fn not_the_document(bytes: &[u8], label: &str) -> CliError {
    let head = String::from_utf8_lossy(&bytes[..bytes.len().min(256)]).to_lowercase();
    let looks_like_html = head.contains("<html") || head.contains("<!doctype html");
    CliError::Upstream(format!(
        "the download was not {label} ({} bytes, starts with {:?}){} — list the documents again \
         for a fresh link",
        bytes.len(),
        String::from_utf8_lossy(&bytes[..bytes.len().min(16)]),
        if looks_like_html {
            ", and looks like an HTML page"
        } else {
            ""
        }
    ))
}

/// Reduce a provider-controlled string to characters that can't change a
/// path: ASCII alphanumerics, `_`, and `-`; everything else becomes `_`.
///
/// Apply to **every** component a filename is built from — type, filetype,
/// date, id. A genuine ISO date or UUID passes through unchanged; a crafted
/// `../../etc` cannot traverse and a `pdf/../x` cannot smuggle a path.
pub fn fs_safe(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pdf_magic_is_required() {
        assert!(verify_pdf(b"%PDF-1.7\n...").is_ok());

        let err = verify_pdf(b"<!DOCTYPE html><html><body>Sign in").unwrap_err();
        assert_eq!(err.exit_code(), 5);
        assert!(err.to_string().contains("HTML"));

        // The empty body case must not panic on the slice bounds.
        assert!(verify_pdf(b"").is_err());
        assert!(verify_pdf(b"%PD").is_err());
    }

    /// Live finding (robinhood-cli, 2026-08-12): a consolidated 1099 also
    /// ships as a CSV export, which an unconditional `%PDF` rule rejects.
    #[test]
    fn a_declared_csv_is_accepted_and_a_declared_pdf_still_is_not() {
        assert!(verify_download(b"1099-DIV,ACCOUNT NUMBER,TAX YEAR\n", Some("csv")).is_ok());
        assert!(verify_download(b"1099-DIV,ACCOUNT NUMBER,TAX YEAR\n", Some("pdf")).is_err());
        // No declared filetype falls back to the strict PDF rule.
        assert!(verify_download(b"1099-DIV,ACCOUNT NUMBER,TAX YEAR\n", None).is_err());
        assert!(verify_download(b"%PDF-1.5\n...", None).is_ok());
    }

    /// The failure shapes an expired link actually serves — a DRF JSON
    /// error, an S3 XML error, an HTML page — must stay errors for text
    /// filetypes.
    #[test]
    fn text_downloads_still_reject_the_known_failure_shapes() {
        for body in [
            &b"{\"detail\":\"Not found.\"}"[..],
            &b"<?xml version=\"1.0\"?><Error><Code>ExpiredToken</Code></Error>"[..],
            &b"<!DOCTYPE html><html>login</html>"[..],
            &[0xFF, 0xFE, 0x00, 0x01][..], // binary under a text filetype
        ] {
            assert!(verify_download(body, Some("csv")).is_err(), "{body:?}");
        }
        // An HTML statement is allowed to be HTML — but not a JSON error.
        assert!(verify_download(b"<!DOCTYPE html><html>statement</html>", Some("html")).is_ok());
        assert!(verify_download(b"{\"detail\":\"gone\"}", Some("html")).is_err());
        // Unknown filetypes get the failure-shape screen, not a hard reject.
        assert!(verify_download(b"some,future,format", Some("tsv")).is_ok());
        assert!(verify_download(b"<html>error</html>", Some("tsv")).is_err());
    }

    /// A multi-byte character straddling the 256-byte inspection window is a
    /// truncation artifact, not binary data.
    #[test]
    fn a_char_cut_by_the_inspection_window_is_not_binary() {
        let mut straddle = vec![b'a'; 255];
        straddle.extend_from_slice("é more,data".as_bytes());
        assert!(verify_download(&straddle, Some("csv")).is_ok());
    }

    #[test]
    fn fs_safe_neutralizes_traversal_and_keeps_honest_values() {
        assert_eq!(fs_safe("../../etc"), "______etc");
        assert_eq!(fs_safe("pdf/../x"), "pdf____x");
        // Honest values pass through unchanged.
        assert_eq!(fs_safe("2026-03-20"), "2026-03-20");
        assert_eq!(fs_safe("account_statement"), "account_statement");
        assert_eq!(
            fs_safe("00000000-0000-4000-8000-000000000001"),
            "00000000-0000-4000-8000-000000000001"
        );
    }
}
