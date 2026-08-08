//! FRED (Federal Reserve Economic Data) MCP server.
//!
//! Search and fetch U.S. economic time series from the St. Louis Fed's FRED
//! API over `wasi:http`. Faithful to the reference `fred` MCP server: three
//! tools, the same endpoints, and the same `FRED_API_KEY` requirement (the key
//! is a query parameter, supplied from the environment — never baked into the
//! component).

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock, Implementation, ServerCapabilities, ServerInfo};
use rmcp::{tool, tool_handler, tool_router, ErrorData, ServerHandler};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::fred;

/// FRED MCP server. Stateless — one instance per request.
#[derive(Clone)]
pub struct FredServer {
    tool_router: ToolRouter<Self>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SearchSeriesParams {
    /// Free-text search, e.g. "real gross domestic product" or "unemployment".
    pub query: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetSeriesParams {
    /// The FRED series ID, e.g. "UNRATE" (unemployment rate), "GDP",
    /// "CPIAUCSL" (CPI).
    pub series_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetObservationsParams {
    /// The FRED series ID, e.g. "UNRATE".
    pub series_id: String,
    /// Maximum number of observations to return, newest first (default 10).
    #[serde(default)]
    pub limit: Option<u32>,
}

#[tool_router]
impl FredServer {
    pub fn new() -> Self {
        Self {
            tool_router: Self::tool_router(),
        }
    }

    /// Search FRED for economic data series matching a query.
    #[tool(description = "Search FRED for economic data series matching a query \
                          (e.g. \"unemployment\", \"GDP\", \"CPI\")")]
    #[tracing::instrument(name = "tool.search_series", skip(self))]
    async fn search_series(
        &self,
        Parameters(params): Parameters<SearchSeriesParams>,
    ) -> Result<CallToolResult, ErrorData> {
        run(fred::search_series(&params.query).await)
    }

    /// Fetch metadata for a FRED series by its ID.
    #[tool(
        description = "Fetch metadata for a FRED economic data series by its ID \
                          (e.g. \"UNRATE\", \"GDP\", \"CPIAUCSL\")"
    )]
    #[tracing::instrument(name = "tool.get_series", skip(self))]
    async fn get_series(
        &self,
        Parameters(params): Parameters<GetSeriesParams>,
    ) -> Result<CallToolResult, ErrorData> {
        run(fred::get_series(&params.series_id).await)
    }

    /// Fetch the observations (data points) for a FRED series, newest first.
    #[tool(
        description = "Fetch the observations (time-series data points) for a FRED series, \
                          most recent first"
    )]
    #[tracing::instrument(name = "tool.get_series_observations", skip(self))]
    async fn get_series_observations(
        &self,
        Parameters(params): Parameters<GetObservationsParams>,
    ) -> Result<CallToolResult, ErrorData> {
        run(fred::get_observations(&params.series_id, params.limit.unwrap_or(10)).await)
    }
}

/// Renders a client result: a FRED JSON value on success, or a tool-level
/// error the caller can read.
fn run(result: Result<serde_json::Value, fred::FredError>) -> Result<CallToolResult, ErrorData> {
    match result {
        Ok(value) => {
            let text = serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string());
            let mut result = CallToolResult::structured(value);
            result.content = vec![ContentBlock::text(text)];
            Ok(result)
        }
        Err(err) => Ok(CallToolResult::error(vec![ContentBlock::text(
            err.to_string(),
        )])),
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for FredServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                env!("CARGO_PKG_NAME"),
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(
                "Federal Reserve economic data (FRED). Requires FRED_API_KEY. Tools: \
                 search_series, get_series, get_series_observations. Series are addressed \
                 by FRED ID (e.g. UNRATE, GDP, CPIAUCSL).",
            )
    }
}
