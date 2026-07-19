//! Output rendering (SPEC v1 §1.4).
//!
//! **Text is the primary format.** Resource reads render token-dense
//! `Key: value` blocks and pipe-delimited tables (`ALL_CAPS` headers). With
//! `--json`, the DTO alone goes to stdout, pretty-printed. Data goes to
//! stdout; diagnostics and confirmations go to stderr.

use serde_json::Value;

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
}
