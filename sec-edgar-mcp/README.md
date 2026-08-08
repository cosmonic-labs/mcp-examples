# sec-edgar-mcp

An MCP server that queries **U.S. public-company filings from SEC EDGAR**,
built as a WebAssembly component for Cosmonic Desktop. This is the first
example that makes **outbound HTTP calls** — to `data.sec.gov` and
`www.sec.gov` — governed by the workload's `allowedHosts` policy.

Faithful to the reference `sec-edgar-mcp` (Go): the same two tools, the same
endpoints, the same `SEC_USER_AGENT` requirement, ticker→CIK resolution, and
per-instance caching of the ticker table.

Built with [mcp-server-template-rs](https://github.com/cosmonic-labs/mcp-server-template-rs):
rmcp 3.x, MCP spec 2026-07-28, exports `wasi:http/handler@0.3.0`.

## Tools

| Tool | Purpose |
|---|---|
| `get_company_facts` | As-reported XBRL figures for a company's fiscal year. Omit `concept` for a summary (assets, liabilities, equity, revenue, net income, diluted EPS); pass one for a single line item. |
| `get_submission_history` | A company's most recent filings, newest first (max 25), with accession numbers and document links. Optional `form_type` filter (`10-K`, `10-Q`, `8-K`). |

Companies are addressed by **ticker** (e.g. `AAPL`); the server resolves it to
the SEC Central Index Key. Unknown tickers return a tool-level error, not a
crash.

## The `SEC_USER_AGENT` requirement

SEC **requires** every request to carry a `User-Agent` naming a real contact
(e.g. `"Company Name admin@example.com"`); requests without one get HTTP 403.
Set `SEC_USER_AGENT` in the workload (a sensible default is compiled in). This
is the whole reason the tool exists as a server rather than a browser call.

## Endpoints & `allowedHosts`

- `https://www.sec.gov/files/company_tickers.json` — ticker→CIK table (cached)
- `https://data.sec.gov/submissions/CIK{cik}.json` — filings
- `https://data.sec.gov/api/xbrl/companyconcept/CIK{cik}/us-gaap/{tag}.json` — XBRL

The workload's outbound allow-list must contain exactly these two hosts:

```yaml
allowedHosts: [www.sec.gov, data.sec.gov]
```

## Build

```console
$ cargo build --release
```

## Deploy on Cosmonic Desktop

Deployment is via [Cosmonic Desktop](https://cosmonic.com/docs/desktop):
apply [`deploy/workload.yaml`](deploy/workload.yaml) for the published image,
or promote a local build and apply `workload.yaml` — see the
[repo README](../README.md#running-an-example-on-cosmonic-desktop) for the
full walkthrough. Then call it through the ingress:

```console
$ curl -X POST http://sec-edgar-mcp.localhost:8200/ \
    -H 'Content-Type: application/json' \
    -H 'Accept: application/json, text/event-stream' \
    -H 'MCP-Protocol-Version: 2026-07-28' \
    -H 'Mcp-Method: tools/call' -H 'Mcp-Name: get_submission_history' \
    -d '{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"get_submission_history","arguments":{"ticker":"AAPL","form_type":"10-K"},"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{}}}}'
```

Outbound requests are governed by the workload's `allowedHosts` policy
(`www.sec.gov`, `data.sec.gov`).

## Test

```console
$ ./scripts/e2e.sh
```

36 cases, fully hermetic: the suite runs a **local fixture that impersonates
SEC EDGAR** (via the server's `SEC_BASE_URL_WWW` / `SEC_BASE_URL_DATA`
overrides) — so no network is needed. Covers ticker→CIK resolution, the
concept fallback chains, fiscal-year selection, form filtering, unknown
tickers, and 8 concurrent outbound calls.

## Configuration

| Env var | Purpose |
|---|---|
| `SEC_USER_AGENT` | Required by SEC; `"Name email"`. Default compiled in. |
| `MCP_OUTBOUND_MAX_BYTES` | Raise the 4 MiB outbound cap; some EDGAR documents are larger. |
| `SEC_BASE_URL_WWW` / `SEC_BASE_URL_DATA` | Override SEC hosts (testing only). |
