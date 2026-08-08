//! FRED API client over `wasi:http` via the bridge's outbound client.
//!
//! Endpoints (all GET, JSON), host `api.stlouisfed.org`:
//! - `/fred/series/search?search_text=…`
//! - `/fred/series?series_id=…`
//! - `/fred/series/observations?series_id=…&sort_order=desc&limit=…`
//!
//! `api_key` and `file_type=json` are always supplied; the key comes from the
//! `FRED_API_KEY` environment variable (a query parameter, per the FRED API —
//! never a header, never compiled into the component).

use std::fmt;

use serde_json::Value;

use crate::bridge::outbound;

/// A FRED client error, surfaced to the model as a tool-level error.
pub enum FredError {
    /// `FRED_API_KEY` is not configured.
    MissingKey,
    /// The request failed or the API returned a non-2xx status.
    Request(String),
}

impl fmt::Display for FredError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FredError::MissingKey => write!(
                f,
                "FRED_API_KEY is not set. Get a free key at \
                 https://fred.stlouisfed.org/docs/api/api_key.html and set it in the \
                 workload environment (or via a Cosmonic secret)."
            ),
            FredError::Request(msg) => write!(f, "FRED request failed: {msg}"),
        }
    }
}

fn base_url() -> String {
    std::env::var("FRED_BASE_URL").unwrap_or_else(|_| "https://api.stlouisfed.org".to_owned())
}

fn api_key() -> Result<String, FredError> {
    std::env::var("FRED_API_KEY").map_err(|_| FredError::MissingKey)
}

/// Percent-encodes a query-parameter value (unreserved set `A-Za-z0-9-_.~`).
fn encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// GETs a FRED endpoint (path plus already-encoded query pairs) and returns the
/// decoded JSON body. `api_key` and `file_type=json` are injected here.
async fn get(path: &str, params: &[(&str, String)]) -> Result<Value, FredError> {
    let key = api_key()?;
    let mut query = format!("api_key={}&file_type=json", encode(&key));
    for (name, value) in params {
        query.push('&');
        query.push_str(name);
        query.push('=');
        query.push_str(&encode(value));
    }
    let url = format!("{}{path}?{query}", base_url());

    let request = http::Request::get(&url)
        .header("Accept", "application/json")
        .body(bytes::Bytes::new())
        .map_err(|err| FredError::Request(format!("building request: {err}")))?;

    let response = outbound::fetch(request)
        .await
        .map_err(|err| FredError::Request(err.to_string()))?;

    let status = response.status();
    let body = response.body();
    if !status.is_success() {
        // FRED returns 400 with a JSON error_message for bad keys / series.
        let detail = serde_json::from_slice::<Value>(body)
            .ok()
            .and_then(|v| {
                v.get("error_message")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .unwrap_or_else(|| String::from_utf8_lossy(body).into_owned());
        return Err(FredError::Request(format!("HTTP {status}: {detail}")));
    }
    serde_json::from_slice(body)
        .map_err(|err| FredError::Request(format!("decoding response: {err}")))
}

/// `search_series` — free-text search, capped at 10 results.
pub async fn search_series(query: &str) -> Result<Value, FredError> {
    get(
        "/fred/series/search",
        &[
            ("search_text", query.to_owned()),
            ("limit", "10".to_owned()),
        ],
    )
    .await
}

/// `get_series` — series metadata.
pub async fn get_series(series_id: &str) -> Result<Value, FredError> {
    get("/fred/series", &[("series_id", series_id.to_owned())]).await
}

/// `get_series_observations` — the actual data points, newest first.
pub async fn get_observations(series_id: &str, limit: u32) -> Result<Value, FredError> {
    // Clamp to FRED's documented maximum (100000) and at least 1.
    let limit = limit.clamp(1, 100_000);
    get(
        "/fred/series/observations",
        &[
            ("series_id", series_id.to_owned()),
            ("sort_order", "desc".to_owned()),
            ("limit", limit.to_string()),
        ],
    )
    .await
}
