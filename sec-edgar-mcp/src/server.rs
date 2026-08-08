//! SEC EDGAR MCP server — query U.S. public-company filings.
//!
//! Companies are addressed by ticker symbol; the server resolves the symbol to
//! the SEC Central Index Key (CIK) and queries the public EDGAR REST APIs over
//! `wasi:http`. Financial figures come from as-reported XBRL data.
//!
//! Faithful to the reference `sec-edgar-mcp` (Go): two tools, the same
//! endpoints, the same `SEC_USER_AGENT` requirement, and per-instance caching
//! of the ticker→CIK table.

use std::collections::HashMap;
use std::sync::Arc;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock, Implementation, ServerCapabilities, ServerInfo};
use rmcp::{tool, tool_handler, tool_router, ErrorData, ServerHandler};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::edgar;

/// SEC EDGAR MCP server. Stateless per request, but the ticker→CIK table is
/// cached for the lifetime of the component instance (see [`edgar`]).
#[derive(Clone)]
pub struct EdgarServer {
    tool_router: ToolRouter<Self>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CompanyFactsParams {
    /// Ticker symbol, e.g. "AAPL".
    pub ticker: String,
    /// Fiscal year to report, e.g. 2024. Matched against the company's own
    /// fiscal calendar, which may not align with the calendar year.
    pub year: i32,
    /// Optional us-gaap XBRL concept, e.g. "Assets", "Revenues",
    /// "StockholdersEquity". Omit for a core summary.
    #[serde(default)]
    pub concept: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SubmissionHistoryParams {
    /// Ticker symbol, e.g. "AAPL".
    pub ticker: String,
    /// Optional form filter, e.g. "10-K", "10-Q", "8-K". Amendments such as
    /// "10-K/A" are included. Omit for all forms.
    #[serde(default)]
    pub form_type: Option<String>,
}

#[tool_router]
impl EdgarServer {
    pub fn new() -> Self {
        Self {
            tool_router: Self::tool_router(),
        }
    }

    /// As-reported XBRL financial figures for a company's fiscal year.
    #[tool(
        description = "Retrieve as-reported XBRL financial figures for a public company's \
                          fiscal year. Omit 'concept' for a core summary (assets, liabilities, \
                          equity, revenue, net income, diluted EPS); pass one for a single item."
    )]
    #[tracing::instrument(name = "tool.get_company_facts", skip(self))]
    async fn get_company_facts(
        &self,
        Parameters(params): Parameters<CompanyFactsParams>,
    ) -> Result<CallToolResult, ErrorData> {
        match edgar::company_facts(&params.ticker, params.year, params.concept.as_deref()).await {
            Ok(value) => Ok(structured_text(value)),
            Err(err) => Ok(err.into_tool_result()),
        }
    }

    /// A company's most recent SEC filings, newest first (max 25).
    #[tool(
        description = "List a public company's most recent SEC filings, newest first, with \
                          accession numbers and document links. Returns at most 25 filings."
    )]
    #[tracing::instrument(name = "tool.get_submission_history", skip(self))]
    async fn get_submission_history(
        &self,
        Parameters(params): Parameters<SubmissionHistoryParams>,
    ) -> Result<CallToolResult, ErrorData> {
        match edgar::submission_history(&params.ticker, params.form_type.as_deref()).await {
            Ok(value) => Ok(structured_text(value)),
            Err(err) => Ok(err.into_tool_result()),
        }
    }
}

/// Emits a JSON value as both `structuredContent` and a pretty-printed text
/// block (so plain clients see readable output).
fn structured_text(value: serde_json::Value) -> CallToolResult {
    let text = serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string());
    let mut result = CallToolResult::structured(value);
    result.content = vec![ContentBlock::text(text)];
    result
}

/// Shared reference to the per-instance ticker→CIK table.
pub(crate) type TickerTable = Arc<HashMap<String, String>>;

#[tool_handler(router = self.tool_router)]
impl ServerHandler for EdgarServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                env!("CARGO_PKG_NAME"),
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(
                "Query U.S. public company filings from SEC EDGAR. Companies are addressed \
                 by ticker symbol; the server resolves the symbol to the SEC Central Index \
                 Key. Financial figures are as-reported XBRL data the company itself filed.",
            )
    }
}
