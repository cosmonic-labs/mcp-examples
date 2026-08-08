#!/usr/bin/env bash
# End-to-end tests for fred-mcp. Shared harness in ../scripts/mcp_e2e_lib.sh.
#
# Outbound tests run against a LOCAL fixture that impersonates the FRED API
# (the server's FRED_BASE_URL override points at it), so the suite is hermetic.
set -u
cd "$(dirname "$0")/.."

PORT=8169
GUARD_PORT=8168
FIXTURE_PORT=8167
WASM=target/wasm32-wasip2/release/fred_mcp.wasm

# shellcheck source=../scripts/mcp_e2e_lib.sh
source "$(dirname "$0")/../../scripts/mcp_e2e_lib.sh"

FIRST_TOOL_NAME=get_series
FIRST_TOOL_ARGS='{"series_id":"UNRATE"}'
FIRST_TOOL_EXPECT='UNRATE'

# --- FRED fixture: records the query string so tests can assert on it. -------
python3 - "$FIXTURE_PORT" <<'EOF' >/dev/null 2>&1 &
import sys, json
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import urlparse, parse_qs

class H(BaseHTTPRequestHandler):
    def log_message(self, *a): pass
    def _send(self, code, obj):
        body = json.dumps(obj).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)
    def do_GET(self):
        u = urlparse(self.path)
        q = parse_qs(u.query)
        # FRED rejects a missing/blank api_key with HTTP 400 + error_message.
        if not q.get("api_key", [""])[0]:
            return self._send(400, {"error_code": 400, "error_message": "Bad Request. The value for variable api_key is not registered."})
        if u.path == "/fred/series/search":
            return self._send(200, {"seriess": [{"id": "UNRATE", "title": "Unemployment Rate",
                                                 "query_echo": q.get("search_text", [""])[0],
                                                 "limit_echo": q.get("limit", [""])[0]}]})
        if u.path == "/fred/series":
            sid = q.get("series_id", [""])[0]
            if sid == "NOPE":
                return self._send(400, {"error_code": 400, "error_message": "The series does not exist."})
            return self._send(200, {"seriess": [{"id": sid, "title": "Unemployment Rate", "frequency": "Monthly"}]})
        if u.path == "/fred/series/observations":
            return self._send(200, {"sort_order_echo": q.get("sort_order", [""])[0],
                                    "limit_echo": q.get("limit", [""])[0],
                                    "observations": [{"date": "2024-12-01", "value": "4.1"},
                                                     {"date": "2024-11-01", "value": "4.2"}]})
        self.send_response(404); self.end_headers()

ThreadingHTTPServer(("127.0.0.1", int(sys.argv[1])), H).serve_forever()
EOF
FIXTURE_PID=$!

FIXTURE_ENV="FRED_BASE_URL=http://127.0.0.1:${FIXTURE_PORT}"

mcp_build_if_needed "${1:-}"
mcp_harness_start --env "$FIXTURE_ENV" --env "FRED_API_KEY=testkey123"
# A second instance WITHOUT FRED_API_KEY, to test the missing-key path. It also
# needs the Host guard set to its own port (mcp_harness_start_guard does that).
mcp_harness_start_guard --env "$FIXTURE_ENV"

framework_tests search_series get_series get_series_observations

echo "== get_series =="
OUT=$(mcp_call get_series '{"series_id":"UNRATE"}')
assert_contains "get_series returns metadata" 'Unemployment Rate' "$OUT"
assert_contains "series id echoed" '"id":"UNRATE"' "$OUT"
OUT=$(mcp_call get_series '{"series_id":"NOPE"}')
assert_contains "unknown series is a tool error" '"isError":true' "$OUT"
assert_contains "unknown series surfaces FRED message" 'does not exist' "$OUT"

echo "== search_series =="
OUT=$(mcp_call search_series '{"query":"unemployment rate"}')
assert_contains "search returns results" 'UNRATE' "$OUT"
# Query is percent-encoded (space -> %20) and limit forced to 10.
assert_contains "search_text passed through" '"query_echo":"unemployment rate"' "$OUT"
assert_contains "limit forced to 10" '"limit_echo":"10"' "$OUT"

echo "== get_series_observations =="
OUT=$(mcp_call get_series_observations '{"series_id":"UNRATE"}')
assert_contains "observations returned" '"value":"4.1"' "$OUT"
assert_contains "sort_order forced desc" '"sort_order_echo":"desc"' "$OUT"
assert_contains "default limit 10" '"limit_echo":"10"' "$OUT"
OUT=$(mcp_call get_series_observations '{"series_id":"UNRATE","limit":3}')
assert_contains "explicit limit passed" '"limit_echo":"3"' "$OUT"
# Adversarial: limit 0 clamps to 1 (FRED rejects 0).
OUT=$(mcp_call get_series_observations '{"series_id":"UNRATE","limit":0}')
assert_contains "limit 0 clamped to 1" '"limit_echo":"1"' "$OUT"
# Huge limit clamps to the documented max.
OUT=$(mcp_call get_series_observations '{"series_id":"UNRATE","limit":99999999}')
assert_contains "huge limit clamped to 100000" '"limit_echo":"100000"' "$OUT"

echo "== FRED_API_KEY requirement =="
# The guard instance has no FRED_API_KEY; the missing-key path must be a clear,
# actionable tool error (not a crash, not a 400 passthrough).
GUARD="http://127.0.0.1:${GUARD_PORT}/"
OUT=$(printf '%s' "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/call\",\"params\":{\"name\":\"get_series\",\"arguments\":{\"series_id\":\"UNRATE\"},${META}}}" \
  | mcp_post "$GUARD" -H 'Mcp-Method: tools/call' -H 'Mcp-Name: get_series')
assert_contains "missing FRED_API_KEY is a clear error" 'FRED_API_KEY is not set' "$OUT"
assert_contains "missing key is a tool error" '"isError":true' "$OUT"

guard_tests
mcp_harness_report
