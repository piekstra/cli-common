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
//! consolidated 1099 season produces a PDF *and* a CSV export, older
//! statements can be HTML, and portals and scanners hand over PNG, JPEG,
//! TIFF and HEIC images, so the check keys off the provider's declared
//! filetype rather than assuming `%PDF`. A file that arrives with no
//! declared type can be named from its head by [`sniff`].
//!
//! **Provider strings don't get to shape paths.** Anything a provider
//! controls that ends up in a filename — a document type, a filetype, a
//! date, an id — goes through [`fs_safe`] first, so a crafted value can't
//! traverse out of the output directory or smuggle in a second extension.

use pk_cli_core::CliError;

/// Verify a downloaded body against the filetype its listing declared.
///
/// The contract, in full:
///
/// - The declared filetype is compared **case-insensitively** throughout —
///   a provider that says `"PDF"` gets the PDF rule.
/// - An **empty body fails** for every declared type; nothing is a document.
/// - `None` or `"pdf"` → the strict [`verify_pdf`] magic check — the right
///   default for a document tool when the provider doesn't say.
/// - An image type → that format's magic bytes at the start of the body:
///   `"png"` the 8-byte PNG signature (`89 50 4E 47 0D 0A 1A 0A`);
///   `"jpg"`/`"jpeg"` `FF D8 FF`; `"tif"`/`"tiff"` `II*\0` or `MM\0*`
///   (`49 49 2A 00`, `4D 4D 00 2A`); `"heic"`/`"heif"` an ISO-BMFF `ftyp`
///   box (bytes 4..8 `ftyp`) whose major brand (bytes 8..12) is one of
///   `heic`, `heix`, `hevc`, `hevx`, `heim`, `heis`, `mif1`, `msf1`. A body
///   shorter than its magic fails.
/// - Any other declared type (`"csv"`, `"html"`, …) → the text check: the
///   inspected head must be valid UTF-8 (a binary blob under a text filetype
///   is some other lie; a multi-byte character cut by the inspection window
///   is tolerated as a truncation artifact — `from_utf8_lossy` alone would
///   wave genuinely binary bodies through), and the failure shapes an
///   expired pre-signed link actually serves — a JSON error object, an XML
///   error, an HTML page — are rejected **unless the declared type is that
///   shape** (`json`/`xml`/`html`), since for those a real document and an
///   error body are indistinguishable by shape.
pub fn verify_download(bytes: &[u8], filetype: Option<&str>) -> Result<(), CliError> {
    // A zero-byte body is never a document, whatever type was declared —
    // and it would otherwise sail through every text check below.
    if bytes.is_empty() {
        return Err(not_the_document(bytes, filetype.unwrap_or("a document")));
    }
    // Normalize once at the boundary; every comparison below is exact.
    let label = filetype.map_or_else(|| PDF.names[0].to_string(), str::to_ascii_lowercase);
    match BY_MAGIC.iter().find(|f| f.names.contains(&label.as_str())) {
        Some(format) => verify_magic(bytes, format.magic, format.label),
        None => verify_text(bytes, &label),
    }
}

/// Name a body's format from its head, for a file that arrives with no
/// declared type: `"pdf"`, `"png"`, `"jpeg"`, `"tiff"`, `"heic"` by the
/// magic bytes [`verify_download`] checks, or `"xml"` for UTF-8 text that
/// opens with markup (an optional byte-order mark and whitespace, then `<`)
/// and is not what the text check reads as an HTML page (`<html`, or any
/// `<!doctype`). Anything else is `None`.
///
/// Every name it returns is a filetype [`verify_download`] accepts the same
/// body under, so `sniff` then `verify_download` is the check for an
/// undeclared file. A sniffed type is a guess from the first bytes, not a
/// validation of the whole file.
pub fn sniff(bytes: &[u8]) -> Option<&'static str> {
    if let Some(format) = BY_MAGIC.iter().find(|f| (f.magic)(bytes)) {
        return Some(format.names[0]);
    }
    let head = text_head(bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes))?;
    (head.starts_with('<') && !looks_like_html(&head)).then_some("xml")
}

/// Whether a body starts with one format's magic bytes.
type Magic = fn(&[u8]) -> bool;

/// A format known by its magic bytes: the declared filetypes that mean it
/// (the first is the name [`sniff`] gives it), its check, and the label a
/// failure names.
struct Format {
    names: &'static [&'static str],
    magic: Magic,
    label: &'static str,
}

/// PDF, also what [`verify_pdf`] checks and what [`verify_download`]
/// assumes when no type is declared.
const PDF: Format = Format {
    names: &["pdf"],
    magic: is_pdf,
    label: "a PDF",
};

/// The one list both [`verify_download`] and [`sniff`] read, so a format
/// `sniff` names is checked by the same magic under that name.
const BY_MAGIC: [Format; 5] = [
    PDF,
    Format {
        names: &["png"],
        magic: is_png,
        label: "a PNG",
    },
    Format {
        names: &["jpeg", "jpg"],
        magic: is_jpeg,
        label: "a JPEG",
    },
    Format {
        names: &["tiff", "tif"],
        magic: is_tiff,
        label: "a TIFF",
    },
    Format {
        names: &["heic", "heif"],
        magic: is_heic,
        label: "a HEIC/HEIF image",
    },
];

fn verify_magic(bytes: &[u8], matches: Magic, label: &str) -> Result<(), CliError> {
    if matches(bytes) {
        return Ok(());
    }
    Err(not_the_document(bytes, label))
}

fn is_pdf(bytes: &[u8]) -> bool {
    bytes.starts_with(b"%PDF")
}

fn is_png(bytes: &[u8]) -> bool {
    bytes.starts_with(b"\x89PNG\r\n\x1a\n")
}

fn is_jpeg(bytes: &[u8]) -> bool {
    bytes.starts_with(b"\xFF\xD8\xFF")
}

/// Little-endian (`II`) or big-endian (`MM`) byte order, then 42.
fn is_tiff(bytes: &[u8]) -> bool {
    bytes.starts_with(b"II*\0") || bytes.starts_with(b"MM\0*")
}

/// The brands HEIC and HEIF stills and sequences declare as their major
/// brand (ISO/IEC 23008-12).
const HEIF_BRANDS: [&[u8; 4]; 8] = [
    b"heic", b"heix", b"hevc", b"hevx", b"heim", b"heis", b"mif1", b"msf1",
];

/// An ISO-BMFF file opens with its `ftyp` box: a 4-byte size, `ftyp`, then
/// the major brand.
fn is_heic(bytes: &[u8]) -> bool {
    bytes.len() >= 12
        && &bytes[4..8] == b"ftyp"
        && HEIF_BRANDS.iter().any(|brand| &bytes[8..12] == *brand)
}

/// The `%PDF` magic check. Split out so it is testable without a server.
pub fn verify_pdf(bytes: &[u8]) -> Result<(), CliError> {
    verify_magic(bytes, PDF.magic, PDF.label)
}

/// Text formats (CSV, HTML, …) have no magic bytes, so the check is the
/// failure shapes we know arrive instead of documents.
///
/// Each rejection is waived when the declared filetype *is* that shape — an
/// HTML statement is allowed to be HTML, a declared JSON export is allowed
/// to be a JSON object — because for those types a real document and an
/// error body are indistinguishable by shape, and refusing everything would
/// make the type undownloadable.
fn verify_text(bytes: &[u8], label: &str) -> Result<(), CliError> {
    let Some(head) = text_head(bytes) else {
        return Err(not_the_document(bytes, label));
    };
    // Any JSON object head, not just one provider's `{"detail"` spelling —
    // `{"error"`, `{"message"`, and friends are equally common ways for an
    // expired link to answer. No legitimate CSV/HTML/TSV body starts `{"`.
    let looks_like_json = head.starts_with('{') && head.contains('"');
    let looks_like_xml = head.starts_with("<?xml");
    // The label was lowercased at the dispatch boundary; exact compares here.
    if (looks_like_json && label != "json")
        || (looks_like_xml && label != "xml")
        || (looks_like_html(&head) && label != "html")
    {
        return Err(not_the_document(bytes, label));
    }
    Ok(())
}

/// The first 256 bytes as text, leading whitespace trimmed, lowercased;
/// `None` when they are not UTF-8.
fn text_head(bytes: &[u8]) -> Option<String> {
    let head_bytes = &bytes[..bytes.len().min(256)];
    let head = match std::str::from_utf8(head_bytes) {
        Ok(h) => h,
        // A multi-byte character cut by the 256-byte window is a truncation
        // artifact of the window, not binary data — keep the valid prefix.
        Err(e) if e.error_len().is_none() => {
            std::str::from_utf8(&head_bytes[..e.valid_up_to()]).unwrap_or_default()
        }
        Err(_) => return None,
    };
    Some(head.trim_start().to_lowercase())
}

fn looks_like_html(head: &str) -> bool {
    head.starts_with("<!doctype") || head.starts_with("<html")
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

    /// A zero-byte body is never a document, whatever the declared type —
    /// and the PDF dispatch is case-insensitive like every other label
    /// comparison.
    #[test]
    fn empty_bodies_fail_and_pdf_dispatch_ignores_case() {
        for ft in [None, Some("pdf"), Some("csv"), Some("html"), Some("tsv")] {
            assert!(verify_download(b"", ft).is_err(), "{ft:?}");
        }
        assert!(verify_download(b"%PDF-1.7\n...", Some("PDF")).is_ok());
        assert!(verify_download(b"1099-DIV,ACCOUNT\n", Some("PDF")).is_err());
    }

    /// The JSON-error screen is any object head, not one provider's
    /// `{"detail"` spelling — and each markup rejection is waived when the
    /// declared type *is* that shape.
    #[test]
    fn error_shapes_are_generic_and_declared_types_are_exempt() {
        for body in [
            &b"{\"error\":\"Unauthorized\"}"[..],
            &b"{\"message\":\"link expired\"}"[..],
            &b"{\"errors\":[{\"code\":401}]}"[..],
        ] {
            assert!(verify_download(body, Some("csv")).is_err(), "{body:?}");
        }
        // A declared JSON export is allowed to be a JSON object…
        assert!(verify_download(b"{\"rows\": []}", Some("json")).is_ok());
        // …but not an HTML page; and a declared XML feed may be XML.
        assert!(verify_download(b"<html>login</html>", Some("json")).is_err());
        assert!(verify_download(b"<?xml version=\"1.0\"?><feed/>", Some("xml")).is_ok());
    }

    /// A multi-byte character straddling the 256-byte inspection window is a
    /// truncation artifact, not binary data.
    #[test]
    fn a_char_cut_by_the_inspection_window_is_not_binary() {
        let mut straddle = vec![b'a'; 255];
        straddle.extend_from_slice("é more,data".as_bytes());
        assert!(verify_download(&straddle, Some("csv")).is_ok());
    }

    const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR";
    const JPEG: &[u8] = b"\xFF\xD8\xFF\xE0\0\x10JFIF\0";
    const TIFF_LE: &[u8] = b"II*\0\x08\0\0\0";
    const TIFF_BE: &[u8] = b"MM\0*\0\0\0\x08";
    const HEIC: &[u8] = b"\0\0\0\x18ftypheic\0\0\0\0mif1heic";
    const LOGIN_PAGE: &[u8] = b"<!DOCTYPE html><html><body>Sign in";

    /// A body with `brand` as its ISO-BMFF major brand.
    fn bmff(brand: &[u8; 4]) -> Vec<u8> {
        let mut body = b"\0\0\0\x18ftyp".to_vec();
        body.extend_from_slice(brand);
        body.extend_from_slice(b"\0\0\0\0mif1");
        body
    }

    /// A real PNG, JPEG, TIFF or HEIC used to fail as "binary under a text
    /// filetype". Every spelling of each image type, in any case, now gets
    /// that format's magic check.
    #[test]
    fn images_pass_under_their_declared_type_in_any_case() {
        for (body, types) in [
            (PNG, &["png", "PNG"][..]),
            (JPEG, &["jpg", "jpeg", "JPG", "Jpeg"][..]),
            (TIFF_LE, &["tif", "tiff", "TIFF"][..]),
            (TIFF_BE, &["tif", "tiff", "Tif"][..]),
            (HEIC, &["heic", "heif", "HEIC", "HEIF"][..]),
        ] {
            for ft in types {
                assert!(verify_download(body, Some(ft)).is_ok(), "{ft} {body:?}");
            }
        }
        for brand in [
            b"heic", b"heix", b"hevc", b"hevx", b"heim", b"heis", b"mif1", b"msf1",
        ] {
            assert!(
                verify_download(&bmff(brand), Some("heic")).is_ok(),
                "{brand:?}"
            );
        }
    }

    /// Another format's bytes, an error page, or a near miss on the magic
    /// all fail, with the declared type named.
    #[test]
    fn an_image_with_the_wrong_magic_fails() {
        for (ft, body, label) in [
            ("png", JPEG, "a PNG"),
            ("png", b"\x89PNG\r\n\x1a\x0a"[..7].as_ref(), "a PNG"),
            ("png", b"\x89PNG\n\r\x1a\n\0".as_ref(), "a PNG"),
            ("jpeg", PNG, "a JPEG"),
            ("jpg", b"\xFF\xD8\xFE\xE0".as_ref(), "a JPEG"),
            ("tiff", b"II\0*\x08\0\0\0".as_ref(), "a TIFF"),
            ("tif", b"MM*\0\0\0\0\x08".as_ref(), "a TIFF"),
            ("tiff", JPEG, "a TIFF"),
            ("heic", TIFF_LE, "a HEIC/HEIF image"),
            ("heic", &bmff(b"avif"), "a HEIC/HEIF image"),
            ("heif", &bmff(b"isom"), "a HEIC/HEIF image"),
            ("heic", b"ftypheic\0\0\0\0".as_ref(), "a HEIC/HEIF image"),
        ] {
            let err = verify_download(body, Some(ft)).unwrap_err();
            assert_eq!(err.exit_code(), 5, "{ft} {body:?}");
            assert!(
                err.to_string()
                    .contains(&format!("the download was not {label} (")),
                "{err}"
            );
        }
        for ft in ["png", "jpeg", "tiff", "heic"] {
            let err = verify_download(LOGIN_PAGE, Some(ft)).unwrap_err();
            assert!(err.to_string().contains("looks like an HTML page"), "{err}");
        }
    }

    /// A body cut off inside its magic fails rather than panicking on a
    /// slice past its end.
    #[test]
    fn a_body_shorter_than_its_magic_fails() {
        for (ft, body) in [
            ("png", &PNG[..7]),
            ("jpeg", &JPEG[..2]),
            ("tiff", &TIFF_LE[..3]),
            ("tiff", &TIFF_BE[..3]),
            ("heic", &HEIC[..11]),
            ("heic", &HEIC[..6]),
        ] {
            assert!(verify_download(body, Some(ft)).is_err(), "{ft} {body:?}");
        }
        for ft in ["png", "jpg", "tif", "heif"] {
            assert!(verify_download(b"", Some(ft)).is_err(), "{ft}");
        }
    }

    /// Each declared name means one format, and the name `sniff` gives a
    /// format leads back to that same format.
    #[test]
    fn every_name_in_the_format_table_means_one_format() {
        let mut seen = std::collections::HashSet::new();
        for format in &BY_MAGIC {
            assert!(!format.names.is_empty(), "{}", format.label);
            for name in format.names {
                assert_eq!(*name, name.to_ascii_lowercase());
                assert!(seen.insert(*name), "{name} is in two formats");
            }
        }
    }

    #[test]
    fn sniff_names_each_format_from_its_head() {
        for (body, name) in [
            (&b"%PDF-1.7\n..."[..], "pdf"),
            (PNG, "png"),
            (JPEG, "jpeg"),
            (TIFF_LE, "tiff"),
            (TIFF_BE, "tiff"),
            (HEIC, "heic"),
            (&bmff(b"mif1")[..], "heic"),
            (&b"<?xml version=\"1.0\"?><ClinicalDocument/>"[..], "xml"),
            (&b"\xEF\xBB\xBF\r\n  <?xml version=\"1.0\"?>"[..], "xml"),
            (&b"<ClinicalDocument xmlns=\"urn:hl7-org:v3\">"[..], "xml"),
            (&b"\n\t<feed/>"[..], "xml"),
        ] {
            assert_eq!(sniff(body), Some(name), "{body:?}");
            // What `sniff` names, `verify_download` accepts.
            assert!(verify_download(body, Some(name)).is_ok(), "{name} {body:?}");
        }
    }

    #[test]
    fn sniff_names_nothing_it_does_not_recognize() {
        for body in [
            &b""[..],
            b"   \n",
            b"\xEF\xBB\xBF",
            LOGIN_PAGE,
            b"  <HTML><body>error</body>",
            b"{\"detail\":\"Not found.\"}",
            b"1099-DIV,ACCOUNT NUMBER\n",
            &PNG[..7],
            &HEIC[..11],
            &bmff(b"avif"),
            b"%PD",
            // Markup that is not UTF-8, or that the text check reads as an
            // HTML page (any doctype), is not named `xml`.
            b"<a>\xFF\xFE\0\x01",
            b"<!DOCTYPE note SYSTEM \"note.dtd\"><note/>",
            b"\xEF\xBB\xBF <!doctype html><html>",
        ] {
            assert_eq!(sniff(body), None, "{body:?}");
        }
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
