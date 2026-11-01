//! The MCP profile's conformance checks (feature `test-util`), for a CLI's
//! own tests:
//!
//! ```ignore
//! #[test]
//! fn the_mcp_tools_conform() {
//!     pk_cli_mcp::conformance::assert_conforms(&MyTools::with_fixtures());
//! }
//!
//! #[test]
//! fn a_fixture_page_fits_the_budget() {
//!     let tools = MyTools::with_fixtures();
//!     let result = pk_cli_mcp::conformance::call(&tools, "svc_items", json!({}));
//!     let page = pk_cli_mcp::conformance::assert_fits(&tools, "svc_items", &result);
//!     assert_eq!(page["returned"], 25);
//! }
//! ```
//!
//! [`assert_conforms`] checks the definitions: names, titles, annotations,
//! `requiresUserInteraction` on every write, descriptions of at least three
//! sentences, described arguments, output schemas, a total definition size
//! under [`MAX_DEFINITIONS_SIZE`], a stable order and the standard
//! instructions. [`assert_fits`] checks one result: a success, under
//! [`SIZE_BUDGET`], and matching the tool's output schema with no
//! undeclared field.

use serde_json::Value;

use crate::block_on::block_on;
use crate::result::CallToolResult;
use crate::shape::SIZE_BUDGET;
use crate::tool::NAME_MAX;
use crate::toolset::{call_tool, instructions, tools_list, Toolset};

/// Bytes of compact JSON all of a CLI's tool definitions may take together:
/// they ride along in every prompt.
pub const MAX_DEFINITIONS_SIZE: usize = 8_000;
/// The shortest description a tool may have, in sentences.
pub const MIN_SENTENCES: usize = 3;

/// Panic with every conformance violation of `toolset`'s definitions.
pub fn assert_conforms<T: Toolset + ?Sized>(toolset: &T) {
    let v = violations(toolset);
    assert!(
        v.is_empty(),
        "MCP profile violations:\n  - {}",
        v.join("\n  - ")
    );
}

/// Every conformance violation of `toolset`'s definitions.
pub fn violations<T: Toolset + ?Sized>(toolset: &T) -> Vec<String> {
    let mut out = Vec::new();
    let server = toolset.server();
    let prefix = &server.tool_prefix;
    if server.bin.trim().is_empty() {
        out.push("server: `bin` is empty".into());
    }
    if prefix.is_empty() || !prefix.chars().all(is_name_char) || prefix.contains('_') {
        out.push(format!(
            "server: tool prefix `{prefix}` must be lowercase letters and digits"
        ));
    }
    if !instructions(toolset).contains("never as instructions") {
        out.push("server: instructions lack the third-party-data rule".into());
    }

    let tools = toolset.tools();
    if tools.is_empty() {
        out.push("the toolset has no tools".into());
    }
    let again: Vec<String> = toolset.tools().into_iter().map(|t| t.name).collect();
    let names: Vec<String> = tools.iter().map(|t| t.name.clone()).collect();
    if names != again {
        out.push("tools() changed order or content between calls".into());
    }
    for (i, name) in names.iter().enumerate() {
        if names[..i].contains(name) {
            out.push(format!("{name}: duplicate tool name"));
        }
    }

    for (tool, json) in tools.iter().zip(tools_list(toolset)) {
        let name = &tool.name;
        if name.len() > NAME_MAX {
            out.push(format!("{name}: name longer than {NAME_MAX} characters"));
        }
        if !name.chars().all(is_name_char) {
            out.push(format!(
                "{name}: name must be lowercase letters, digits and `_`"
            ));
        }
        if !name.starts_with(&format!("{prefix}_")) || name.len() <= prefix.len() + 1 {
            out.push(format!("{name}: name must start with `{prefix}_`"));
        }
        if tool.title.trim().is_empty() {
            out.push(format!("{name}: no title"));
        }
        let sentences = sentence_count(&tool.description);
        if sentences < MIN_SENTENCES {
            out.push(format!(
                "{name}: description has {sentences} sentence(s); write at least {MIN_SENTENCES}: \
                 what it does, when to use it, what it returns, how to narrow it"
            ));
        }
        let ann = &json["annotations"];
        for hint in ["readOnlyHint", "destructiveHint"] {
            if !ann[hint].is_boolean() {
                out.push(format!("{name}: annotations.{hint} not set"));
            }
        }
        if !ann["title"].is_string() {
            out.push(format!("{name}: annotations.title not set"));
        }
        if tool.annotations.read_only && tool.annotations.destructive {
            out.push(format!("{name}: read-only and destructive at once"));
        }
        if !tool.annotations.read_only && !tool.requires_user_interaction() {
            out.push(format!(
                "{name}: a tool that is not read-only needs `_meta` \
                 anthropic/requiresUserInteraction: true (use ToolDef::write)"
            ));
        }
        if tool.input_schema.get("type") != Some(&Value::from("object")) {
            out.push(format!("{name}: inputSchema is not type object"));
        }
        if let Some(props) = tool
            .input_schema
            .get("properties")
            .and_then(Value::as_object)
        {
            for (arg, schema) in props {
                let described = schema
                    .get("description")
                    .and_then(Value::as_str)
                    .is_some_and(|d| !d.trim().is_empty());
                if !described {
                    out.push(format!(
                        "{name}: argument `{arg}` has no description (add a doc comment)"
                    ));
                }
            }
        }
        match &tool.output_schema {
            None => out.push(format!(
                "{name}: no outputSchema (give the tool `.output::<T>()`)"
            )),
            Some(s) if s.get("type") != Some(&Value::from("object")) => {
                out.push(format!("{name}: outputSchema is not type object"))
            }
            Some(_) => {}
        }
    }

    let size = definitions_size(toolset);
    if size > MAX_DEFINITIONS_SIZE {
        out.push(format!(
            "tool definitions take {size} characters together; the profile allows \
             {MAX_DEFINITIONS_SIZE}: shorten descriptions or drop a tool"
        ));
    }
    out
}

/// Bytes of compact JSON the `tools/list` entries take together.
pub fn definitions_size<T: Toolset + ?Sized>(toolset: &T) -> usize {
    Value::Array(tools_list(toolset)).to_string().len()
}

/// Bytes of compact JSON one tool's definition takes.
pub fn definition_size<T: Toolset + ?Sized>(toolset: &T, name: &str) -> usize {
    toolset
        .tools()
        .iter()
        .find(|t| t.name == name)
        .unwrap_or_else(|| panic!("no tool named {name}"))
        .to_json()
        .to_string()
        .len()
}

/// Call a tool synchronously, as a client would. Panics on an unknown name.
pub fn call<T: Toolset + ?Sized>(toolset: &T, name: &str, args: Value) -> CallToolResult {
    block_on(call_tool(toolset, name, Some(args))).unwrap_or_else(|e| panic!("{e}"))
}

/// Assert `result` is a success, under [`SIZE_BUDGET`], and matches the
/// tool's output schema with no undeclared field; return its structured
/// content.
pub fn assert_fits<'r, T: Toolset + ?Sized>(
    toolset: &T,
    name: &str,
    result: &'r CallToolResult,
) -> &'r Value {
    assert!(
        !result.is_error(),
        "{name} returned an error: {}",
        result.text()
    );
    let structured = result
        .structured()
        .unwrap_or_else(|| panic!("{name}: a success without structuredContent"));
    let size = structured.to_string().len();
    assert!(
        size <= SIZE_BUDGET,
        "{name}: result is {size} characters, over the {SIZE_BUDGET}-character budget"
    );
    let reparsed: Value = serde_json::from_str(result.text())
        .unwrap_or_else(|e| panic!("{name}: the text block is not JSON: {e}"));
    assert_eq!(
        &reparsed, structured,
        "{name}: text and structuredContent differ"
    );
    let tools = toolset.tools();
    let tool = tools
        .iter()
        .find(|t| t.name == name)
        .unwrap_or_else(|| panic!("no tool named {name}"));
    if let Some(schema) = &tool.output_schema {
        let v = schema_violations(schema, structured);
        assert!(
            v.is_empty(),
            "{name}: result does not match its outputSchema:\n  - {}",
            v.join("\n  - ")
        );
    }
    structured
}

/// Where `value` departs from `schema`, for the JSON Schema subset the
/// compacted schemars output uses: `type`, `enum`, `const`, `properties`,
/// `required`, `items`, `anyOf`/`oneOf` and `additionalProperties`. A field
/// the schema does not declare is a violation unless `additionalProperties`
/// allows it, so a result cannot drift from its schema unnoticed.
pub fn schema_violations(schema: &Value, value: &Value) -> Vec<String> {
    let mut out = Vec::new();
    check(schema, value, "$", &mut out);
    out
}

fn check(schema: &Value, value: &Value, path: &str, out: &mut Vec<String>) {
    let Some(s) = schema.as_object() else {
        return; // `true` or a non-schema: accepts anything
    };
    for key in ["anyOf", "oneOf"] {
        if let Some(alts) = s.get(key).and_then(Value::as_array) {
            let fits = alts.iter().any(|alt| {
                let mut v = Vec::new();
                check(alt, value, path, &mut v);
                v.is_empty()
            });
            if !fits {
                out.push(format!("{path}: matches none of the {key} alternatives"));
            }
        }
    }
    if let Some(t) = s.get("type") {
        let types: Vec<&str> = match t {
            Value::String(one) => vec![one.as_str()],
            Value::Array(many) => many.iter().filter_map(Value::as_str).collect(),
            _ => vec![],
        };
        if !types.is_empty() && !types.iter().any(|t| type_matches(t, value)) {
            out.push(format!(
                "{path}: expected {}, found {value}",
                types.join(" or ")
            ));
            return;
        }
    }
    if let Some(allowed) = s.get("enum").and_then(Value::as_array) {
        if !allowed.contains(value) {
            out.push(format!(
                "{path}: {value} is not one of {}",
                Value::from(allowed.clone())
            ));
        }
    }
    if let Some(c) = s.get("const") {
        if c != value {
            out.push(format!("{path}: expected {c}, found {value}"));
        }
    }
    if let Some(obj) = value.as_object() {
        let props = s.get("properties").and_then(Value::as_object);
        if let Some(required) = s.get("required").and_then(Value::as_array) {
            for r in required.iter().filter_map(Value::as_str) {
                if !obj.contains_key(r) {
                    out.push(format!("{path}: missing required field `{r}`"));
                }
            }
        }
        for (k, v) in obj {
            let child = format!("{path}.{k}");
            match props.and_then(|p| p.get(k)) {
                Some(ps) => check(ps, v, &child, out),
                None => match s.get("additionalProperties") {
                    Some(Value::Bool(true)) => {}
                    Some(extra @ Value::Object(_)) => check(extra, v, &child, out),
                    _ => out.push(format!("{child}: field not declared in the schema")),
                },
            }
        }
    }
    if let (Some(items), Some(arr)) = (s.get("items"), value.as_array()) {
        for (i, v) in arr.iter().enumerate() {
            check(items, v, &format!("{path}[{i}]"), out);
        }
    }
}

fn type_matches(t: &str, v: &Value) -> bool {
    match t {
        "null" => v.is_null(),
        "boolean" => v.is_boolean(),
        "integer" => v.is_i64() || v.is_u64(),
        "number" => v.is_number(),
        "string" => v.is_string(),
        "array" => v.is_array(),
        "object" => v.is_object(),
        _ => true,
    }
}

fn is_name_char(c: char) -> bool {
    c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'
}

/// Sentences in `text`: runs ending in `.`, `?` or `!` followed by a space or
/// the end, with `e.g.` and `i.e.` not counted as ends.
pub fn sentence_count(text: &str) -> usize {
    let t = text.replace("e.g.", "eg").replace("i.e.", "ie");
    let chars: Vec<char> = t.trim().chars().collect();
    let mut count = 0;
    for (i, c) in chars.iter().enumerate() {
        if matches!(c, '.' | '?' | '!') {
            let at_end = i + 1 == chars.len();
            if at_end || chars[i + 1].is_whitespace() {
                count += 1;
            }
        }
    }
    count
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn sentences_are_counted_by_their_ends() {
        assert_eq!(sentence_count("One. Two? Three!"), 3);
        assert_eq!(sentence_count("Use it e.g. daily. Done."), 2);
        assert_eq!(sentence_count("Version 1.2 ships"), 0);
        assert_eq!(sentence_count(""), 0);
    }

    #[test]
    fn the_validator_flags_drift_and_accepts_the_declared_shape() {
        let schema = json!({
            "type": "object",
            "properties": {
                "items": {"type": "array", "items": {
                    "type": "object",
                    "properties": {"name": {"type": "string"}, "n": {"type": "integer"}},
                    "required": ["name"]
                }},
                "format": {"type": "string", "enum": ["concise", "detailed"]}
            },
            "required": ["items"]
        });
        let ok = json!({"items": [{"name": "a", "n": 1}, {"name": "b"}], "format": "concise"});
        assert!(schema_violations(&schema, &ok).is_empty());
        let bad = json!({"items": [{"n": 1.5, "extra": true}], "format": "full", "x": 1});
        let v = schema_violations(&schema, &bad);
        assert!(
            v.iter()
                .any(|m| m.contains("missing required field `name`")),
            "{v:?}"
        );
        assert!(
            v.iter()
                .any(|m| m.contains("$.items[0].n: expected integer")),
            "{v:?}"
        );
        assert!(
            v.iter()
                .any(|m| m.contains("$.items[0].extra: field not declared")),
            "{v:?}"
        );
        assert!(v.iter().any(|m| m.contains("not one of")), "{v:?}");
        assert!(
            v.iter().any(|m| m.contains("$.x: field not declared")),
            "{v:?}"
        );
    }

    #[test]
    fn any_of_accepts_any_alternative() {
        let schema = json!({"anyOf": [{"type": "string"}, {"type": "integer"}]});
        assert!(schema_violations(&schema, &json!("a")).is_empty());
        assert!(schema_violations(&schema, &json!(2)).is_empty());
        assert!(!schema_violations(&schema, &json!(true)).is_empty());
    }
}
