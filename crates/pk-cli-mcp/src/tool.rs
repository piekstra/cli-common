//! Tool definitions: what `tools/list` says about each tool.
//!
//! A [`ToolDef`] is built with [`ToolDef::read`] or [`ToolDef::write`], which
//! set every annotation explicitly (MCP's defaults are `readOnlyHint: false`
//! and `destructiveHint: true`, the opposite of what a read wants), then given
//! typed input and output through [`ToolDef::input`] and [`ToolDef::output`].
//! The schemas come from `schemars` derives on the CLI's argument and result
//! structs, compacted for the model: subschemas inlined, `$schema` and Rust
//! integer formats dropped, and the `null` half of an optional field folded
//! away. An output schema also loses its descriptions, because Claude Code
//! does not show output schemas to the model: the returned fields are
//! described in the tool's description instead.

use schemars::generate::SchemaSettings;
use schemars::JsonSchema;
use serde_json::{json, Map, Value};

/// The longest tool name the Claude API accepts.
pub const NAME_MAX: usize = 64;

/// The `_meta` key that makes Claude ask the user before every call.
pub const REQUIRES_USER_INTERACTION: &str = "anthropic/requiresUserInteraction";

/// The behavior hints a client may use to decide how to run a tool. All four
/// are always sent: a client that finds one missing assumes the unsafe value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Annotations {
    /// The tool changes nothing. Claude Code runs read-only tools in parallel.
    pub read_only: bool,
    /// A write may destroy or overwrite data (only meaningful when not
    /// read-only).
    pub destructive: bool,
    /// Repeating a call with the same arguments has no further effect.
    pub idempotent: bool,
    /// The tool reaches entities beyond a closed domain (a web search, say).
    /// A read of the user's own account at one provider is a closed domain.
    pub open_world: bool,
}

/// One tool, as `tools/list` describes it.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolDef {
    /// `<service>_<what>`, at most [`NAME_MAX`] characters of `[a-z0-9_]`.
    pub name: String,
    /// A short human-readable title, shown in client UIs.
    pub title: String,
    /// What the tool does, when to use it, what it returns and how to narrow
    /// it: at least three sentences.
    pub description: String,
    /// JSON Schema of the arguments (always `type: object`).
    pub input_schema: Value,
    /// JSON Schema of `structuredContent` (always `type: object`).
    pub output_schema: Option<Value>,
    pub annotations: Annotations,
    /// `_meta` entries; [`ToolDef::write`] sets [`REQUIRES_USER_INTERACTION`].
    pub meta: Map<String, Value>,
}

impl ToolDef {
    /// A tool that only reads: `readOnlyHint: true`, `destructiveHint:
    /// false`, `idempotentHint: true`, `openWorldHint: false`.
    pub fn read(
        name: impl Into<String>,
        title: impl Into<String>,
        description: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            title: title.into(),
            description: description.into(),
            input_schema: empty_object_schema(),
            output_schema: None,
            annotations: Annotations {
                read_only: true,
                destructive: false,
                idempotent: true,
                open_world: false,
            },
            meta: Map::new(),
        }
    }

    /// A tool that changes something. It is never read-only, says whether it
    /// can destroy or overwrite data, and carries
    /// `anthropic/requiresUserInteraction: true`, so the user approves every
    /// call. One write per tool: a tool that both reads and writes, or that
    /// moves money, is refused by the connector directory.
    pub fn write(
        name: impl Into<String>,
        title: impl Into<String>,
        description: impl Into<String>,
        destructive: bool,
    ) -> Self {
        let mut meta = Map::new();
        meta.insert(REQUIRES_USER_INTERACTION.into(), Value::Bool(true));
        Self {
            annotations: Annotations {
                read_only: false,
                destructive,
                idempotent: false,
                open_world: false,
            },
            meta,
            ..Self::read(name, title, description)
        }
    }

    /// The arguments, from the `schemars` derive on `T`.
    pub fn input<T: JsonSchema>(mut self) -> Self {
        self.input_schema = input_schema_for::<T>();
        self
    }

    /// The `structuredContent` shape, from the `schemars` derive on `T`.
    pub fn output<T: JsonSchema>(mut self) -> Self {
        self.output_schema = Some(output_schema_for::<T>());
        self
    }

    pub fn idempotent(mut self, idempotent: bool) -> Self {
        self.annotations.idempotent = idempotent;
        self
    }

    pub fn open_world(mut self, open_world: bool) -> Self {
        self.annotations.open_world = open_world;
        self
    }

    /// Add a `_meta` entry (reverse-DNS or `vendor/` namespaced keys).
    pub fn meta(mut self, key: impl Into<String>, value: Value) -> Self {
        self.meta.insert(key.into(), value);
        self
    }

    /// Whether the user must approve each call.
    pub fn requires_user_interaction(&self) -> bool {
        self.meta.get(REQUIRES_USER_INTERACTION) == Some(&Value::Bool(true))
    }

    /// The argument names the input schema declares.
    pub fn input_properties(&self) -> Vec<&str> {
        self.input_schema
            .get("properties")
            .and_then(Value::as_object)
            .map(|p| p.keys().map(String::as_str).collect())
            .unwrap_or_default()
    }

    /// The `tools/list` entry, exactly as MCP spells it.
    pub fn to_json(&self) -> Value {
        let mut tool = Map::new();
        tool.insert("name".into(), json!(self.name));
        tool.insert("title".into(), json!(self.title));
        tool.insert("description".into(), json!(self.description));
        tool.insert("inputSchema".into(), self.input_schema.clone());
        if let Some(out) = &self.output_schema {
            tool.insert("outputSchema".into(), out.clone());
        }
        tool.insert(
            "annotations".into(),
            json!({
                "title": self.title,
                "readOnlyHint": self.annotations.read_only,
                "destructiveHint": self.annotations.destructive,
                "idempotentHint": self.annotations.idempotent,
                "openWorldHint": self.annotations.open_world,
            }),
        );
        if !self.meta.is_empty() {
            tool.insert("_meta".into(), Value::Object(self.meta.clone()));
        }
        Value::Object(tool)
    }
}

fn empty_object_schema() -> Value {
    json!({"type": "object", "properties": {}})
}

/// Which side of the wire a schema describes.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Side {
    /// What the tool accepts: schemars' deserialize contract, descriptions
    /// kept, `null` folded out everywhere (an optional argument is simply
    /// left out).
    Input,
    /// What the tool returns: the serialize contract, so a field with
    /// `skip_serializing_if` is optional and a plain `Option` is a required,
    /// nullable field, exactly as `serde_json` writes the value. Descriptions
    /// are dropped (the tool description names the fields), and `null` is
    /// folded out only where the field is optional and so absent rather than
    /// `null`.
    Output,
}

/// The compacted input schema for `T`.
pub fn input_schema_for<T: JsonSchema>() -> Value {
    let mut schema = raw_schema_for::<T>(Side::Input);
    compact(&mut schema, Side::Input, true);
    ensure_object(schema)
}

/// The compacted output schema for `T`: the shape `serde_json` gives a `T`.
pub fn output_schema_for<T: JsonSchema>() -> Value {
    let mut schema = raw_schema_for::<T>(Side::Output);
    compact(&mut schema, Side::Output, false);
    ensure_object(schema)
}

fn raw_schema_for<T: JsonSchema>(side: Side) -> Value {
    SchemaSettings::draft2020_12()
        .with(|s| {
            s.inline_subschemas = true;
            s.meta_schema = None;
            if side == Side::Output {
                s.contract = schemars::generate::Contract::Serialize;
            }
        })
        .into_generator()
        .into_root_schema_for::<T>()
        .to_value()
}

/// MCP requires `type: object` at the root of both schemas. A unit struct or
/// a struct with no fields still produces an object schema. A root
/// `oneOf`/`anyOf`/`allOf` (an enum, or a struct flattening one) is left as
/// it is: the profile takes flat structs only, and the conformance check
/// reports anything else instead of this papering over it.
fn ensure_object(mut schema: Value) -> Value {
    if let Some(obj) = schema.as_object_mut() {
        // The root's description is the struct's doc comment, written for
        // Rust readers (and, through `#[serde(flatten)]`, sometimes a
        // flattened struct's). The tool's own description speaks to the
        // model instead.
        obj.remove("description");
        if has_root_combinator(obj) {
            return schema;
        }
        obj.entry("type").or_insert_with(|| json!("object"));
        if obj.get("type") == Some(&json!("object")) {
            obj.entry("properties").or_insert_with(|| json!({}));
        }
    }
    schema
}

/// The schema's root combines alternatives (`oneOf`/`anyOf`/`allOf`), which
/// the profile does not support: dispatch checks arguments against the root
/// `properties` alone.
pub(crate) fn has_root_combinator(obj: &Map<String, Value>) -> bool {
    ["oneOf", "anyOf", "allOf"]
        .iter()
        .any(|k| obj.contains_key(*k))
}

const RUST_FORMATS: &[&str] = &[
    "int", "int8", "int16", "int32", "int64", "int128", "uint", "uint8", "uint16", "uint32",
    "uint64", "uint128", "float", "double",
];

/// Shrink a schemars schema to what a model needs, recursively. `fold_null`
/// says whether this schema may lose its `null` alternative: always for
/// input, and for output only where the property is not `required`.
fn compact(v: &mut Value, side: Side, fold_null: bool) {
    let strip_descriptions = side == Side::Output;
    match v {
        Value::Object(obj) => {
            obj.remove("$schema");
            obj.remove("title");
            if obj.get("default") == Some(&Value::Null) {
                obj.remove("default");
            }
            // Only the keyword, never a property that happens to be named
            // `description` (property maps are recursed into below, not
            // treated as schemas).
            if let Some(Value::String(d)) = obj.get("description") {
                if strip_descriptions {
                    obj.remove("description");
                } else {
                    // Doc comments wrap at 80 columns; the model reads prose.
                    let flat = d.split_whitespace().collect::<Vec<_>>().join(" ");
                    obj.insert("description".into(), Value::String(flat));
                }
            }
            if obj
                .get("format")
                .and_then(Value::as_str)
                .is_some_and(|f| RUST_FORMATS.contains(&f))
            {
                obj.remove("format");
            }
            if fold_null {
                fold_nullable_type(obj);
                fold_nullable_any_of(obj);
            }
            let required: Vec<String> = obj
                .get("required")
                .and_then(Value::as_array)
                .map(|r| {
                    r.iter()
                        .filter_map(Value::as_str)
                        .map(String::from)
                        .collect()
                })
                .unwrap_or_default();
            let fold_child = |name: Option<&str>| match side {
                Side::Input => true,
                Side::Output => name.is_some_and(|n| !required.iter().any(|r| r == n)),
            };
            for (key, child) in obj.iter_mut() {
                // Values, not schemas: never rewrite data.
                if matches!(key.as_str(), "enum" | "const" | "default" | "examples") {
                    continue;
                }
                // Property names are data, not keywords: recurse into each
                // property's schema without treating the map as a schema.
                if key == "properties" {
                    if let Value::Object(props) = child {
                        for (name, prop) in props.iter_mut() {
                            compact(prop, side, fold_child(Some(name)));
                        }
                    }
                } else if key == "$defs" {
                    if let Value::Object(defs) = child {
                        for def in defs.values_mut() {
                            compact(def, side, fold_child(None));
                        }
                    }
                } else {
                    compact(child, side, fold_child(None));
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                compact(item, side, fold_null);
            }
        }
        _ => {}
    }
}

/// `"type": ["integer", "null"]` → `"type": "integer"`, for a field that is
/// optional by being absent from `required`: there the `null` only costs
/// characters.
fn fold_nullable_type(obj: &mut Map<String, Value>) {
    let Some(Value::Array(types)) = obj.get("type") else {
        return;
    };
    let non_null: Vec<Value> = types
        .iter()
        .filter(|t| t.as_str() != Some("null"))
        .cloned()
        .collect();
    if non_null.len() == 1 && types.len() == 2 {
        obj.insert("type".into(), non_null[0].clone());
        // An optional enum also lists `null` among its values.
        if let Some(Value::Array(values)) = obj.get_mut("enum") {
            values.retain(|v| !v.is_null());
        }
    }
}

/// `"anyOf": [{…}, {"type": "null"}]` → the `{…}` merged in place, for the
/// same reason.
fn fold_nullable_any_of(obj: &mut Map<String, Value>) {
    let Some(Value::Array(alts)) = obj.get("anyOf") else {
        return;
    };
    if alts.len() != 2 {
        return;
    }
    let is_null = |v: &Value| v == &json!({"type": "null"});
    let keep = match (is_null(&alts[0]), is_null(&alts[1])) {
        (true, false) => alts[1].clone(),
        (false, true) => alts[0].clone(),
        _ => return,
    };
    let Value::Object(inner) = keep else {
        return;
    };
    obj.remove("anyOf");
    for (k, v) in inner {
        // The outer description (the field's doc comment) wins over the
        // inner type's.
        obj.entry(k).or_insert(v);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};

    /// Rust-facing notes that must not reach the model.
    #[allow(dead_code)]
    #[derive(Deserialize, JsonSchema)]
    struct Args {
        /// Which account,
        /// wrapped across lines.
        account: String,
        format: Option<crate::shape::ResponseFormat>,
        /// How many.
        #[schemars(range(min = 1, max = 200))]
        limit: Option<u32>,
        nested: Option<Inner>,
    }

    #[allow(dead_code)]
    #[derive(Serialize, Deserialize, JsonSchema)]
    struct Inner {
        /// A note.
        description: Option<String>,
    }

    #[test]
    fn input_schemas_are_compacted() {
        let s = input_schema_for::<Args>();
        assert_eq!(s["type"], "object");
        assert!(s.get("$schema").is_none());
        assert!(s.get("title").is_none());
        assert!(s.get("description").is_none(), "the doc comment stays out");
        assert_eq!(s["required"], json!(["account"]));
        let limit = &s["properties"]["limit"];
        assert_eq!(limit["type"], "integer", "nullable folded: {limit}");
        assert!(limit.get("format").is_none());
        assert_eq!(limit["minimum"], 1);
        assert_eq!(limit["maximum"], 200);
        assert_eq!(limit["description"], "How many.");
        // Option<struct> is inlined and its null alternative folded away.
        let format = &s["properties"]["format"];
        assert_eq!(format["type"], "string", "{format}");
        assert_eq!(format["enum"], json!(["concise", "detailed"]));
        assert_eq!(
            s["properties"]["account"]["description"],
            "Which account, wrapped across lines."
        );
        let nested = &s["properties"]["nested"];
        assert_eq!(nested["type"], "object", "{nested}");
        assert!(nested.get("anyOf").is_none());
        assert!(s.to_string().find("$ref").is_none());
    }

    /// A plain `Option` serializes as `null` and an empty `Vec` with
    /// `skip_serializing_if` is left out: the output schema must accept both
    /// exactly as `serde_json` writes them.
    #[test]
    fn output_schemas_describe_what_serde_writes() {
        #[derive(Serialize, JsonSchema)]
        struct Row {
            name: String,
            memo: Option<String>,
            #[serde(skip_serializing_if = "Option::is_none")]
            note: Option<String>,
            #[serde(skip_serializing_if = "Vec::is_empty")]
            tags: Vec<String>,
            kind: Option<crate::shape::ResponseFormat>,
        }
        let s = output_schema_for::<Row>();
        let required: Vec<&str> = s["required"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
            .collect();
        assert!(
            required.contains(&"memo") && !required.contains(&"tags"),
            "{s}"
        );
        assert!(!required.contains(&"note"), "{s}");
        assert_eq!(
            s["properties"]["note"]["type"], "string",
            "optional: null folded"
        );
        for value in [
            Row {
                name: "n".into(),
                memo: None,
                note: None,
                tags: vec![],
                kind: None,
            },
            Row {
                name: "n".into(),
                memo: Some("m".into()),
                note: Some("x".into()),
                tags: vec!["t".into()],
                kind: Some(crate::shape::ResponseFormat::Detailed),
            },
        ] {
            let v = serde_json::to_value(&value).unwrap();
            let violations = crate::conformance::schema_violations(&s, &v);
            assert!(violations.is_empty(), "{v}: {violations:?}\n{s}");
        }
    }

    /// An enum (or a struct flattening one) is not a flat object: the root
    /// keeps its alternatives and gains no `type: object` to hide them.
    #[test]
    fn a_root_enum_is_left_for_conformance_to_report() {
        #[allow(dead_code)]
        #[derive(Deserialize, JsonSchema)]
        #[serde(tag = "by", rename_all = "lowercase")]
        enum Lookup {
            /// By id.
            Id { id: String },
            /// By name.
            Name { name: String },
        }
        let s = input_schema_for::<Lookup>();
        assert!(s.get("oneOf").is_some(), "{s}");
        assert!(s.get("type").is_none(), "{s}");
    }

    #[test]
    fn output_schemas_lose_descriptions_but_keep_a_property_named_description() {
        let s = output_schema_for::<Inner>();
        let prop = &s["properties"]["description"];
        // A plain Option: serialized as null, so required and nullable.
        assert_eq!(prop["type"], json!(["string", "null"]), "{s}");
        assert!(prop.get("description").is_none());
    }

    #[test]
    fn read_and_write_set_every_annotation() {
        let r = ToolDef::read("svc_list", "List", "d").to_json();
        assert_eq!(r["annotations"]["readOnlyHint"], true);
        assert_eq!(r["annotations"]["destructiveHint"], false);
        assert_eq!(r["annotations"]["idempotentHint"], true);
        assert_eq!(r["annotations"]["openWorldHint"], false);
        assert_eq!(r["annotations"]["title"], "List");
        assert!(r.get("_meta").is_none());
        assert_eq!(
            r["inputSchema"],
            json!({"type": "object", "properties": {}})
        );

        let w = ToolDef::write("svc_rename", "Rename", "d", false).to_json();
        assert_eq!(w["annotations"]["readOnlyHint"], false);
        assert_eq!(w["annotations"]["destructiveHint"], false);
        assert_eq!(w["_meta"][REQUIRES_USER_INTERACTION], true);
    }

    #[test]
    fn the_entry_uses_mcp_spellings() {
        let t = ToolDef::read("svc_list", "List", "d")
            .input::<Args>()
            .output::<Inner>()
            .to_json();
        let keys: Vec<&str> = t.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            [
                "name",
                "title",
                "description",
                "inputSchema",
                "outputSchema",
                "annotations"
            ]
        );
    }
}
