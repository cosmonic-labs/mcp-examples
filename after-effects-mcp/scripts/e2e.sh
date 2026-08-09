#!/usr/bin/env bash
# End-to-end tests for after-effects-mcp. See ../scripts/mcp_e2e_lib.sh for the
# shared framework harness (protocol, spec enforcement, robustness, guard).
#
# The live-control tools need a wasi:keyvalue host, which `wasmtime serve` does
# not provide, so the harness composes testing/kv-stub into the component with
# `wac plug` and drives the bridge endpoints itself — curl plays the part of
# the After Effects panel. No copy of After Effects is involved.
set -u
cd "$(dirname "$0")/.."

PORT=8199
GUARD_PORT=8198
WASM=target/wasm32-wasip2/release/after_effects_mcp.wasm
PLUGGED=target/wasm32-wasip2/release/after_effects_mcp_plugged.wasm
STUB=testing/kv-stub/target/wasm32-wasip2/release/kv_stub.wasm

# shellcheck source=../scripts/mcp_e2e_lib.sh
source "$(dirname "$0")/../../scripts/mcp_e2e_lib.sh"

# Used by the framework's concurrency test.
FIRST_TOOL_NAME=frames_to_timecode
FIRST_TOOL_ARGS='{"frames":48,"fps":24}'
FIRST_TOOL_EXPECT='00:00:02:00'

mcp_build_if_needed "${1:-}"

# Satisfy the wasi:keyvalue import with the test stub, then serve the composed
# component instead of the raw one.
if [ "${1:-}" != "--no-build" ]; then
  echo "building keyvalue stub..."
  (cd testing/kv-stub && cargo build --release --target wasm32-wasip2) || exit 1
fi
[ -f "$STUB" ] || { echo "missing $STUB"; exit 1; }
echo "composing keyvalue stub into the component..."
wac plug "$WASM" --plug "$STUB" -o "$PLUGGED" || exit 1
if wasm-tools component wit "$PLUGGED" 2>/dev/null | grep -q 'import wasi:keyvalue'; then
  fail "keyvalue import satisfied by the stub" "import still present"
else
  pass "keyvalue import satisfied by the stub"
fi
WASM="$PLUGGED"

# The stub keeps state in a preopened directory, because `wasmtime serve`
# builds a fresh instance per request and anything in linear memory would not
# outlive the request that wrote it.
KV_DIR=$(mktemp -d -t ae-mcp-kv)
trap 'rm -rf "$KV_DIR" "${KV_DIR}-guard"' EXIT

mkdir -p "${KV_DIR}-guard"
mcp_harness_start --dir "${KV_DIR}::/kv"
mcp_harness_start_guard --dir "${KV_DIR}-guard::/kv"

BASE="http://127.0.0.1:${PORT}"

# --- panel simulation -------------------------------------------------------

# Identifies the simulated panel. The server serves only the highest client id
# it has seen, so any test that registers a newer panel must raise this too.
PANEL_CLIENT=1000

# panel_poll [client] — one bridge poll, as an up-to-date panel would make it.
panel_poll() { curl -s "${BASE}/bridge/command?v=2&client=${1:-$PANEL_CLIENT}"; }

# panel_answer <id> <result-json> — post a result for a dispatched command.
panel_answer() {
  curl -s -o /dev/null -X POST "${BASE}/bridge/result?id=$1" \
    -H 'Content-Type: application/json' -d "$2"
}

# json_field <json> <python-expression over `d`>
json_field() { printf '%s' "$1" | python3 -c "import sys,json;d=json.load(sys.stdin);print($2)"; }

# panel_serve <result-json> — claim the next command and answer it. Echoes the
# command the panel received so callers can assert on the arguments.
panel_serve() {
  local cmd id
  for _ in $(seq 1 40); do
    cmd=$(panel_poll)
    case "$cmd" in *'"command":null'*|'') sleep 0.2 ;; *) break ;; esac
  done
  id=$(json_field "$cmd" "d['id']")
  panel_answer "$id" "$1"
  printf '%s' "$cmd"
}

framework_tests frames_to_timecode timecode_to_frames interpolate expression \
  effect_template hex_to_rgb bridge_status get_results get_help run_script \
  get_project_info list_compositions get_layer_info create_composition \
  create_text_layer create_shape_layer create_solid_layer add_image_layer \
  create_camera set_layer_properties batch_set_layer_properties \
  set_layer_keyframe set_layer_expression set_layer_mask apply_effect \
  apply_effect_template set_composition_properties duplicate_layer \
  delete_layer delete_composition run_batch save_frame_png save_project \
  bridge_test_effects

echo "== frames_to_timecode / timecode_to_frames =="
OUT=$(mcp_call frames_to_timecode '{"frames":48,"fps":24}')
assert_contains "48 frames @ 24 = 00:00:02:00" '00:00:02:00' "$OUT"
OUT=$(mcp_call frames_to_timecode '{"frames":90000,"fps":30}')
assert_contains "90000 frames @ 30 = 00:50:00:00" '00:50:00:00' "$OUT"
# 29.97 rounds to 30 for the FF field (non-drop).
OUT=$(mcp_call frames_to_timecode '{"frames":30,"fps":29.97}')
assert_contains "1 second @ 29.97 (non-drop)" '00:00:01:00' "$OUT"
OUT=$(mcp_call timecode_to_frames '{"timecode":"00:00:02:00","fps":24}')
assert_contains "roundtrip 00:00:02:00 @ 24 = 48" '"frames":48' "$OUT"
# Adversarial: fps <= 0, non-finite, frame field out of range, bad format.
OUT=$(mcp_call frames_to_timecode '{"frames":10,"fps":0}')
assert_contains "fps=0 rejected" '-32602' "$OUT"
OUT=$(mcp_call frames_to_timecode '{"frames":10,"fps":-24}')
assert_contains "negative fps rejected" '-32602' "$OUT"
OUT=$(mcp_call timecode_to_frames '{"timecode":"00:00:00:24","fps":24}')
assert_contains "frame field == fps rejected" 'out of range' "$OUT"
OUT=$(mcp_call timecode_to_frames '{"timecode":"1:2:3","fps":24}')
assert_contains "3-field timecode rejected" 'four fields' "$OUT"
OUT=$(mcp_call timecode_to_frames '{"timecode":"aa:bb:cc:dd","fps":24}')
assert_contains "non-numeric timecode rejected" 'invalid timecode field' "$OUT"
# Large but valid frame count must not overflow or panic.
OUT=$(mcp_call frames_to_timecode '{"frames":9999999999,"fps":24}')
assert_contains "very large frame count handled" '"timecode"' "$OUT"
# fps that rounds to 0 must be rejected, not divide-by-zero trap the instance.
OUT=$(mcp_call frames_to_timecode '{"frames":10,"fps":0.4}')
assert_contains "fps rounding to 0 rejected" '-32602' "$OUT"
OUT=$(mcp_call frames_to_timecode '{"frames":10,"fps":0.4}')
assert_contains "server alive after fps=0.4" 'round to at least 1' "$OUT"
# Overflowing timecode must be a clean error, not a silent wrap.
OUT=$(mcp_call timecode_to_frames '{"timecode":"9999999999999999:00:00:00","fps":24}')
assert_contains "overflowing timecode rejected" '-32602' "$OUT"

echo "== interpolate =="
OUT=$(mcp_call interpolate '{"from":0,"to":100,"t":0.5,"easing":"linear"}')
assert_contains "linear midpoint = 50" '"value":50' "$OUT"
OUT=$(mcp_call interpolate '{"from":0,"to":100,"t":0.5,"easing":"ease_in_out"}')
assert_contains "ease_in_out midpoint = 50" '"value":50' "$OUT"
OUT=$(mcp_call interpolate '{"from":0,"to":100,"t":2.5,"easing":"linear"}')
assert_contains "t clamped above 1" '"value":100' "$OUT"
OUT=$(mcp_call interpolate '{"from":0,"to":100,"t":-1,"easing":"linear"}')
assert_contains "t clamped below 0" '"value":0' "$OUT"
OUT=$(mcp_call interpolate '{"from":0,"to":100,"t":1,"easing":"ease_out_back"}')
assert_contains "ease_out_back lands on target at t=1" '"value":100' "$OUT"
# easeOutBack overshoots past the target (~108) before settling back to 100.
OUT=$(mcp_call interpolate '{"from":0,"to":100,"t":0.7,"easing":"ease_out_back"}')
assert_contains "ease_out_back overshoots past target" '"value":108' "$OUT"
# Adversarial: Infinity in t (JSON 1e400 parses to inf) is rejected cleanly.
OUT=$(printf '%s' "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/call\",\"params\":{\"name\":\"interpolate\",\"arguments\":{\"from\":0,\"to\":100,\"t\":1e400,\"easing\":\"linear\"},$META}}" | mcp_post "http://127.0.0.1:${PORT}/" -H 'Mcp-Method: tools/call' -H 'Mcp-Name: interpolate')
assert_not_contains "infinite t does not produce a null value" '"value":null' "$OUT"
# Overflow to non-finite output is reported, not returned as null.
OUT=$(mcp_call interpolate '{"from":-1e308,"to":1e308,"t":0.5,"easing":"linear"}')
assert_contains "non-finite interpolation rejected" 'non-finite' "$OUT"

echo "== expression =="
OUT=$(mcp_call expression '{"kind":"wiggle","rate":3,"amplitude":50}')
assert_contains "wiggle expression" 'wiggle(3, 50)' "$OUT"
OUT=$(mcp_call expression '{"kind":"wiggle"}')
assert_contains "wiggle defaults" 'wiggle(2, 30)' "$OUT"
OUT=$(mcp_call expression '{"kind":"loop_out"}')
assert_contains "loopOut expression" 'loopOut' "$OUT"
OUT=$(mcp_call expression '{"kind":"time_drive","rate":90}')
assert_contains "time drive expression" 'time * 90' "$OUT"
OUT=$(mcp_call expression '{"kind":"bounce"}')
assert_contains "bounce expression" 'velocityAtTime' "$OUT"

echo "== effect_template =="
OUT=$(mcp_call effect_template '{"template":"gaussian-blur"}')
assert_contains "gaussian-blur match name" 'ADBE Gaussian Blur 2' "$OUT"
OUT=$(mcp_call effect_template '{"template":"text-pop"}')
assert_contains "text-pop is two effects" 'ADBE Glow' "$OUT"
assert_contains "text-pop includes drop shadow" 'ADBE Drop Shadow' "$OUT"
# An unknown enum variant fails parameter deserialization, which rmcp reports
# as a tool-level error (isError:true), not a JSON-RPC -32602.
OUT=$(mcp_call effect_template '{"template":"not-a-template"}')
assert_contains "unknown template rejected" 'unknown variant' "$OUT"
assert_contains "unknown template is a tool error" '"isError":true' "$OUT"

echo "== hex_to_rgb =="
OUT=$(mcp_call hex_to_rgb '{"hex":"#ff8800"}')
assert_contains "ff8800 -> 255,136,0" '"rgba_255":[255,136,0,255]' "$OUT"
OUT=$(mcp_call hex_to_rgb '{"hex":"f80"}')
assert_contains "shorthand f80 expands" '[255,136,0,255]' "$OUT"
OUT=$(mcp_call hex_to_rgb '{"hex":"#11223344"}')
assert_contains "8-digit hex keeps alpha" '[17,34,51,68]' "$OUT"
OUT=$(mcp_call hex_to_rgb '{"hex":"nothex"}')
assert_contains "non-hex rejected" '-32602' "$OUT"
OUT=$(mcp_call hex_to_rgb '{"hex":"#12345"}')
assert_contains "5-digit hex rejected" '3, 6, or 8' "$OUT"

echo "== bridge: no panel =="
OUT=$(mcp_call bridge_status '{}')
assert_contains "bridge_status reports no panel" '"panelConnected":false' "$OUT"
assert_contains "bridge_status says how to open the panel" 'mcp-bridge-auto.jsx' "$OUT"
OUT=$(mcp_call get_results '{}')
assert_contains "get_results with nothing queued" 'no-results' "$OUT"
# A command with no panel is queued but NOT executed, and says so as a tool
# error — a success here would claim work that never happened.
OUT=$(mcp_call list_compositions '{}')
assert_contains "unserviced command is a tool error" '"isError":true' "$OUT"
assert_contains "unserviced command reports it was only queued" 'queued-not-executed' "$OUT"
assert_contains "unserviced command names the fix" 'install-bridge.sh' "$OUT"

echo "== bridge: panel protocol =="
OUT=$(panel_poll)
assert_contains "panel claims the queued command" '"command":"listCompositions"' "$OUT"
assert_contains "claimed command is marked dispatched" '"status":"dispatched"' "$OUT"
OUT=$(panel_poll)
assert_contains "a claimed command is not handed out twice" '"command":null' "$OUT"
OUT=$(mcp_call bridge_status '{}')
assert_contains "bridge_status sees the poll" '"panelConnected":true' "$OUT"
# A panel that does not declare v=2 mishandles our responses, so it is refused
# rather than allowed to consume and drop commands.
OUT=$(curl -s "${BASE}/bridge/command?client=1000")
assert_contains "panel without v=2 is refused" '"command":null' "$OUT"
assert_contains "refused panel is told to reinstall" 'outdated or has been superseded' "$OUT"
# Highest client id wins, so reloading the panel supersedes the old instance.
curl -s -o /dev/null "${BASE}/bridge/command?v=2&client=2000"
OUT=$(curl -s "${BASE}/bridge/command?v=2&client=1000")
assert_contains "superseded panel is refused" 'superseded' "$OUT"
# The reloaded panel is now the live one; everything below polls as that.
PANEL_CLIENT=2000

echo "== bridge: round trip =="
# The tool call blocks on the panel, so the panel has to run concurrently —
# which is the point: /bridge/* is served without the MCP request lock.
mcp_call create_text_layer '{"text":"Hello","fontSize":96,"compName":"Main"}' >/tmp/ae-e2e-tool.json &
TOOL_PID=$!
CMD=$(panel_serve '{"status":"success","message":"Text layer created successfully","layer":{"name":"Hello","index":1}}')
wait $TOOL_PID
OUT=$(cat /tmp/ae-e2e-tool.json)
assert_contains "panel receives the camelCase command name" '"command":"createTextLayer"' "$CMD"
assert_contains "panel receives the tool arguments" '"text":"Hello"' "$CMD"
assert_contains "panel receives compName" '"compName":"Main"' "$CMD"
assert_contains "tool returns the panel's result" 'Text layer created successfully' "$OUT"
assert_contains "successful result is not an error" '"isError":false' "$OUT"
assert_contains "result is machine-readable" '"structuredContent"' "$OUT"
# Absent optional fields must not be sent: the panel distinguishes "omitted"
# (leave alone) from any concrete value.
assert_not_contains "omitted optional params are not sent" 'startTime' "$CMD"

echo "== bridge: panel-reported failure =="
mcp_call delete_layer '{"layerIndex":99}' >/tmp/ae-e2e-tool.json &
TOOL_PID=$!
panel_serve '{"status":"error","message":"Layer index out of bounds: 99"}' >/dev/null
wait $TOOL_PID
OUT=$(cat /tmp/ae-e2e-tool.json)
assert_contains "panel error surfaces as a tool error" '"isError":true' "$OUT"
assert_contains "panel error message is preserved" 'out of bounds' "$OUT"
# The panel's other failure spelling (success:false) must be caught too.
mcp_call set_layer_keyframe '{"layerIndex":1,"propertyName":"Nope","timeInSeconds":0,"value":0}' >/tmp/ae-e2e-tool.json &
TOOL_PID=$!
panel_serve '{"success":false,"message":"Property not found"}' >/dev/null
wait $TOOL_PID
assert_contains "success:false is also a tool error" '"isError":true' "$(cat /tmp/ae-e2e-tool.json)"

echo "== bridge: get_results =="
OUT=$(mcp_call get_results '{}')
assert_contains "get_results returns the last result" 'Property not found' "$OUT"

echo "== bridge: enum and argument mapping =="
mcp_call apply_effect_template '{"layerIndex":1,"templateName":"drop-shadow"}' >/dev/null &
TOOL_PID=$!
CMD=$(panel_serve '{"status":"success"}')
wait $TOOL_PID
assert_contains "effect templates keep their kebab-case names" '"templateName":"drop-shadow"' "$CMD"
mcp_call run_batch '{"commands":[{"command":"createComposition","args":{"name":"A"}}],"undoGroup":"scene"}' >/dev/null &
TOOL_PID=$!
CMD=$(panel_serve '{"status":"success","requested":1,"completed":1,"failed":0}')
wait $TOOL_PID
assert_contains "batch entries name panel scripts" '"command":"createComposition"' "$CMD"
assert_contains "batch passes the undo group" '"undoGroup":"scene"' "$CMD"
mcp_call run_script '{"script":"createCamera","parameters":{"zoom":1000}}' >/dev/null &
TOOL_PID=$!
CMD=$(panel_serve '{"status":"success"}')
wait $TOOL_PID
assert_contains "run_script forwards the raw parameters" '"zoom":1000' "$CMD"
# An unknown script is rejected by the schema before it reaches the panel.
OUT=$(mcp_call run_script '{"script":"rm -rf /","parameters":{}}')
assert_contains "unknown script rejected" 'unknown variant' "$OUT"
assert_contains "unknown script is a tool error" '"isError":true' "$OUT"

echo "== bridge: panel source is served =="
OUT=$(curl -s "${BASE}/bridge/panel.jsx")
assert_contains "panel.jsx is served for installation" 'MCP Bridge Auto' "$OUT"
assert_contains "served panel polls with v=2" '/bridge/command?v=2' "$OUT"
OUT=$(curl -s "${BASE}/healthz")
assert_contains "healthz responds" 'ok' "$OUT"

guard_tests
mcp_harness_report
