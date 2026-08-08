//! Output rendering (SPEC v1 §1.4).
//!
//! **Text is the primary format.** Resource reads render token-dense
//! `Key: value` blocks and pipe-delimited tables (`ALL_CAPS` headers). With
//! `--json`, the DTO alone goes to stdout, pretty-printed. Data goes to
//! stdout; diagnostics and confirmations go to stderr.

use serde_json::Value;

/// Emit a DTO: the `schema`-tagged payload in JSON mode, a rendered view in
/// text mode.
///
/// This is §1.4's output contract as a function. The tag is inserted first so
/// it leads the object, and an object payload is flattened *alongside* it
/// rather than nested, so consumers read `.payments` and not `.data.payments`.
/// A non-object payload (an array, a scalar) has nowhere to merge, so it lands
/// under `data`.
pub fn emit(json_mode: bool, schema: &str, payload: Value, text: impl FnOnce(&Value)) {
    if !json_mode {
        text(&payload);
        return;
    }
    let mut tagged = serde_json::Map::new();
    tagged.insert("schema".into(), Value::String(format!("{schema}/v1")));
    match payload {
        Value::Object(m) => tagged.extend(m),
        other => {
            tagged.insert("data".into(), other);
        }
    }
    json(&Value::Object(tagged));
}

/// Project selected columns out of an array of objects, for [`table`].
///
/// Absent columns are **omitted** rather than emitted as null, per §1.4's
/// omit-don't-null rule — so a row missing a field renders as a blank cell
/// instead of the literal text "null".
pub fn table_view(items: &[Value], columns: &[&str]) -> Vec<Value> {
    items
        .iter()
        .map(|item| {
            let mut row = serde_json::Map::new();
            for col in columns {
                if let Some(v) = item.get(*col) {
                    row.insert((*col).to_string(), v.clone());
                }
            }
            Value::Object(row)
        })
        .collect()
}

/// Read an array field out of a payload, for a text renderer that receives the
/// same DTO the JSON path emits.
///
/// Returns empty for a missing or wrongly-typed field rather than panicking,
/// so a provider shape change empties a table instead of aborting the CLI.
pub fn rows_of(payload: &Value, key: &str) -> Vec<Value> {
    payload
        .get(key)
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

/// Pretty JSON on stdout.
pub fn json(v: &Value) {
    println!(
        "{}",
        serde_json::to_string_pretty(v).unwrap_or_else(|_| v.to_string())
    );
}

/// Default text renderer for a resource read: an object renders as a
/// key/value block, an array as a pipe-delimited table.
pub fn render(v: &Value) {
    match v {
        Value::Array(arr) => table(arr),
        Value::Object(_) => kv(v, 0),
        Value::Null => println!("(no data)"),
        other => println!("{}", scalar(other)),
    }
}

/// Render an object as an indented `Key: value` block.
pub fn kv(obj: &Value, indent: usize) {
    let pad = " ".repeat(indent);
    if let Some(map) = obj.as_object() {
        for (k, val) in map {
            match val {
                Value::Object(_) if as_money(val).is_some() => {
                    println!("{pad}{k}: {}", scalar(val));
                }
                Value::Object(_) => {
                    println!("{pad}{k}:");
                    kv(val, indent + 2);
                }
                Value::Array(arr) if arr.iter().all(|x| !x.is_object() && !x.is_array()) => {
                    let joined = arr.iter().map(scalar).collect::<Vec<_>>().join(", ");
                    println!("{pad}{k}: {joined}");
                }
                Value::Array(arr) => {
                    println!("{pad}{k}: [{} items]", arr.len());
                    table(arr);
                }
                other => println!("{pad}{k}: {}", scalar(other)),
            }
        }
    }
}

/// Render an array of objects as a pipe-delimited table with `ALL_CAPS`
/// headers (column order = union of keys, first-seen order). Falls back to
/// one value per line for arrays of scalars.
pub fn table(arr: &[Value]) {
    if arr.is_empty() {
        println!("(none)");
        return;
    }
    if arr.iter().all(|x| !x.is_object()) {
        for x in arr {
            println!("{}", scalar(x));
        }
        return;
    }
    let mut cols: Vec<String> = Vec::new();
    for row in arr {
        if let Some(map) = row.as_object() {
            for k in map.keys() {
                if !cols.iter().any(|c| c == k) {
                    cols.push(k.clone());
                }
            }
        }
    }
    println!(
        "{}",
        cols.iter()
            .map(|c| c.to_uppercase())
            .collect::<Vec<_>>()
            .join(" | ")
    );
    for row in arr {
        let cells: Vec<String> = cols
            .iter()
            .map(|c| row.get(c).map(scalar).unwrap_or_default())
            .collect();
        println!("{}", cells.join(" | "));
    }
}

/// Render a JSON scalar without quotes; null renders empty. A `Money` object
/// (`{"amount", "currency"}`) renders as `$12.34` / `12.34 EUR` so tables and
/// blocks stay readable.
pub fn scalar(v: &Value) -> String {
    if let Some(money) = as_money(v) {
        return money;
    }
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn as_money(v: &Value) -> Option<String> {
    let map = v.as_object()?;
    if map.len() != 2 {
        return None;
    }
    let amount = map.get("amount")?.as_str()?;
    let currency = map.get("currency")?.as_str()?;
    Some(if currency == "USD" {
        format!("${amount}")
    } else {
        format!("{amount} {currency}")
    })
}

/// Terminal error path: in `--json` mode emit the error DTO on stdout, always
/// write the human message to stderr, and return the exit code to pass to
/// `std::process::exit`.
pub fn fail(err: &crate::CliError, json_mode: bool) -> i32 {
    if json_mode {
        json(&err.to_json());
    }
    eprintln!("error: {err}");
    err.exit_code()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn scalar_unwraps_strings() {
        assert_eq!(scalar(&json!("hi")), "hi");
        assert_eq!(scalar(&json!(3)), "3");
        assert_eq!(scalar(&Value::Null), "");
    }

    #[test]
    fn scalar_renders_money_objects() {
        assert_eq!(
            scalar(&json!({"amount": "42.00", "currency": "USD"})),
            "$42.00"
        );
        assert_eq!(
            scalar(&json!({"amount": "9.99", "currency": "EUR"})),
            "9.99 EUR"
        );
        // not money: wrong keys or extra fields pass through untouched
        assert_eq!(
            scalar(&json!({"amount": "1.00", "kind": "fee"})),
            r#"{"amount":"1.00","kind":"fee"}"#
        );
    }

    /// `emit` writes to stdout, so these assert on the value it would print by
    /// rebuilding it the same way; the flattening rule is the part that
    /// matters to consumers.
    #[test]
    fn table_view_projects_and_omits_missing() {
        let rows = table_view(
            &[json!({"a": 1, "b": 2, "c": 3}), json!({"a": 4})],
            &["a", "c"],
        );
        assert_eq!(rows[0], json!({"a": 1, "c": 3}));
        // Omit-don't-null: the absent column simply isn't there.
        assert_eq!(rows[1], json!({"a": 4}));
        assert!(table_view(&[], &["a"]).is_empty());
    }

    #[test]
    fn table_view_keeps_the_requested_column_order() {
        // With serde_json's preserve_order feature the projection order is what
        // the table renders, so it must follow `columns`, not the input.
        let rows = table_view(&[json!({"z": 1, "a": 2})], &["z", "a"]);
        let keys: Vec<&String> = rows[0].as_object().unwrap().keys().collect();
        assert_eq!(keys, vec!["z", "a"]);
    }

    #[test]
    fn rows_of_tolerates_missing_and_mistyped_fields() {
        assert_eq!(
            rows_of(&json!({"xs": [1, 2]}), "xs"),
            vec![json!(1), json!(2)]
        );
        assert!(rows_of(&json!({}), "xs").is_empty());
        assert!(rows_of(&json!({"xs": "not an array"}), "xs").is_empty());
        assert!(rows_of(&Value::Null, "xs").is_empty());
    }

    #[test]
    fn emit_text_mode_passes_the_untagged_payload_to_the_renderer() {
        let payload = json!({ "payments": [1] });
        let mut seen = None;
        emit(false, "payment-list", payload.clone(), |v| {
            seen = Some(v.clone())
        });
        // The text renderer sees the payload as built — no schema tag.
        assert_eq!(seen, Some(payload));
    }

    #[test]
    fn emit_json_mode_does_not_invoke_the_text_renderer() {
        let mut called = false;
        emit(true, "x", json!({ "a": 1 }), |_| called = true);
        assert!(!called, "text renderer must not run in --json mode");
    }
}
