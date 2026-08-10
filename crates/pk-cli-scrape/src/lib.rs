//! Minimal HTML scanning for server-rendered provider pages.
//!
//! Plenty of the portals this CLI family talks to publish no API. Some of what
//! they expose answers in JSON; the rest is a server-rendered page, and the
//! data has to come out of the markup.
//!
//! This crate is deliberately a set of small string scanners rather than a DOM
//! parser. Provider pages come from one templating engine each, with stable,
//! boring markup, and the handful of shapes a CLI actually reads — an
//! attribute off an `<option>`, the cells of a row, a hidden input's value —
//! is far less code than a parser and its dependency tree. This crate has **no
//! dependencies at all.**
//!
//! # What this is not
//!
//! Not a general-purpose HTML parser: no selector engine, no document tree, no
//! recovery from exotic markup. If a page needs real parsing, use a real
//! parser. This is for reading a known field out of known markup.
//!
//! # Totality
//!
//! Every function here is total — malformed input yields `None` or an empty
//! list, never a panic. That is the property that matters in a CLI, because a
//! provider redesign should surface as an empty table the user can report, not
//! a crash.
//!
//! # Policy stays with the caller
//!
//! *Which* element IDs and attribute names to look for is provider knowledge
//! and belongs to the CLI that owns that provider. This crate only knows how
//! to look.
//!
//! ```
//! use pk_cli_scrape as scrape;
//! let page = r#"<select id="acct"><option value="1">Checking</option></select>"#;
//! let select = scrape::block_by_id(page, "select", "acct").unwrap();
//! let options = scrape::elements(&select, "option");
//! assert_eq!(scrape::attr(&options[0].tag, "value").as_deref(), Some("1"));
//! assert_eq!(options[0].text, "Checking");
//! ```

/// One matched element: its opening tag, and its decoded inner text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Element {
    /// The opening tag verbatim, e.g. `<option value="1" attr_Foo="bar">`.
    pub tag: String,
    /// Inner text with nested tags stripped and entities decoded.
    pub text: String,
}

/// Find every `<name ...>…</name>` element at any depth.
///
/// Nesting-aware: an outer `<div>` containing inner `<div>`s yields the outer
/// element with all inner markup stripped from its text, plus each inner one.
pub fn elements(html: &str, name: &str) -> Vec<Element> {
    raw_blocks(html, name)
        .into_iter()
        .map(|block| {
            let tag_end = block.find('>').map(|i| i + 1).unwrap_or(block.len());
            let close = format!("</{name}");
            let body_end = block.rfind(&close).unwrap_or(block.len());
            Element {
                tag: block[..tag_end].to_string(),
                text: strip_tags(&block[tag_end.min(body_end)..body_end]),
            }
        })
        .collect()
}

/// Raw markup (opening tag through closing tag) of every `<name>` block.
/// Self-closing and unclosed tags yield the opening tag alone.
fn raw_blocks(html: &str, name: &str) -> Vec<String> {
    let open = format!("<{name}");
    let close = format!("</{name}");
    let mut out = Vec::new();
    let mut search = 0usize;
    while let Some(rel) = html[search..].find(&open) {
        let start = search + rel;
        search = start + open.len();
        // Require a tag boundary so `<div` doesn't match `<divider`.
        match html[start + open.len()..].chars().next() {
            Some(c) if c == '>' || c.is_whitespace() || c == '/' => {}
            _ => continue,
        }
        let Some(tag_end) = html[start..].find('>').map(|i| start + i + 1) else {
            break;
        };
        if html[start..tag_end].ends_with("/>") {
            out.push(html[start..tag_end].to_string());
            continue;
        }
        match inner_text_end(html, tag_end, name) {
            Some(end) => out.push(html[start..end + close.len() + 1].to_string()),
            // Unclosed: keep the opening tag so attributes remain readable.
            None => out.push(html[start..tag_end].to_string()),
        }
    }
    out
}

/// Byte offset of the closing tag that balances the element opened before
/// `from`, accounting for same-name elements nested inside it.
fn inner_text_end(html: &str, from: usize, name: &str) -> Option<usize> {
    let open = format!("<{name}");
    let close = format!("</{name}");
    let mut depth = 1usize;
    let mut i = from;
    loop {
        let next_open = html[i..].find(&open).map(|p| i + p);
        let next_close = html[i..].find(&close).map(|p| i + p);
        match (next_open, next_close) {
            (_, None) => return None,
            (Some(o), Some(c)) if o < c => {
                depth += 1;
                i = o + open.len();
            }
            (_, Some(c)) => {
                depth -= 1;
                if depth == 0 {
                    return Some(c);
                }
                i = c + close.len();
            }
        }
    }
}

/// Raw markup of the `<name id="…">` block with the given id, for scoping a
/// search to one region of a large page.
pub fn block_by_id(html: &str, name: &str, id: &str) -> Option<String> {
    raw_blocks(html, name).into_iter().find(|b| {
        let tag_end = b.find('>').map(|i| i + 1).unwrap_or(b.len());
        attr(&b[..tag_end], "id").as_deref() == Some(id)
    })
}

/// Read an attribute out of an opening tag. Attribute names are matched
/// case-insensitively: templating engines emit mixed casing (`attr_MgCoId`,
/// `paymentType`), and it drifts between pages of the same site.
pub fn attr(tag: &str, name: &str) -> Option<String> {
    let lower = tag.to_ascii_lowercase();
    let needle = format!("{}=\"", name.to_ascii_lowercase());
    let mut from = 0usize;
    while let Some(rel) = lower[from..].find(&needle) {
        let at = from + rel;
        from = at + needle.len();
        // Only match at an attribute boundary, so a lookup for `attr_Member`
        // doesn't match `attr_AltMemberId`.
        let boundary = lower[..at]
            .chars()
            .next_back()
            .is_some_and(|c| c.is_whitespace());
        if !boundary {
            continue;
        }
        let rest = &tag[from..];
        let end = rest.find('"')?;
        return Some(decode_entities(&rest[..end]));
    }
    None
}

/// The `value` of an `<input>` carrying the given `id`.
pub fn input_value(html: &str, id: &str) -> Option<String> {
    elements(html, "input")
        .into_iter()
        .find(|e| attr(&e.tag, "id").as_deref() == Some(id))
        .and_then(|e| attr(&e.tag, "value"))
}

/// Raw markup of every `<tr>` in the document.
///
/// For providers that build tables out of `<div>`s instead — a common CSS-grid
/// pattern — pair this with [`blocks_with_class`], since the class name that
/// marks a row is the provider's, not this crate's.
pub fn table_rows(html: &str) -> Vec<String> {
    raw_blocks(html, "tr")
}

/// Raw markup of every `<name class="… class …">` block.
///
/// The class is matched as a whole word among the attribute's classes, so
/// `divTableRow` does not match `divTableRowHeader`.
pub fn blocks_with_class(html: &str, name: &str, class: &str) -> Vec<String> {
    raw_blocks(html, name)
        .into_iter()
        .filter(|b| has_class(b, class))
        .collect()
}

/// Text of each `<td>` in a row.
pub fn cells(row: &str) -> Vec<String> {
    elements(row, "td").into_iter().map(|e| e.text).collect()
}

/// Text of each `<name class="… class …">` inside a row, for div-built tables.
pub fn cells_with_class(row: &str, name: &str, class: &str) -> Vec<String> {
    elements(row, name)
        .into_iter()
        .filter(|e| has_class(&e.tag, class))
        .map(|e| e.text)
        .collect()
}

/// Whether a block's opening tag carries the given class.
fn has_class(block: &str, class: &str) -> bool {
    let tag_end = block.find('>').map(|i| i + 1).unwrap_or(block.len());
    attr(&block[..tag_end], "class").is_some_and(|c| c.split_whitespace().any(|w| w == class))
}

/// Remove tags and collapse whitespace, leaving decoded text.
///
/// Each tag becomes a space rather than vanishing, so `<p>a</p><p>b</p>` reads
/// as "a b" and not "ab" — providers rely on block elements for word
/// separation in message bodies and table cells.
pub fn strip_tags(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    for c in html.chars() {
        match c {
            '<' => {
                in_tag = true;
                out.push(' ');
            }
            '>' => in_tag = false,
            c if !in_tag => out.push(c),
            _ => {}
        }
    }
    decode_entities(&out)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// How far past an `&` to look for the closing `;`. The longest entity this
/// decodes is `&#NNNNNN;`, so a dozen bytes is generous.
const ENTITY_SCAN_BYTES: usize = 12;

/// Decode the entities that appear in practice in provider markup.
pub fn decode_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        // Scan by char, not by byte: an entity is short, but the text after
        // `&` is arbitrary provider content, and slicing to a fixed byte
        // offset panics when a multi-byte character straddles it.
        let semi = rest
            .char_indices()
            .take_while(|(i, _)| *i < ENTITY_SCAN_BYTES)
            .find(|(_, c)| *c == ';')
            .map(|(i, _)| i);
        let Some(semi) = semi else {
            out.push('&');
            rest = &rest[1..];
            continue;
        };
        let decoded = match &rest[1..semi] {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            // A non-breaking space decodes to a plain one: it is layout in
            // the markup, and callers want a word separator.
            "nbsp" | "#160" => Some(' '),
            e => e
                .strip_prefix('#')
                .and_then(|n| n.parse::<u32>().ok())
                .and_then(char::from_u32),
        };
        match decoded {
            Some(c) => {
                out.push(c);
                rest = &rest[semi + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attributes_read_off_an_option_tag() {
        let tag =
            r#"<option value="1111111" attr_Balance="0" attr_MgCoId="9001" attr_AssocId="SA">"#;
        assert_eq!(attr(tag, "value").as_deref(), Some("1111111"));
        assert_eq!(attr(tag, "attr_MgCoId").as_deref(), Some("9001"));
        // Case-insensitive, matching Razor's inconsistent casing.
        assert_eq!(attr(tag, "attr_mgcoid").as_deref(), Some("9001"));
        assert_eq!(attr(tag, "missing"), None);
    }

    #[test]
    fn attribute_lookup_respects_name_boundaries() {
        let tag = r#"<option attr_AltMemberId="999" attr_MemberId="222222">"#;
        assert_eq!(attr(tag, "attr_MemberId").as_deref(), Some("222222"));
        assert_eq!(attr(tag, "attr_AltMemberId").as_deref(), Some("999"));
        // A suffix of a longer attribute name must not match it.
        assert_eq!(attr(tag, "MemberId"), None);
    }

    #[test]
    fn elements_extract_tag_and_text() {
        let html = r#"<select><option value="1"> 1 Sample St </option><option value="2">Other</option></select>"#;
        let opts = elements(html, "option");
        assert_eq!(opts.len(), 2);
        assert_eq!(attr(&opts[0].tag, "value").as_deref(), Some("1"));
        // Inner text is whitespace-collapsed and trimmed.
        assert_eq!(opts[0].text, "1 Sample St");
        assert_eq!(opts[1].text, "Other");
    }

    #[test]
    fn element_matching_requires_a_tag_boundary() {
        let html = "<divider>no</divider><div>yes</div>";
        let found = elements(html, "div");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].text, "yes");
    }

    #[test]
    fn nested_same_name_elements_balance() {
        let html = r#"<div class="outer"><div class="inner">deep</div>shallow</div>"#;
        let divs = elements(html, "div");
        assert_eq!(divs.len(), 2);
        // The outer div's text includes the inner div's, tags stripped.
        assert_eq!(divs[0].text, "deep shallow");
        assert_eq!(divs[1].text, "deep");
    }

    #[test]
    fn self_closing_tags_have_no_text() {
        let found = elements(r#"<input id="a" value="v" /><input id="b">"#, "input");
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].text, "");
        assert_eq!(attr(&found[0].tag, "value").as_deref(), Some("v"));
    }

    #[test]
    fn blocks_can_be_scoped_by_id() {
        let html = r#"<select id="idPropertyMember"><option value="p1">P</option></select>
                      <select id="idPaymentMethod"><option value="m1">M</option></select>"#;
        let props = block_by_id(html, "select", "idPropertyMember").expect("select found");
        let opts = elements(&props, "option");
        assert_eq!(opts.len(), 1);
        assert_eq!(attr(&opts[0].tag, "value").as_deref(), Some("p1"));
        assert!(block_by_id(html, "select", "nope").is_none());
    }

    #[test]
    fn cells_read_a_div_rendered_row() {
        // The class names are the provider's; the caller passes them in.
        let row = r##"<div class="tableRow" role="row">
            <div class="tableCell" role="cell">08/01/2026</div>
            <div class="tableCell" role="cell"><a href="#">Confirmation of Payment</a></div>
            <div class="tableCell" role="cell"></div>
        </div>"##;
        assert_eq!(
            cells_with_class(row, "div", "tableCell"),
            vec!["08/01/2026", "Confirmation of Payment", ""]
        );
    }

    #[test]
    fn cells_read_a_table_rendered_row() {
        let row = r#"<tr id="1"><td>08/01/2026</td><td class="hidden-xs"> 1 Sample St </td><td>$100.00</td></tr>"#;
        assert_eq!(cells(row), vec!["08/01/2026", "1 Sample St", "$100.00"]);
    }

    #[test]
    fn table_rows_and_class_blocks_cover_both_dialects() {
        let html = r#"<table><tbody><tr><td>a</td></tr></tbody></table>
                      <div class="tableBody"><div class="tableRow"><div class="tableCell">b</div></div></div>"#;
        let tr = table_rows(html);
        assert_eq!(tr.len(), 1);
        assert_eq!(cells(&tr[0]), vec!["a"]);

        let divs = blocks_with_class(html, "div", "tableRow");
        assert_eq!(divs.len(), 1);
        assert_eq!(cells_with_class(&divs[0], "div", "tableCell"), vec!["b"]);
    }

    /// A class match is a whole word, so a longer class that merely starts
    /// with the requested one must not be picked up.
    #[test]
    fn class_matching_is_not_a_prefix_match() {
        let html = r#"<div class="tableRowHeader">head</div><div class="tableRow">body</div>"#;
        let found = blocks_with_class(html, "div", "tableRow");
        assert_eq!(found.len(), 1);
        assert!(found[0].contains("body"));
        // A class among several still matches.
        let multi = r#"<div class="nowrap tableRow alt">x</div>"#;
        assert_eq!(blocks_with_class(multi, "div", "tableRow").len(), 1);
    }

    #[test]
    fn hidden_input_values_are_found_by_id() {
        let html =
            r#"<input type="hidden" id="idSiteUserLogin" name="SiteUserLogin" value="7654321" />"#;
        assert_eq!(
            input_value(html, "idSiteUserLogin").as_deref(),
            Some("7654321")
        );
        assert_eq!(input_value(html, "nope"), None);
    }

    #[test]
    fn entities_decode() {
        assert_eq!(decode_entities("a&amp;b"), "a&b");
        assert_eq!(decode_entities("x&#160;y"), "x y");
        assert_eq!(decode_entities("plain"), "plain");
        // A bare ampersand is left alone rather than eating the rest.
        assert_eq!(decode_entities("Q&A now"), "Q&A now");
    }

    #[test]
    fn strip_tags_collapses_whitespace() {
        assert_eq!(
            strip_tags("<p>Dear  Caleb,</p>\n<p>Hi</p>"),
            "Dear Caleb, Hi"
        );
        assert_eq!(strip_tags(""), "");
    }

    /// The crate promises totality, and the byte-slicing lookahead in
    /// `decode_entities` broke it: a multi-byte character straddling the scan
    /// cap panicked with "not a char boundary". Provider pages carry accented
    /// names and addresses, so this is ordinary input, not a fuzz case.
    #[test]
    fn multibyte_text_near_an_ampersand_does_not_panic() {
        // `&` + 10 ASCII digits puts `é` across the 12-byte cap exactly.
        assert_eq!(decode_entities("&0123456789\u{e9};"), "&0123456789\u{e9};");
        // A multi-byte char at every offset around the cap.
        for pad in 0..16 {
            let s = format!("&{}\u{e9};x", "0".repeat(pad));
            let _ = decode_entities(&s);
            let _ = strip_tags(&s);
        }
        // Real-world shapes: an accented name beside a literal ampersand.
        assert_eq!(decode_entities("Ren\u{e9} & Co"), "Ren\u{e9} & Co");
        assert_eq!(decode_entities("caf\u{e9}&amp;bar"), "caf\u{e9}&bar");
        // Multi-byte inside what looks like an entity.
        assert_eq!(decode_entities("&\u{4e2d}\u{6587};"), "&\u{4e2d}\u{6587};");
    }

    /// A well-formed entity must still decode when multi-byte text surrounds
    /// it — the fix must not narrow the scan so far that real entities miss.
    #[test]
    fn entities_still_decode_amid_multibyte_text() {
        assert_eq!(decode_entities("\u{e9}&nbsp;\u{e9}"), "\u{e9} \u{e9}");
        assert_eq!(decode_entities("&#160;\u{4e2d}"), " \u{4e2d}");
        // The longest form this decodes, unaffected by the cap.
        assert_eq!(decode_entities("&#128512;"), "\u{1f600}");
    }

    #[test]
    fn malformed_markup_yields_data_rather_than_panicking() {
        // An unclosed tag still surfaces its attributes, and nothing panics.
        let found = elements(r#"<option value="1">unterminated"#, "option");
        assert_eq!(found.len(), 1);
        assert_eq!(attr(&found[0].tag, "value").as_deref(), Some("1"));
        assert_eq!(attr("<option value=\"unterminated>", "value"), None);
        assert!(cells("<tr>no cells</tr>").is_empty());
    }
}
