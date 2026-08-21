#!/usr/bin/env bash
# End-to-end tests for cfpb-complaints-mcp. Shared harness in ../scripts/mcp_e2e_lib.sh.
#
# Outbound tests run against a LOCAL fixture that impersonates the CFPB
# complaint API (the server's CFPB_BASE_URL override points at it), so the
# suite is hermetic and needs no network.
#
# The fixture reproduces the three upstream behaviors this server exists to
# paper over, so the tests fail if that handling regresses:
#   1. facets are computed with their own field's filter removed,
#   2. `dateRangeBuckets` ignores the non-date filters, only `dateRangeArea`
#      honors them,
#   3. a categorical `lens` without a `sub_lens` is a 400.
set -u
cd "$(dirname "$0")/.."

PORT=8189
GUARD_PORT=8188
FIXTURE_PORT=8187
WASM=target/wasm32-wasip2/release/cfpb_complaints_mcp.wasm

# shellcheck source=../scripts/mcp_e2e_lib.sh
source "$(dirname "$0")/../../scripts/mcp_e2e_lib.sh"

FIRST_TOOL_NAME=suggest_company
FIRST_TOOL_ARGS='{"text":"wells"}'
FIRST_TOOL_EXPECT='WELLS FARGO'

# --- CFPB fixture server ----------------------------------------------------
python3 - "$FIXTURE_PORT" <<'EOF' >/dev/null 2>&1 &
import sys, json
from urllib.parse import urlparse, parse_qs
# ThreadingHTTPServer: the server makes concurrent outbound calls (the
# framework concurrency test fires 8 at once), so the fixture must handle
# concurrent connections or those requests fail.
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

API = "/data-research/consumer-complaints/search/api/v1"

VALID_STATES = {"CA", "TX", "FL", "NY", "WI"}
VALID_SORTS = {"relevance_desc", "relevance_asc", "created_date_desc", "created_date_asc"}
# Mirrors the upstream: a categorical lens is rejected without a second
# dimension, and the 400 names the sub-lenses that lens accepts.
SUB_LENSES = {
    "product": ("sub_product", "issue", "company", "tags"),
    "issue": ("product", "sub_issue", "company", "tags"),
    "company": ("product", "issue", "tags"),
    "tags": ("product", "issue", "company"),
}

def complaint(cid, company, product, state, narrative=""):
    return {
        "complaint_id": cid, "company": company, "product": product,
        "sub_product": "Conventional home mortgage", "issue": "Trouble during payment process",
        "sub_issue": "Escrow, taxes, or insurance", "state": state, "zip_code": "53214",
        "date_received": "2025-03-04T00:00:00.000Z",
        "date_sent_to_company": "2025-03-05T00:00:00.000Z",
        "company_response": "Closed with explanation", "company_public_response": None,
        "consumer_disputed": None, "timely": "Yes", "submitted_via": "Web", "tags": None,
        "complaint_what_happened": narrative, "has_narrative": bool(narrative),
    }

LONG_NARRATIVE = "N" * 900          # exceeds the 600-char preview cap
ROWS = [
    complaint("111", "WELLS FARGO & COMPANY", "Mortgage", "TX", LONG_NARRATIVE),
    complaint("222", "CAPITAL ONE FINANCIAL CORPORATION", "Mortgage", "CA"),
    complaint("333", "WELLS FARGO & COMPANY", "Credit card", "TX"),
]

def hits(rows, total):
    return {"hits": {"total": {"value": total, "relation": "eq"},
                     "hits": [{"_source": r} for r in rows]}}

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
        one = lambda k, d=None: (q.get(k) or [d])[0]

        if u.path == "%s/_suggest_company/" % API:
            text = (one("text") or "").lower()
            return self._send(200, [c for c in
                                    ("WELLS FARGO & COMPANY", "CAPITAL ONE FINANCIAL CORPORATION")
                                    if text and text in c.lower()])

        # Validation errors: shape matches the upstream so the pass-through of
        # the 400 body can be asserted.
        for st in q.get("state", []):
            if st not in VALID_STATES:
                return self._send(400, {"state": {"0": ['"%s" is not a valid choice.' % st]}})
        if one("sort") and one("sort") not in VALID_SORTS:
            return self._send(400, {"sort": ['"%s" is not a valid choice.' % one("sort")]})

        if u.path == "%s/trends" % API:
            lens = one("lens", "overview")
            if lens != "overview":
                if lens not in SUB_LENSES:
                    return self._send(400, {"lens": ['"%s" is not a valid choice.' % lens]})
                if not one("sub_lens"):
                    return self._send(400, {"non_field_errors": [
                        "Either Focus or Sub-lens is required for lens '%s'. "
                        "Valid sub-lens are: %s" % (lens, SUB_LENSES[lens])]})
            # dateRangeArea honors every filter; dateRangeBuckets does NOT
            # (it drops the non-date ones) — exactly the upstream trap.
            filtered = [{"key_as_string": "2025-01-01T00:00:00.000Z", "doc_count": 40},
                        {"key_as_string": "2025-04-01T00:00:00.000Z", "doc_count": 60}]
            unfiltered = [{"key_as_string": "2025-01-01T00:00:00.000Z", "doc_count": 5000},
                          {"key_as_string": "2025-04-01T00:00:00.000Z", "doc_count": 7000}]
            aggs = {
                "dateRangeArea": {"doc_count": 100, "dateRangeArea": {"buckets": filtered}},
                "dateRangeBuckets": {"doc_count": 12000,
                                     "dateRangeBuckets": {"buckets": unfiltered}},
                "dateRangeBrush": {"doc_count": 999999, "dateRangeBrush": {"buckets": unfiltered}},
            }
            if lens != "overview":
                # Upstream returns each category's periods newest-first.
                aggs[lens] = {"doc_count": 100, lens: {"buckets": [
                    {"key": "Trouble during payment process", "doc_count": 70,
                     "trend_period": {"buckets": list(reversed(filtered))}},
                    {"key": "Struggling to pay mortgage", "doc_count": 30,
                     "trend_period": {"buckets": list(reversed(filtered))}},
                ]}}
            body = hits([], 100)
            body["aggregations"] = aggs
            return self._send(200, body)

        # Single complaint by id: /<API>/<digits>
        tail = u.path[len(API):].strip("/")
        if tail.isdigit():
            row = next((r for r in ROWS if r["complaint_id"] == tail), None)
            return self._send(200, hits([row] if row else [], 1 if row else 0))

        if u.path in ("%s/" % API, API):
            rows = ROWS
            for st in q.get("state", []):
                rows = [r for r in rows if r["state"] == st]
            for co in q.get("company", []):
                rows = [r for r in rows if r["company"] == co]
            for pr in q.get("product", []):
                rows = [r for r in rows if r["product"] == pr]
            total = len(rows)
            size = int(one("size", "10"))
            frm = int(one("frm", "0"))
            page = rows[frm:frm + size]
            body = hits(page, total)
            if one("no_aggs") != "true":
                # Facet semantics: an aggregation is computed with its OWN
                # field's filter removed, so `state` here ignores state=... and
                # reports every state. doc_count differs from hits.total to
                # make the distinction observable.
                by_state = {}
                for r in ROWS:
                    by_state[r["state"]] = by_state.get(r["state"], 0) + 1
                body["aggregations"] = {
                    "state": {"doc_count": len(ROWS), "state": {"buckets": [
                        {"key": k, "doc_count": v}
                        for k, v in sorted(by_state.items(), key=lambda kv: -kv[1])]}},
                    "product": {"doc_count": total, "product": {"buckets": [
                        {"key": "Mortgage", "doc_count": total,
                         "sub_product.raw": {"buckets": [
                             {"key": "Conventional home mortgage", "doc_count": total}]}}]}},
                }
            return self._send(200, body)

        self.send_response(404); self.end_headers()

ThreadingHTTPServer(("127.0.0.1", int(sys.argv[1])), H).serve_forever()
EOF
FIXTURE_PID=$!

FIXTURE_ENV="CFPB_BASE_URL=http://127.0.0.1:${FIXTURE_PORT}"

mcp_build_if_needed "${1:-}"
mcp_harness_start --env "$FIXTURE_ENV"
mcp_harness_start_guard --env "$FIXTURE_ENV"

framework_tests search_complaints get_complaint complaint_stats complaint_trends suggest_company

NARRATIVE_900=$(python3 -c "print('N' * 900)")
NARRATIVE_700=$(python3 -c "print('N' * 700)")

echo "== suggest_company =="
OUT=$(mcp_call suggest_company '{"text":"wells"}')
assert_contains "resolves a partial name" 'WELLS FARGO & COMPANY' "$OUT"
assert_contains "explains the exact-match contract" 'verbatim' "$OUT"
OUT=$(mcp_call suggest_company '{"text":"  "}')
assert_contains "blank text rejected" 'must not be empty' "$OUT"
assert_contains "blank text is a tool error" '"isError":true' "$OUT"

echo "== search_complaints =="
OUT=$(mcp_call search_complaints '{"company":["WELLS FARGO & COMPANY"]}')
assert_contains "filters by exact company" '"total_matching":2' "$OUT"
assert_not_contains "other companies excluded" 'CAPITAL ONE' "$OUT"
assert_contains "dates trimmed to YYYY-MM-DD" '"date_received":"2025-03-04"' "$OUT"
# The 900-char fixture narrative must come back capped and flagged.
assert_contains "long narrative truncated" '"narrative_truncated":true' "$OUT"
assert_not_contains "preview stops at the cap" "$NARRATIVE_700" "$OUT"
OUT=$(mcp_call search_complaints '{"company":["WELLS FARGO & COMPANY"],"full_narrative":true}')
assert_contains "full_narrative returns the whole text" "$NARRATIVE_900" "$OUT"
OUT=$(mcp_call search_complaints '{"size":1}')
assert_contains "paging reports the next offset" '"next_offset":1' "$OUT"
assert_contains "one row returned" '"returned":1' "$OUT"
OUT=$(mcp_call search_complaints '{"size":1,"offset":2}')
assert_contains "last page has no next offset" '"next_offset":null' "$OUT"
OUT=$(mcp_call search_complaints '{"state":["ZZ"]}')
assert_contains "upstream validation reaches the caller" 'not a valid choice' "$OUT"
assert_contains "invalid filter is a tool error" '"isError":true' "$OUT"

echo "== get_complaint =="
OUT=$(mcp_call get_complaint '{"complaint_id":"111"}')
assert_contains "fetches by id" '"complaint_id":"111"' "$OUT"
assert_contains "single complaint is never truncated" "$NARRATIVE_900" "$OUT"
OUT=$(mcp_call get_complaint '{"complaint_id":"999"}')
assert_contains "unknown id is a clean not-found" 'No matching CFPB complaint record' "$OUT"
OUT=$(mcp_call get_complaint '{"complaint_id":"not-a-number"}')
assert_contains "non-numeric id rejected before the call" 'must be a numeric' "$OUT"

echo "== complaint_stats =="
OUT=$(mcp_call complaint_stats '{"group_by":"product"}')
assert_contains "ranks the requested facet" '"key":"Mortgage"' "$OUT"
assert_contains "surfaces nested sub-products" 'Conventional home mortgage' "$OUT"
# Facet semantics: grouping by `state` while filtering `state` must report the
# true filtered count separately from what the buckets sum to.
OUT=$(mcp_call complaint_stats '{"group_by":"state","state":["CA"]}')
assert_contains "filtered_total is the real count" '"filtered_total":1' "$OUT"
assert_contains "facet_total is the facet's own scope" '"facet_total":3' "$OUT"
assert_contains "the discrepancy is flagged" '"facet_excludes_own_filter":true' "$OUT"
assert_contains "buckets ignore their own filter" '"key":"TX"' "$OUT"
OUT=$(mcp_call complaint_stats '{"group_by":"nonesuch"}')
assert_contains "unknown group_by explained" 'unknown group_by' "$OUT"

echo "== complaint_trends =="
OUT=$(mcp_call complaint_trends '{}')
assert_contains "defaults to the overview lens" '"lens":"overview"' "$OUT"
assert_contains "defaults to a monthly interval" '"interval":"month"' "$OUT"
# The series must come from dateRangeArea (filtered), NOT dateRangeBuckets.
assert_contains "range_total from the filtered aggregation" '"range_total":100' "$OUT"
assert_contains "series from the filtered aggregation" '"count":40' "$OUT"
assert_not_contains "unfiltered dateRangeBuckets not used" '"count":5000' "$OUT"
assert_contains "overview has no per-category split" '"by_category":[]' "$OUT"
# A categorical lens is a 400 upstream unless a sub_lens rides along.
OUT=$(mcp_call complaint_trends '{"lens":"issue"}')
assert_contains "sub_lens auto-filled for a categorical lens" '"sub_lens":"product"' "$OUT"
assert_contains "per-category series returned" 'Trouble during payment process' "$OUT"
OUT=$(mcp_call complaint_trends '{"lens":"company","sub_lens":"tags"}')
assert_contains "explicit sub_lens honored" '"sub_lens":"tags"' "$OUT"
OUT=$(mcp_call complaint_trends '{"lens":"nonesuch"}')
assert_contains "unknown lens surfaces the upstream error" 'not a valid choice' "$OUT"

guard_tests
mcp_harness_report
