//! Tool results: structured JSON plus the same JSON as compact text.
//!
//! `structuredContent` is what Claude Code forwards to the model; clients
//! that only read `content` get the identical JSON as one text block. Both
//! come from one value, so they cannot disagree.

use serde::Serialize;
use serde_json::{json, Value};

use crate::error::ToolError;

/// A successful tool's result: one JSON object.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolOutput {
    structured: Value,
    text: String,
}

impl ToolOutput {
    /// Serialize `value`, which must serialize to a JSON object (MCP's
    /// `structuredContent` is an object for every protocol version a client
    /// may speak).
    pub fn new<T: Serialize + ?Sized>(value: &T) -> Result<Self, ToolError> {
        let structured = serde_json::to_value(value)
            .map_err(|e| ToolError::other(format!("serializing the result: {e}")))?;
        Self::from_value(structured)
    }

    /// From an already built JSON object.
    pub fn from_value(structured: Value) -> Result<Self, ToolError> {
        if !structured.is_object() {
            return Err(ToolError::other(
                "a tool result must be a JSON object (a bug in the tool, not the call)",
            ));
        }
        let text = structured.to_string();
        Ok(Self { structured, text })
    }

    pub fn structured(&self) -> &Value {
        &self.structured
    }

    /// The compact JSON text.
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Size of the compact JSON in characters: the measure every size limit
    /// in this crate uses, as the hosts' own result caps do.
    pub fn size(&self) -> usize {
        self.text.chars().count()
    }
}

/// What `tools/call` returns: MCP's `CallToolResult`, success or error.
#[derive(Debug, Clone, PartialEq)]
pub struct CallToolResult {
    structured: Option<Value>,
    text: String,
    is_error: bool,
}

impl CallToolResult {
    pub fn success(output: ToolOutput) -> Self {
        Self {
            structured: Some(output.structured),
            text: output.text,
            is_error: false,
        }
    }

    /// An error the model reads and acts on (`isError: true`). It carries no
    /// `structuredContent`: the tool's output schema describes success only,
    /// and a client validating against it must not reject the error.
    pub fn error(text: impl Into<String>) -> Self {
        Self {
            structured: None,
            text: text.into(),
            is_error: true,
        }
    }

    pub fn is_error(&self) -> bool {
        self.is_error
    }

    /// Any combination, including inconsistent ones, for the conformance
    /// checks' own tests.
    #[cfg(test)]
    pub(crate) fn from_parts(structured: Option<Value>, text: &str, is_error: bool) -> Self {
        Self {
            structured,
            text: text.into(),
            is_error,
        }
    }

    pub fn structured(&self) -> Option<&Value> {
        self.structured.as_ref()
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    /// The MCP JSON object:
    /// `{"content":[{"type":"text","text":…}],"structuredContent":{…},"isError":false}`.
    ///
    /// Version-specific envelope fields (`resultType` from 2026-07-28,
    /// `_meta` server identity) are the transport's to add for the version
    /// it negotiated.
    pub fn to_json(&self) -> Value {
        let mut v = json!({
            "content": [{"type": "text", "text": self.text}],
        });
        if let Some(s) = &self.structured {
            v["structuredContent"] = s.clone();
        }
        v["isError"] = Value::Bool(self.is_error);
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_is_the_structured_json_compacted() {
        let out = ToolOutput::new(&json!({"items": [1, 2], "returned": 2})).unwrap();
        assert_eq!(out.text(), r#"{"items":[1,2],"returned":2}"#);
        assert_eq!(out.size(), out.text().chars().count());
        let r = CallToolResult::success(out).to_json();
        assert_eq!(r["isError"], false);
        assert_eq!(r["content"][0]["type"], "text");
        let reparsed: Value = serde_json::from_str(r["content"][0]["text"].as_str().unwrap())
            .expect("the text block is JSON");
        assert_eq!(reparsed, r["structuredContent"]);
    }

    #[test]
    fn a_result_must_be_an_object() {
        assert!(ToolOutput::new(&json!([1, 2])).is_err());
        assert!(ToolOutput::new(&json!("x")).is_err());
    }

    #[test]
    fn errors_carry_text_only() {
        let r = CallToolResult::error("auth: nope.").to_json();
        assert_eq!(r["isError"], true);
        assert!(r.get("structuredContent").is_none());
        assert_eq!(r["content"][0]["text"], "auth: nope.");
    }
}
