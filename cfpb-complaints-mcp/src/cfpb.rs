//! CFPB Consumer Complaint Database client, over `wasi:http` via the bridge's
//! outbound client.
//!
//! The database is the public record of complaints the Consumer Financial
//! Protection Bureau has sent to companies for response. It is exposed as a
//! read-only Elasticsearch-backed REST API — no key, no auth:
//!
//! - `GET /data-research/consumer-complaints/search/api/v1/` — search + facets
//! - `GET /data-research/consumer-complaints/search/api/v1/{id}` — one complaint
//! - `GET /data-research/consumer-complaints/search/api/v1/trends` — time series
//! - `GET /data-research/consumer-complaints/search/api/v1/_suggest_company/` — autocomplete
//!
//! Two behaviors of the upstream are worth knowing, because they shape the
//! tool results this module returns:
//!
//! 1. **Facet semantics.** Every aggregation is computed with all filters
//!    applied *except the one on its own field*. Grouping by `state` while
//!    filtering `state=CA` therefore returns every state, not just CA. The
//!    true filtered count is `hits.total`, which this module reports
//!    separately as `filtered_total`.
//! 2. **Validation errors are informative.** A bad filter value comes back as
//!    HTTP 400 with a JSON body naming the field and the valid choices. That
//!    body is passed through to the caller verbatim rather than flattened
//!    into "request failed" — it is the fastest way for a caller to correct
//!    its own arguments.

use serde_json::{json, Map, Value};

use crate::bridge::outbound;

/// Narrative text is the largest field by far. Truncate it in list results so
/// a wide page of complaints does not blow out the caller's context; the full
/// text is always available from `get_complaint`.
const NARRATIVE_PREVIEW_CHARS: usize = 600;

/// Upper bound on `size` for a search page, matching the upstream cap.
pub const MAX_SIZE: u32 = 100;

/// Upper bound on the number of facet buckets returned by `complaint_stats`.
pub const MAX_TOP: usize = 100;

/// Base URL for the CFPB site. `CFPB_BASE_URL` overrides it for testing
/// against a local fixture.
fn base_url() -> String {
    std::env::var("CFPB_BASE_URL").unwrap_or_else(|_| "https://www.consumerfinance.gov".to_owned())
}

fn api_root() -> String {
    format!(
        "{}/data-research/consumer-complaints/search/api/v1",
        base_url()
    )
}

/// A lookup error, distinguishing a rejected request (the caller can fix it),
/// an empty result, and an infrastructure failure.
pub enum CfpbError {
    /// The upstream rejected the query — 400 with a field-level explanation.
    BadRequest(String),
    /// An argument this server rejected before or after the call, without the
    /// upstream having refused anything.
    InvalidArgument(String),
    /// The query was valid but matched nothing.
    NotFound(String),
    /// Transport, timeout, or an unexpected upstream status.
    Failed(String),
}

impl CfpbError {
    /// Renders the error as an MCP tool-level error result — the caller sees
    /// the message. (This is not a protocol error; the request was valid MCP.)
    pub fn into_tool_result(self) -> rmcp::model::CallToolResult {
        use rmcp::model::{CallToolResult, ContentBlock};
        let text = match self {
            CfpbError::BadRequest(msg) => {
                format!("CFPB rejected the query: {msg}")
            }
            CfpbError::InvalidArgument(msg) => format!("Invalid argument: {msg}"),
            CfpbError::NotFound(msg) => format!("No matching CFPB complaint record: {msg}"),
            CfpbError::Failed(msg) => format!("CFPB request failed: {msg}"),
        };
        CallToolResult::error(vec![ContentBlock::text(text)])
    }
}

/// Accumulates `key=value` pairs into a percent-encoded query string.
///
/// The CFPB API takes repeated keys for multi-valued filters (`product=a&
/// product=b`), so this deliberately appends rather than replacing.
#[derive(Default)]
pub struct Query {
    pairs: Vec<(String, String)>,
}

impl Query {
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends a pair. Empty values are dropped — the upstream treats an empty
    /// filter as a validation error rather than as "unset".
    pub fn push(&mut self, key: &str, value: &str) -> &mut Self {
        let value = value.trim();
        if !value.is_empty() {
            self.pairs.push((key.to_owned(), value.to_owned()));
        }
        self
    }

    /// Appends a pair only when the option is present and non-empty.
    pub fn push_opt(&mut self, key: &str, value: Option<&str>) -> &mut Self {
        if let Some(value) = value {
            self.push(key, value);
        }
        self
    }

    /// Appends one pair per element of a multi-valued filter.
    pub fn push_all(&mut self, key: &str, values: &[String]) -> &mut Self {
        for value in values {
            self.push(key, value);
        }
        self
    }

    pub fn push_num(&mut self, key: &str, value: impl std::fmt::Display) -> &mut Self {
        self.pairs.push((key.to_owned(), value.to_string()));
        self
    }

    fn encode(&self) -> String {
        self.pairs
            .iter()
            .map(|(k, v)| format!("{}={}", urlencoding::encode(k), urlencoding::encode(v)))
            .collect::<Vec<_>>()
            .join("&")
    }
}

/// GETs a CFPB API path with the given query and deserializes the JSON body.
async fn get_json(path: &str, query: &Query) -> Result<Value, CfpbError> {
    let encoded = query.encode();
    let url = if encoded.is_empty() {
        format!("{}{path}", api_root())
    } else {
        format!("{}{path}?{encoded}", api_root())
    };

    let request = http::Request::get(&url)
        .header("Accept", "application/json")
        .header("User-Agent", user_agent())
        .body(bytes::Bytes::new())
        .map_err(|err| CfpbError::Failed(format!("building request: {err}")))?;

    let response = outbound::fetch(request)
        .await
        .map_err(|err| CfpbError::Failed(format!("GET {url}: {err}")))?;

    let status = response.status().as_u16();
    match status {
        200 => serde_json::from_slice(response.body())
            .map_err(|err| CfpbError::Failed(format!("decoding {url}: {err}"))),
        // The 400 body names the offending field and its valid choices, so
        // pass it through instead of discarding it.
        400 => Err(CfpbError::BadRequest(describe_body(response.body()))),
        404 => Err(CfpbError::NotFound(url)),
        429 => Err(CfpbError::Failed(format!(
            "GET {url}: 429 Too Many Requests (the CFPB API is rate limiting; retry shortly)"
        ))),
        other => Err(CfpbError::Failed(format!(
            "GET {url}: unexpected status {other}"
        ))),
    }
}

/// The CFPB does not require a contact header, but sending one is good
/// citizenship for a public API and makes traffic identifiable.
fn user_agent() -> String {
    std::env::var("CFPB_USER_AGENT")
        .unwrap_or_else(|_| "cfpb-complaints-mcp (Cosmonic Desktop MCP server)".to_owned())
}

/// Renders an error body as text, preferring compact JSON when it parses.
fn describe_body(body: &[u8]) -> String {
    match serde_json::from_slice::<Value>(body) {
        Ok(value) => value.to_string(),
        Err(_) => String::from_utf8_lossy(body).chars().take(500).collect(),
    }
}

/// The filters shared by search, stats, and trends. Kept as one struct so the
/// three tools accept exactly the same slice of the database.
#[derive(Debug, Default)]
pub struct Filters {
    pub search_term: Option<String>,
    pub field: Option<String>,
    pub company: Vec<String>,
    pub state: Vec<String>,
    pub product: Vec<String>,
    pub issue: Vec<String>,
    pub zip_code: Vec<String>,
    pub tags: Vec<String>,
    pub company_response: Vec<String>,
    pub company_public_response: Vec<String>,
    pub consumer_consent_provided: Vec<String>,
    pub submitted_via: Vec<String>,
    pub timely: Option<String>,
    pub has_narrative: Option<bool>,
    pub date_received_min: Option<String>,
    pub date_received_max: Option<String>,
    pub company_received_min: Option<String>,
    pub company_received_max: Option<String>,
}

impl Filters {
    /// Writes every set filter into `query`.
    fn apply(&self, query: &mut Query) {
        query
            .push_opt("search_term", self.search_term.as_deref())
            .push_opt("field", self.field.as_deref())
            .push_all("company", &self.company)
            .push_all("state", &self.state)
            .push_all("product", &self.product)
            .push_all("issue", &self.issue)
            .push_all("zip_code", &self.zip_code)
            .push_all("tags", &self.tags)
            .push_all("company_response", &self.company_response)
            .push_all("company_public_response", &self.company_public_response)
            .push_all("consumer_consent_provided", &self.consumer_consent_provided)
            .push_all("submitted_via", &self.submitted_via)
            .push_opt("timely", self.timely.as_deref())
            .push_opt("date_received_min", self.date_received_min.as_deref())
            .push_opt("date_received_max", self.date_received_max.as_deref())
            .push_opt("company_received_min", self.company_received_min.as_deref())
            .push_opt("company_received_max", self.company_received_max.as_deref());
        if let Some(has_narrative) = self.has_narrative {
            query.push(
                "has_narrative",
                if has_narrative { "true" } else { "false" },
            );
        }
    }

    /// A compact echo of the filters actually sent, so the caller can see what
    /// slice of the database a number describes.
    fn echo(&self) -> Value {
        let mut query = Query::new();
        self.apply(&mut query);
        let mut map = Map::new();
        for (key, value) in &query.pairs {
            match map.get_mut(key) {
                // Repeated key -> collect into an array.
                Some(Value::Array(existing)) => existing.push(Value::String(value.clone())),
                Some(slot) => {
                    let first = slot.take();
                    *slot = Value::Array(vec![first, Value::String(value.clone())]);
                }
                None => {
                    map.insert(key.clone(), Value::String(value.clone()));
                }
            }
        }
        Value::Object(map)
    }
}

/// `search_complaints` implementation.
pub async fn search(
    filters: &Filters,
    size: u32,
    offset: u32,
    sort: Option<&str>,
    full_narrative: bool,
) -> Result<Value, CfpbError> {
    let size = size.min(MAX_SIZE);
    let mut query = Query::new();
    filters.apply(&mut query);
    query
        .push_num("size", size)
        .push_num("frm", offset)
        .push_opt("sort", sort)
        // Facets are a separate tool; skipping them here is a large latency win.
        .push("no_aggs", "true");

    let value = get_json("/", &query).await?;
    let hits = value.get("hits");
    let total = hits
        .and_then(|h| h.get("total"))
        .and_then(|t| t.get("value"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let relation = hits
        .and_then(|h| h.get("total"))
        .and_then(|t| t.get("relation"))
        .and_then(Value::as_str)
        .unwrap_or("eq");

    let rows = hits
        .and_then(|h| h.get("hits"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    let complaints: Vec<Value> = rows
        .iter()
        .filter_map(|row| row.get("_source"))
        .map(|source| summarize(source, full_narrative))
        .collect();

    let returned = complaints.len() as u64;
    Ok(json!({
        "total_matching": total,
        // Elasticsearch reports "gte" once the count passes its tracking
        // limit; surface that rather than implying an exact number.
        "total_is_exact": relation == "eq",
        "returned": returned,
        "offset": offset,
        "next_offset": (offset as u64 + returned < total).then_some(offset as u64 + returned),
        "sort": sort,
        "filters": filters.echo(),
        "complaints": complaints,
        "source": "CFPB Consumer Complaint Database",
    }))
}

/// Reshapes one `_source` document into the flat record the tools return.
fn summarize(source: &Value, full_narrative: bool) -> Value {
    let text = |key: &str| source.get(key).and_then(Value::as_str);
    let narrative = text("complaint_what_happened").unwrap_or("");
    let (narrative, truncated) =
        if full_narrative || narrative.chars().count() <= NARRATIVE_PREVIEW_CHARS {
            (narrative.to_owned(), false)
        } else {
            (
                narrative.chars().take(NARRATIVE_PREVIEW_CHARS).collect(),
                true,
            )
        };

    let mut record = json!({
        "complaint_id": text("complaint_id"),
        // Dates arrive as full ISO timestamps; the date is the meaningful part.
        "date_received": text("date_received").map(date_only),
        "date_sent_to_company": text("date_sent_to_company").map(date_only),
        "product": text("product"),
        "sub_product": text("sub_product"),
        "issue": text("issue"),
        "sub_issue": text("sub_issue"),
        "company": text("company"),
        "state": text("state"),
        "zip_code": text("zip_code"),
        "tags": source.get("tags"),
        "submitted_via": text("submitted_via"),
        "company_response": text("company_response"),
        "company_public_response": text("company_public_response"),
        "consumer_disputed": text("consumer_disputed"),
        "timely_response": text("timely"),
        "has_narrative": source.get("has_narrative"),
    });

    if let Some(object) = record.as_object_mut() {
        if narrative.is_empty() {
            // An absent narrative is the norm (only consumers who opt in are
            // published), so say so rather than emitting an empty string.
            object.insert("narrative".to_owned(), Value::Null);
        } else {
            object.insert("narrative".to_owned(), Value::String(narrative));
            if truncated {
                object.insert("narrative_truncated".to_owned(), Value::Bool(true));
            }
        }
    }
    record
}

/// `2024-09-03T22:42:53.000Z` -> `2024-09-03`.
fn date_only(value: &str) -> String {
    value.get(0..10).unwrap_or(value).to_owned()
}

/// `get_complaint` implementation.
pub async fn get_complaint(complaint_id: &str) -> Result<Value, CfpbError> {
    let id = complaint_id.trim();
    if id.is_empty() || !id.chars().all(|c| c.is_ascii_digit()) {
        return Err(CfpbError::InvalidArgument(format!(
            "complaint_id must be a numeric complaint identifier, got {complaint_id:?}"
        )));
    }

    let value = get_json(&format!("/{id}"), &Query::new()).await?;
    let source = value
        .get("hits")
        .and_then(|h| h.get("hits"))
        .and_then(Value::as_array)
        .and_then(|rows| rows.first())
        .and_then(|row| row.get("_source"));

    match source {
        // Full narrative here: a single complaint is exactly when the caller
        // wants the untruncated text.
        Some(source) => Ok(summarize(source, true)),
        None => Err(CfpbError::NotFound(format!("complaint {id}"))),
    }
}

/// `complaint_stats` implementation: one facet, ranked.
pub async fn stats(filters: &Filters, group_by: &str, top: usize) -> Result<Value, CfpbError> {
    let top = top.min(MAX_TOP);
    let mut query = Query::new();
    filters.apply(&mut query);
    // size=0 asks for facets only; the aggregations come back regardless.
    query.push_num("size", 0);

    let value = get_json("/", &query).await?;

    let filtered_total = value
        .get("hits")
        .and_then(|h| h.get("total"))
        .and_then(|t| t.get("value"))
        .and_then(Value::as_u64)
        .unwrap_or(0);

    // Each facet is nested one level under its own name:
    //   aggregations.<field>.<field>.buckets
    let aggregation = value
        .get("aggregations")
        .and_then(|a| a.get(group_by))
        .ok_or_else(|| {
            CfpbError::InvalidArgument(format!(
                "unknown group_by {group_by:?} — the CFPB API returned no such facet"
            ))
        })?;

    let facet_total = aggregation.get("doc_count").and_then(Value::as_u64);
    let buckets = aggregation
        .get(group_by)
        .and_then(|inner| inner.get("buckets"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    let mut groups = Vec::new();
    for bucket in buckets.iter().take(top) {
        // `has_narrative` buckets key on 0/1 with a `key_as_string` label.
        let key = bucket
            .get("key_as_string")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or_else(|| bucket.get("key").and_then(Value::as_str).map(str::to_owned))
            .or_else(|| bucket.get("key").map(ToString::to_string))
            .unwrap_or_default();
        let count = bucket.get("doc_count").and_then(Value::as_u64).unwrap_or(0);

        let mut group = json!({ "key": key, "count": count });
        // The product facet nests sub-products; surface them, they are the
        // most useful second level in the whole dataset.
        if let Some(sub) = bucket
            .get("sub_product.raw")
            .and_then(|s| s.get("buckets"))
            .and_then(Value::as_array)
        {
            let subs: Vec<Value> = sub
                .iter()
                .take(top)
                .map(|b| {
                    json!({
                        "key": b.get("key").and_then(Value::as_str).unwrap_or_default(),
                        "count": b.get("doc_count").and_then(Value::as_u64).unwrap_or(0),
                    })
                })
                .collect();
            if let Some(object) = group.as_object_mut() {
                object.insert("sub_groups".to_owned(), Value::Array(subs));
            }
        }
        groups.push(group);
    }

    let truncated = buckets.len() > top;
    Ok(json!({
        "group_by": group_by,
        // The count of the filtered slice, from hits.total — the number to
        // quote when describing "how many complaints match".
        "filtered_total": filtered_total,
        // What the buckets below actually sum toward. It differs from
        // filtered_total whenever `group_by` is also filtered, because the
        // upstream computes each facet with its own field's filter removed.
        "facet_total": facet_total,
        "facet_excludes_own_filter": true,
        "groups": groups,
        "truncated": truncated,
        "filters": filters.echo(),
        "source": "CFPB Consumer Complaint Database",
    }))
}

/// The upstream rejects every categorical lens that arrives without a
/// `sub_lens` (or a `focus`), so supply a sensible second dimension when the
/// caller does not name one. Each pairing below is in the lens's own list of
/// accepted sub-lenses, per the API's 400 message.
pub fn default_sub_lens(lens: &str) -> Option<&'static str> {
    match lens {
        "overview" => None,
        "product" => Some("issue"),
        "issue" => Some("product"),
        "company" => Some("product"),
        "tags" => Some("product"),
        // An unrecognized lens goes upstream untouched, so its own error
        // message reaches the caller instead of a guess made here.
        _ => None,
    }
}

/// `complaint_trends` implementation: a volume time series, optionally split
/// by a categorical lens.
pub async fn trends(
    filters: &Filters,
    lens: &str,
    sub_lens: Option<&str>,
    interval: &str,
    top: usize,
) -> Result<Value, CfpbError> {
    let top = top.min(MAX_TOP);
    let sub_lens = sub_lens
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .or_else(|| default_sub_lens(lens));
    let mut query = Query::new();
    filters.apply(&mut query);
    query.push("lens", lens).push("trend_interval", interval);
    if lens != "overview" {
        query.push_opt("sub_lens", sub_lens);
    }

    let value = get_json("/trends", &query).await?;
    let aggregations = value.get("aggregations");

    // `dateRangeArea` is the overall series, and the only date aggregation
    // that honors every filter: the sibling `dateRangeBuckets` drops the
    // non-date filters (under product=Mortgage it reports the whole database
    // over the same range), and `dateRangeBrush` covers all of time for the
    // UI's range slider. Like the facets, the buckets nest under a repeat of
    // the aggregation's own name.
    let series = aggregations
        .and_then(|a| a.get("dateRangeArea"))
        .and_then(|a| a.get("dateRangeArea"))
        .and_then(|a| a.get("buckets"))
        .and_then(Value::as_array)
        .map(|buckets| {
            buckets
                .iter()
                .map(|bucket| {
                    json!({
                        "period": bucket
                            .get("key_as_string")
                            .and_then(Value::as_str)
                            .map(date_only),
                        "count": bucket.get("doc_count").and_then(Value::as_u64).unwrap_or(0),
                    })
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    let range_total = aggregations
        .and_then(|a| a.get("dateRangeArea"))
        .and_then(|a| a.get("doc_count"))
        .and_then(Value::as_u64);

    // For a categorical lens the same aggregation name carries per-category
    // series under `trend_period`.
    let mut by_category = Vec::new();
    if lens != "overview" {
        if let Some(buckets) = aggregations
            .and_then(|a| a.get(lens))
            .and_then(|a| a.get(lens))
            .and_then(|a| a.get("buckets"))
            .and_then(Value::as_array)
        {
            for bucket in buckets.iter().take(top) {
                let key = bucket
                    .get("key")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .or_else(|| bucket.get("key").map(ToString::to_string))
                    .unwrap_or_default();
                let mut points: Vec<Value> = bucket
                    .get("trend_period")
                    .and_then(|t| t.get("buckets"))
                    .and_then(Value::as_array)
                    .map(|periods| {
                        periods
                            .iter()
                            .map(|period| {
                                json!({
                                    "period": period
                                        .get("key_as_string")
                                        .and_then(Value::as_str)
                                        .map(date_only),
                                    "count": period
                                        .get("doc_count")
                                        .and_then(Value::as_u64)
                                        .unwrap_or(0),
                                })
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                // Upstream returns these newest-first; oldest-first reads
                // like a time series everywhere else in this response.
                points.reverse();
                by_category.push(json!({
                    "key": key,
                    "count": bucket.get("doc_count").and_then(Value::as_u64).unwrap_or(0),
                    "series": points,
                }));
            }
        }
    }

    Ok(json!({
        "lens": lens,
        "sub_lens": sub_lens,
        "interval": interval,
        "range_total": range_total,
        "series": series,
        "by_category": by_category,
        "filters": filters.echo(),
        "source": "CFPB Consumer Complaint Database",
    }))
}

/// `suggest_company` implementation: company-name autocomplete.
pub async fn suggest_company(text: &str, limit: usize) -> Result<Value, CfpbError> {
    let text = text.trim();
    if text.is_empty() {
        return Err(CfpbError::InvalidArgument(
            "text must not be empty".to_owned(),
        ));
    }
    let mut query = Query::new();
    query.push("text", text);
    let value = get_json("/_suggest_company/", &query).await?;

    let names: Vec<Value> = value
        .as_array()
        .map(|items| items.iter().take(limit).cloned().collect())
        .unwrap_or_default();

    Ok(json!({
        "text": text,
        "matches": names,
        // These strings are the exact values the `company` filter expects —
        // the database stores one canonical spelling per company.
        "note": "Pass a match verbatim as the `company` filter on other tools.",
    }))
}
