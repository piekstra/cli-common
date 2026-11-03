//! Read-only queries against an ArcGIS REST layer, which is how many US counties
//! publish parcel data.
//!
//! Three behaviors of these servers shape this crate:
//!
//! - **Errors arrive as HTTP 200**, with an `error` object in the body, so a success
//!   status proves nothing. They map onto the exit codes: a rejected query is usage,
//!   a missing layer is not found, and anything else is upstream.
//! - **A page can come back short with more to follow.** A server asked for 2,000 rows
//!   can send 1,900 with `exceededTransferLimit` set, so paging advances by the rows
//!   received and ends only when that flag is absent.
//! - **A body without `features` is not an empty result.** It fails, so a changed
//!   response cannot read as "no parcels here".

use pk_cli_core::CliError;
use serde_json::Value;

const DEFAULT_PAGE_SIZE: u32 = 1000;

/// One layer, addressed by its full URL, such as `.../FeatureServer/0`.
#[derive(Debug, Clone)]
pub struct Layer {
    http: reqwest::blocking::Client,
    url: String,
    page_size: u32,
}

/// What to ask a layer for. The sort is required because paging is only coherent over
/// a stable order: without one, a row can repeat or go missing between pages.
#[derive(Debug, Clone)]
pub struct Query<'a> {
    where_clause: &'a str,
    order_by: &'a str,
    fields: &'a str,
    limit: u32,
}

impl<'a> Query<'a> {
    /// Every column of every matching row, sorted by `order_by`.
    pub fn new(where_clause: &'a str, order_by: &'a str) -> Self {
        Self {
            where_clause,
            order_by,
            fields: "*",
            limit: u32::MAX,
        }
    }

    /// A comma-separated list of columns, to avoid carrying all of them.
    pub fn fields(mut self, fields: &'a str) -> Self {
        self.fields = fields;
        self
    }

    /// The most rows to return.
    pub fn limit(mut self, limit: u32) -> Self {
        self.limit = limit;
        self
    }
}

impl Layer {
    pub fn new(http: reqwest::blocking::Client, url: impl Into<String>) -> Self {
        Self {
            http,
            url: url.into().trim_end_matches('/').to_string(),
            page_size: DEFAULT_PAGE_SIZE,
        }
    }

    /// Rows to ask for per request. A server may send fewer, which paging handles.
    pub fn page_size(mut self, rows: u32) -> Self {
        self.page_size = rows.max(1);
        self
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    /// How many rows match, whatever a later query's limit.
    pub fn count(&self, where_clause: &str) -> Result<u64, CliError> {
        let body = self.request(&[
            ("where", where_clause),
            ("returnCountOnly", "true"),
            ("f", "json"),
        ])?;
        body.get("count")
            .and_then(Value::as_u64)
            .ok_or_else(|| CliError::Upstream("ArcGIS count response had no `count`".into()))
    }

    /// The attributes of each matching row, up to the query's limit.
    pub fn rows(&self, query: &Query) -> Result<Vec<Value>, CliError> {
        let limit = query.limit as usize;
        let mut rows = Vec::new();
        while rows.len() < limit {
            let want = (limit - rows.len()).min(self.page_size as usize);
            let body = self.request(&[
                ("where", query.where_clause),
                ("outFields", query.fields),
                ("orderByFields", query.order_by),
                ("returnGeometry", "false"),
                ("resultOffset", &rows.len().to_string()),
                ("resultRecordCount", &want.to_string()),
                ("f", "json"),
            ])?;
            let features = body
                .get("features")
                .and_then(Value::as_array)
                .ok_or_else(|| CliError::Upstream("ArcGIS response had no `features`".into()))?;
            let more = body
                .get("exceededTransferLimit")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if more && features.is_empty() {
                return Err(CliError::Upstream(
                    "ArcGIS said more rows follow and sent none".into(),
                ));
            }
            rows.extend(features.iter().filter_map(|f| f.get("attributes").cloned()));
            if !more {
                break;
            }
        }
        Ok(rows)
    }

    fn request(&self, params: &[(&str, &str)]) -> Result<Value, CliError> {
        let response = self
            .http
            .get(format!("{}/query", self.url))
            .query(params)
            .send()
            .map_err(|e| CliError::Upstream(format!("requesting {}: {e}", self.url)))?;
        let body = pk_cli_http::json_response(response)?;
        check_error(&body)?;
        Ok(body)
    }
}

fn check_error(body: &Value) -> Result<(), CliError> {
    let Some(error) = body.get("error") else {
        return Ok(());
    };
    let code = error.get("code").and_then(Value::as_i64).unwrap_or(0);
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("unspecified ArcGIS error");
    let details = error
        .get("details")
        .and_then(Value::as_array)
        .map(|d| {
            d.iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join("; ")
        })
        .filter(|d| !d.is_empty());
    let full = match details {
        Some(d) => format!("{message} ({d})"),
        None => message.to_string(),
    };
    Err(match code {
        400 => CliError::Usage(format!("ArcGIS rejected the query: {full}")),
        404 => CliError::NotFound(format!("ArcGIS layer not found: {full}")),
        _ => CliError::Upstream(format!("ArcGIS error {code}: {full}")),
    })
}

/// A value as a SQL string literal, for building a `where` clause from user input.
pub fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}
