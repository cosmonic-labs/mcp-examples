//! SEC EDGAR client: ticker→CIK resolution, submissions, and XBRL company
//! concepts, over `wasi:http` via the bridge's outbound client.
//!
//! Endpoints (all GET, JSON):
//! - `https://www.sec.gov/files/company_tickers.json` — ticker→CIK table
//! - `https://data.sec.gov/submissions/CIK{cik10}.json` — recent filings
//! - `https://data.sec.gov/api/xbrl/companyconcept/CIK{cik10}/us-gaap/{tag}.json`
//!
//! Every request carries a `User-Agent` naming a real contact, per SEC policy
//! (a request without one is refused with HTTP 403). The value comes from the
//! `SEC_USER_AGENT` environment variable.

use std::collections::HashMap;
use std::sync::OnceLock;

use rmcp::model::{CallToolResult, ContentBlock};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::Mutex;

use crate::bridge::outbound;
use crate::server::TickerTable;

const MAX_FILINGS: usize = 25;
const DEFAULT_USER_AGENT: &str = "Cosmonic Labs sec-edgar-mcp admin@cosmonic.com";

/// Base URL for `www.sec.gov` (ticker table, filing document links). The
/// `SEC_BASE_URL_WWW` override exists for testing against a local fixture.
fn base_www() -> String {
    std::env::var("SEC_BASE_URL_WWW").unwrap_or_else(|_| "https://www.sec.gov".to_owned())
}

/// Base URL for `data.sec.gov` (submissions, XBRL concepts). Override with
/// `SEC_BASE_URL_DATA` for testing.
fn base_data() -> String {
    std::env::var("SEC_BASE_URL_DATA").unwrap_or_else(|_| "https://data.sec.gov".to_owned())
}

/// Concept groups fetched for the core summary, each with a fallback chain of
/// us-gaap tags tried in order (mirrors the reference).
const SUMMARY_GROUPS: &[(&str, &[&str])] = &[
    ("Assets", &["Assets"]),
    ("Liabilities", &["Liabilities"]),
    ("StockholdersEquity", &["StockholdersEquity"]),
    (
        "Revenues",
        &[
            "RevenueFromContractWithCustomerExcludingAssessedTax",
            "Revenues",
            "SalesRevenueNet",
        ],
    ),
    ("NetIncomeLoss", &["NetIncomeLoss"]),
    ("EarningsPerShareDiluted", &["EarningsPerShareDiluted"]),
];

/// An EDGAR lookup error, distinguishing "no such thing" (a normal, expected
/// answer) from an infrastructure failure.
pub enum EdgarError {
    NotFound(String),
    Failed(String),
}

impl EdgarError {
    /// Renders the error as an MCP tool-level error result — the caller sees
    /// the message. (This is not a protocol error; the request was valid.)
    pub fn into_tool_result(self) -> CallToolResult {
        let text = match self {
            EdgarError::NotFound(msg) => format!("No matching SEC EDGAR record: {msg}"),
            EdgarError::Failed(msg) => format!("SEC EDGAR request failed: {msg}"),
        };
        CallToolResult::error(vec![ContentBlock::text(text)])
    }
}

fn user_agent() -> String {
    std::env::var("SEC_USER_AGENT").unwrap_or_else(|_| DEFAULT_USER_AGENT.to_owned())
}

/// GETs a URL and deserializes the JSON body, applying SEC's required headers
/// and status-code conventions.
async fn get_json(url: &str) -> Result<Value, EdgarError> {
    let request = http::Request::get(url)
        .header("User-Agent", user_agent())
        .header("Accept", "application/json")
        .body(bytes::Bytes::new())
        .map_err(|err| EdgarError::Failed(format!("building request: {err}")))?;

    let response = outbound::fetch(request)
        .await
        .map_err(|err| EdgarError::Failed(format!("GET {url}: {err}")))?;

    match response.status().as_u16() {
        200 => serde_json::from_slice(response.body())
            .map_err(|err| EdgarError::Failed(format!("decoding {url}: {err}"))),
        404 => Err(EdgarError::NotFound(url.to_owned())),
        403 => Err(EdgarError::Failed(format!(
            "GET {url}: 403 Forbidden (check SEC_USER_AGENT names a real contact)"
        ))),
        other => Err(EdgarError::Failed(format!(
            "GET {url}: unexpected status {other}"
        ))),
    }
}

/// Per-instance ticker→CIK cache. Requests are serialized by the transport, so
/// a plain async mutex around the lazily-loaded table is sufficient.
fn ticker_cache() -> &'static Mutex<Option<TickerTable>> {
    static CACHE: OnceLock<Mutex<Option<TickerTable>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(None))
}

#[derive(Deserialize)]
struct TickerRecord {
    cik_str: u64,
    ticker: String,
}

/// Resolves a ticker symbol to a zero-padded 10-digit CIK, loading and caching
/// the SEC ticker table on first use.
async fn resolve_cik(ticker: &str) -> Result<String, EdgarError> {
    let symbol = ticker.trim().to_ascii_uppercase();
    if symbol.is_empty() {
        return Err(EdgarError::NotFound("ticker must not be empty".to_owned()));
    }

    let mut guard = ticker_cache().lock().await;
    if guard.is_none() {
        let value = get_json(&format!("{}/files/company_tickers.json", base_www())).await?;
        // The document is a JSON object keyed by arbitrary row index.
        let rows: HashMap<String, TickerRecord> = serde_json::from_value(value)
            .map_err(|err| EdgarError::Failed(format!("parsing ticker table: {err}")))?;
        let mut table = HashMap::with_capacity(rows.len());
        for record in rows.into_values() {
            if !record.ticker.is_empty() {
                table.insert(record.ticker.to_ascii_uppercase(), pad_cik(record.cik_str));
            }
        }
        if table.is_empty() {
            return Err(EdgarError::Failed(
                "EDGAR returned no usable ticker rows".to_owned(),
            ));
        }
        *guard = Some(std::sync::Arc::new(table));
    }

    let table = guard.as_ref().expect("cache populated above");
    table
        .get(&symbol)
        .cloned()
        .ok_or_else(|| EdgarError::NotFound(format!("no CIK registered for ticker {symbol:?}")))
}

/// Zero-pads a CIK to the 10-digit form EDGAR's data APIs expect.
fn pad_cik(cik: u64) -> String {
    format!("{cik:010}")
}

/// `get_company_facts` implementation.
pub async fn company_facts(
    ticker: &str,
    year: i32,
    concept: Option<&str>,
) -> Result<Value, EdgarError> {
    let cik = resolve_cik(ticker).await?;

    // Build the list of (group name, candidate tags) to fetch.
    let groups: Vec<(String, Vec<String>)> = match concept {
        Some(c) => {
            let c = c.trim();
            if c.is_empty() {
                return Err(EdgarError::NotFound("concept must not be empty".to_owned()));
            }
            vec![(c.to_owned(), vec![c.to_owned()])]
        }
        None => SUMMARY_GROUPS
            .iter()
            .map(|(name, tags)| {
                (
                    (*name).to_owned(),
                    tags.iter().map(|t| (*t).to_owned()).collect(),
                )
            })
            .collect(),
    };

    let mut metrics = Vec::new();
    let mut missing = Vec::new();
    let mut entity_name: Option<String> = None;

    for (group, tags) in &groups {
        let mut found = None;
        for tag in tags {
            let url = format!(
                "{}/api/xbrl/companyconcept/CIK{cik}/us-gaap/{tag}.json",
                base_data()
            );
            match get_json(&url).await {
                Ok(value) => {
                    if entity_name.is_none() {
                        entity_name = value
                            .get("entityName")
                            .and_then(Value::as_str)
                            .map(str::to_owned);
                    }
                    if let Some(metric) = select_annual(&value, group, year) {
                        found = Some(metric);
                        break;
                    }
                }
                // A missing tag just advances the fallback chain.
                Err(EdgarError::NotFound(_)) => continue,
                Err(other) => return Err(other),
            }
        }
        match found {
            Some(metric) => metrics.push(metric),
            None => missing.push(group.clone()),
        }
    }

    if metrics.is_empty() && groups.len() == 1 {
        return Err(EdgarError::NotFound(format!(
            "{} reported no {} for fiscal year {year}",
            ticker.trim().to_ascii_uppercase(),
            groups[0].0,
        )));
    }

    Ok(json!({
        "ticker": ticker.trim().to_ascii_uppercase(),
        "cik": cik,
        "entity_name": entity_name,
        "fiscal_year": year,
        "metrics": metrics,
        "missing": missing,
    }))
}

/// Picks the best annual (10-K) fact for `fiscal_year` from a companyconcept
/// response, preferring an exact `fy`/`fp=="FY"` match, then the latest
/// `end`/`filed` date.
fn select_annual(value: &Value, group: &str, year: i32) -> Option<Value> {
    let units = value.get("units")?.as_object()?;
    // USD first, then other unit keys in a deterministic order.
    let mut unit_keys: Vec<&String> = units.keys().collect();
    unit_keys.sort_by_key(|k| (k.as_str() != "USD", k.as_str().to_owned()));

    // Rank tuple: (rank, is_usd) so a USD fact wins ties over a same-rank,
    // same-date fact reported in another unit (e.g. a foreign currency).
    let mut best: Option<(i32, bool, String, String, Value)> = None;
    for unit in unit_keys {
        let is_usd = unit.as_str() == "USD";
        let Some(facts) = units.get(unit).and_then(Value::as_array) else {
            continue;
        };
        for fact in facts {
            let form = fact.get("form").and_then(Value::as_str).unwrap_or_default();
            if !form.starts_with("10-K") {
                continue;
            }
            let fy = fact.get("fy").and_then(Value::as_i64).map(|v| v as i32);
            let fp = fact.get("fp").and_then(Value::as_str).unwrap_or_default();
            let end = fact.get("end").and_then(Value::as_str).unwrap_or_default();
            let end_year = end.get(0..4).and_then(|s| s.parse::<i32>().ok());
            let rank = if fy == Some(year) && fp == "FY" {
                3
            } else if fy == Some(year) {
                2
            } else if end_year == Some(year) {
                1
            } else {
                continue;
            };
            let filed = fact
                .get("filed")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let candidate = json!({
                "concept": group,
                "label": value.get("label").and_then(Value::as_str),
                "value": fact.get("val"),
                "unit": unit,
                "form": form,
                "end": end,
                "filed": filed,
            });
            // Compare on (rank, is_usd, end, filed), higher wins.
            let key = (rank, is_usd, end, filed);
            let better = match &best {
                None => true,
                Some((r, u, e, f, _)) => key > (*r, *u, e.as_str(), f.as_str()),
            };
            if better {
                best = Some((rank, is_usd, end.to_owned(), filed.to_owned(), candidate));
            }
        }
    }
    best.map(|(_, _, _, _, metric)| metric)
}

/// `get_submission_history` implementation.
pub async fn submission_history(
    ticker: &str,
    form_type: Option<&str>,
) -> Result<Value, EdgarError> {
    let cik = resolve_cik(ticker).await?;
    let url = format!("{}/submissions/CIK{cik}.json", base_data());
    let value = get_json(&url).await?;

    let filter = form_type.map(|f| f.trim().to_ascii_uppercase());
    let recent = value
        .get("filings")
        .and_then(|f| f.get("recent"))
        .cloned()
        .unwrap_or_else(|| json!({}));

    let col = |name: &str| -> Vec<Value> {
        recent
            .get(name)
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
    };
    let forms = col("form");
    let accession = col("accessionNumber");
    let filing_date = col("filingDate");
    let report_date = col("reportDate");
    let primary_doc = col("primaryDocument");
    let primary_desc = col("primaryDocDescription");

    let count = forms.len().min(accession.len()).min(filing_date.len());
    let mut filings = Vec::new();
    let mut truncated = false;
    let strip_cik: String = cik.trim_start_matches('0').to_owned();

    for i in 0..count {
        let form = forms[i].as_str().unwrap_or_default();
        if let Some(want) = &filter {
            let upper = form.to_ascii_uppercase();
            if &upper != want && !upper.starts_with(&format!("{want}/")) {
                continue;
            }
        }
        if filings.len() >= MAX_FILINGS {
            truncated = true;
            break;
        }
        let acc = accession[i].as_str().unwrap_or_default();
        let doc = primary_doc
            .get(i)
            .and_then(Value::as_str)
            .unwrap_or_default();
        let url = if !acc.is_empty() && !doc.is_empty() {
            let acc_nodash = acc.replace('-', "");
            Some(format!(
                "{}/Archives/edgar/data/{strip_cik}/{acc_nodash}/{doc}",
                base_www()
            ))
        } else {
            None
        };
        filings.push(json!({
            "form": form,
            "filing_date": filing_date[i].as_str().unwrap_or_default(),
            "report_date": report_date.get(i).and_then(Value::as_str),
            "accession_number": acc,
            "description": primary_desc.get(i).and_then(Value::as_str),
            "url": url,
        }));
    }

    Ok(json!({
        "ticker": ticker.trim().to_ascii_uppercase(),
        "cik": cik,
        "entity_name": value.get("name").and_then(Value::as_str),
        "industry": value.get("sicDescription").and_then(Value::as_str),
        "form_type": filter,
        "filings": filings,
        "truncated": truncated,
    }))
}
