# fred-mcp

An MCP server for **FRED — Federal Reserve Economic Data** — built as a
WebAssembly component for Cosmonic Desktop. Search and fetch U.S. economic
time series (unemployment, GDP, CPI, …) from the St. Louis Fed's API.

This example demonstrates **outbound HTTP with an API key**: FRED requires a
free `api_key` query parameter, supplied from the environment (`FRED_API_KEY`)
and **never compiled into the component**.

Faithful to the reference Rust `fred` MCP server: three tools, the same
endpoints, the same key handling.

Built with [mcp-server-template-rs](https://github.com/cosmonic-labs/mcp-server-template-rs):
rmcp 3.x, MCP spec 2026-07-28, exports `wasi:http/handler@0.3.0`.

## Tools

| Tool | Purpose |
|---|---|
| `search_series` | Search FRED for series matching a query (e.g. "unemployment", "GDP"). |
| `get_series` | Fetch metadata for a series by ID (e.g. `UNRATE`, `GDP`, `CPIAUCSL`). |
| `get_series_observations` | Fetch the data points for a series, newest first (`limit`, default 10). |

## The `FRED_API_KEY` requirement

Get a free key at
<https://fred.stlouisfed.org/docs/api/api_key.html>. The server reads it from
`FRED_API_KEY` and adds it as a query parameter. If it is not set, every tool
returns a clear, actionable error (not a crash) telling you to set it.

- **Cosmonic Desktop:** register a secret *reference* and point the workload
  at it with `secretFrom` (as `deploy/workload.yaml` does), so the key lives
  in your OS keychain and never appears in a manifest. With the
  `cosmonic_set_secret` MCP tool:
  ```
  name:  fred-api-key                      # the reference name used in secretFrom
  uri:   keychain://cosmonic/fred-api-key  # keychain-backed storage
  env:   FRED_API_KEY                      # env var injected into the component
  value: <your-key>                        # write-only; never returned or logged
  ```
  ```yaml
  localResources:
    environment:
      secretFrom:
        - name: fred-api-key   # provides FRED_API_KEY
  ```

## Endpoints & `allowedHosts`

All under `https://api.stlouisfed.org`: `/fred/series/search`, `/fred/series`,
`/fred/series/observations`. The workload allow-list needs one host:

```yaml
allowedHosts: [api.stlouisfed.org]
```

## Build

```console
$ cargo build --release
```

## Deploy on Cosmonic Desktop

Deployment is via [Cosmonic Desktop](https://cosmonic.com/docs/desktop):
register the `fred-api-key` secret (above), then apply
[`deploy/workload.yaml`](deploy/workload.yaml) for the published image, or
promote a local build and apply `workload.yaml` — see the
[repo README](../README.md#running-an-example-on-cosmonic-desktop) for the
full walkthrough. Then call it through the ingress:

```console
$ curl -X POST http://fred-mcp.localhost:8200/ \
    -H 'Content-Type: application/json' \
    -H 'Accept: application/json, text/event-stream' \
    -H 'MCP-Protocol-Version: 2026-07-28' \
    -H 'Mcp-Method: tools/call' -H 'Mcp-Name: get_series_observations' \
    -d '{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"get_series_observations","arguments":{"series_id":"UNRATE","limit":5},"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{}}}}'
```

## Test

```console
$ ./scripts/e2e.sh
```

31 cases, fully hermetic: a **local fixture impersonates the FRED API** (via
the `FRED_BASE_URL` override). Covers all three tools, query encoding, forced
`limit`/`sort_order`, limit clamping, unknown series, and — importantly — the
missing-`FRED_API_KEY` path.

## Configuration

| Env var | Purpose |
|---|---|
| `FRED_API_KEY` | Required. Free key from FRED. Prefer a Cosmonic secret in production. |
| `FRED_BASE_URL` | Override the FRED host (testing only). |
