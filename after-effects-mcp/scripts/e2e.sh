#!/usr/bin/env bash
# End-to-end tests for after-effects-mcp. See ../scripts/mcp_e2e_lib.sh for the
# shared framework harness (protocol, spec enforcement, robustness, guard).
set -u
cd "$(dirname "$0")/.."

PORT=8199
GUARD_PORT=8198
WASM=target/wasm32-wasip2/release/after_effects_mcp.wasm

# shellcheck source=../scripts/mcp_e2e_lib.sh
source "$(dirname "$0")/../../scripts/mcp_e2e_lib.sh"

# Used by the framework's concurrency test.
FIRST_TOOL_NAME=frames_to_timecode
FIRST_TOOL_ARGS='{"frames":48,"fps":24}'
FIRST_TOOL_EXPECT='00:00:02:00'

mcp_build_if_needed "${1:-}"
mcp_harness_start
mcp_harness_start_guard

framework_tests frames_to_timecode timecode_to_frames interpolate expression effect_template hex_to_rgb

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

guard_tests
mcp_harness_report
