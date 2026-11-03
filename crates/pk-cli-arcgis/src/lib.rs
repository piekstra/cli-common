//! Read-only queries against an ArcGIS REST layer, which is how many US counties
//! publish parcel data. The crate owns the query protocol: paging and error mapping.
//! Layer URLs, field names, `where` clauses and page sizes stay in each CLI.
//!
//! These behaviors of the servers shape it:
//!
//! - **Errors arrive as HTTP 200**, with an `error` object in the body, so a success
//!   status proves nothing. A query rejected on its first request is usage and a missing
//!   layer is not found. Anything else is upstream, including a 400 on a later page,
//!   because the same query was accepted a page ago.
//! - **A page can come back short with more to follow.** A server asked for 2,000 rows
//!   can send 1,900 with `exceededTransferLimit` set, so paging advances by the rows
//!   received. A full page without the flag is asked past too, since not every server
//!   sets it, which costs one request when a result ends on a page boundary.
//! - **A changed response fails.** A body without `features`, a feature without
//!   `attributes`, or a flag that is not a boolean cannot read as "no parcels here".
//! - **A layer that ignores the offset fails** on the second page. Each row carries the
//!   sort's columns, which identify it, so a repeated page is the same rows.
//!
//! It runs over [`Transport`], whose errors name the origin only: a `where` clause holds
//! what someone searched for, and does not belong in an error message. Native code passes
//! `pk_cli_http::BlockingTransport::new()`, which behaves as the Worker does, and drives the
//! futures with [`block_on`](pk_cli_core::block_on). A layer that moved answers a redirect,
//! which is not followed and fails naming where it went, so the CLI's pinned URL gets fixed.

use pk_cli_core::transport::{HttpRequest, Transport};
use pk_cli_core::CliError;
use serde_json::Value;

const DEFAULT_PAGE_SIZE: u32 = 1000;

/// One layer, addressed by its full URL, such as `.../FeatureServer/0`.
#[derive(Debug, Clone)]
pub struct Layer<T> {
    transport: T,
    url: String,
    page_size: u32,
    user_agent: Option<(String, String)>,
}

/// What to ask a layer for. The sort is required because paging is only coherent over
/// a stable order: without one, a row can repeat or go missing between pages. It has to
/// identify a row uniquely, or tied rows can do the same. Its columns come back with each
/// row, whatever [`Query::fields`] says, so a page repeating can only be the same rows.
#[derive(Debug, Clone)]
pub struct Query<'a> {
    where_clause: &'a str,
    order_by: &'a str,
    fields: &'a str,
    limit: Option<u32>,
}

impl<'a> Query<'a> {
    /// Every column of every matching row, sorted by `order_by`, until [`Query::fields`]
    /// or [`Query::limit`] say otherwise. `order_by` must identify a row uniquely.
    pub fn new(where_clause: &'a str, order_by: &'a str) -> Self {
        Self {
            where_clause,
            order_by,
            fields: "*",
            limit: None,
        }
    }

    /// A comma-separated list of columns, to avoid carrying all of them. The sort's columns
    /// come back too.
    pub fn fields(mut self, fields: &'a str) -> Self {
        self.fields = fields;
        self
    }

    /// The most rows to return. Without this, every matching row comes back.
    pub fn limit(mut self, limit: u32) -> Self {
        self.limit = Some(limit);
        self
    }

    fn out_fields(&self) -> String {
        if self.fields.trim() == "*" {
            return "*".to_string();
        }
        let mut fields: Vec<&str> = self.fields.split(',').map(str::trim).collect();
        // Each sort term is a column, optionally followed by ASC or DESC.
        for column in self
            .order_by
            .split(',')
            .filter_map(|term| term.split_whitespace().next())
        {
            if !fields.iter().any(|f| f.eq_ignore_ascii_case(column)) {
                fields.push(column);
            }
        }
        fields.join(",")
    }
}

impl<T: Transport> Layer<T> {
    pub fn new(transport: T, url: impl Into<String>) -> Self {
        Self {
            transport,
            url: url.into().trim_end_matches('/').to_string(),
            page_size: DEFAULT_PAGE_SIZE,
            user_agent: None,
        }
    }

    /// Rows to ask for per request. A server may send fewer, which paging handles.
    pub fn page_size(mut self, rows: u32) -> Self {
        self.page_size = rows.max(1);
        self
    }

    /// The family's `User-Agent: <bin>/<version>` on every request.
    pub fn user_agent(mut self, binary: &str, version: &str) -> Self {
        self.user_agent = Some((binary.to_string(), version.to_string()));
        self
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    /// How many rows match, whatever a later query's limit.
    pub async fn count(&self, where_clause: &str) -> Result<u64, CliError> {
        let params = [
            ("where", where_clause.to_string()),
            ("returnCountOnly", "true".to_string()),
            ("f", "json".to_string()),
        ];
        self.request(&params, true)
            .await?
            .get("count")
            .and_then(Value::as_u64)
            .ok_or_else(|| CliError::Upstream("ArcGIS count response had no `count`".into()))
    }

    /// The attributes of each matching row, up to the query's limit.
    pub async fn rows(&self, query: &Query<'_>) -> Result<Vec<Value>, CliError> {
        let page_size = self.page_size as usize;
        let mut rows: Vec<Value> = Vec::new();
        let mut last_page: Vec<Value> = Vec::new();
        loop {
            let want = match query.limit {
                Some(limit) => (limit as usize).saturating_sub(rows.len()).min(page_size),
                None => page_size,
            };
            if want == 0 {
                return Ok(rows);
            }
            let params = [
                ("where", query.where_clause.to_string()),
                ("outFields", query.out_fields()),
                ("orderByFields", query.order_by.to_string()),
                ("returnGeometry", "false".to_string()),
                ("resultOffset", rows.len().to_string()),
                ("resultRecordCount", want.to_string()),
                ("f", "json".to_string()),
            ];
            let body = self.request(&params, rows.is_empty()).await?;
            let features = body
                .get("features")
                .and_then(Value::as_array)
                .ok_or_else(|| CliError::Upstream("ArcGIS response had no `features`".into()))?;
            let more = more_follow(&body)?;
            if features.is_empty() {
                return if more {
                    Err(CliError::Upstream(
                        "ArcGIS said more rows follow and sent none".into(),
                    ))
                } else {
                    Ok(rows)
                };
            }
            let page = features
                .iter()
                .map(|feature| {
                    feature.get("attributes").cloned().ok_or_else(|| {
                        CliError::Upstream("ArcGIS feature had no `attributes`".into())
                    })
                })
                .collect::<Result<Vec<Value>, CliError>>()?;
            // A server that ignores the offset would send this page forever.
            if page == last_page {
                return Err(CliError::Upstream(
                    "ArcGIS sent the same rows twice: the layer does not page, or the sort does \
                     not identify a row"
                        .into(),
                ));
            }
            rows.extend_from_slice(&page);
            last_page = page;
            if let Some(limit) = query.limit {
                rows.truncate(limit as usize);
            }
            if !more && features.len() < want {
                return Ok(rows);
            }
        }
    }

    async fn request(
        &self,
        params: &[(&str, String)],
        first_request: bool,
    ) -> Result<Value, CliError> {
        let mut request = HttpRequest::get(format!("{}/query", self.url)).query(params);
        if let Some((binary, version)) = &self.user_agent {
            request = request.user_agent(binary, version);
        }
        let response = self.transport.send(request).await?;
        if response.status == 404 {
            return Err(CliError::NotFound("ArcGIS layer not found".into()));
        }
        if (300..400).contains(&response.status) {
            let moved_to = response
                .header_value("location")
                .filter(|to| to.starts_with("http"))
                .map(|to| format!(" to {}", HttpRequest::get(to).origin()))
                .unwrap_or_default();
            return Err(CliError::Upstream(format!(
                "ArcGIS layer moved: HTTP {}{moved_to}",
                response.status
            )));
        }
        if !response.is_success() {
            return Err(CliError::Upstream(format!(
                "ArcGIS returned HTTP {}",
                response.status
            )));
        }
        let body: Value = serde_json::from_str(&response.body)
            .map_err(|e| CliError::Upstream(format!("ArcGIS response was not JSON: {e}")))?;
        check_error(&body, first_request)?;
        Ok(body)
    }
}

fn more_follow(body: &Value) -> Result<bool, CliError> {
    match body.get("exceededTransferLimit") {
        None | Some(Value::Null) => Ok(false),
        Some(Value::Bool(more)) => Ok(*more),
        Some(other) => Err(CliError::Upstream(format!(
            "ArcGIS sent exceededTransferLimit as {other}"
        ))),
    }
}

fn check_error(body: &Value, first_request: bool) -> Result<(), CliError> {
    let Some(error) = body.get("error") else {
        return Ok(());
    };
    let code = error.get("code").and_then(Value::as_i64);
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
        Some(400) if first_request => CliError::Usage(format!("ArcGIS rejected the query: {full}")),
        Some(404) => CliError::NotFound(format!("ArcGIS layer not found: {full}")),
        Some(code) => CliError::Upstream(format!("ArcGIS error {code}: {full}")),
        None => CliError::Upstream(format!("ArcGIS error: {full}")),
    })
}

/// A value as a SQL string literal, for building a `where` clause from user input.
pub fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}
