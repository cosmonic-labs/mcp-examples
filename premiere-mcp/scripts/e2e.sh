#!/usr/bin/env bash
# End-to-end tests for premiere-mcp. Shared harness in ../scripts/mcp_e2e_lib.sh.
set -u
cd "$(dirname "$0")/.."

PORT=8189
GUARD_PORT=8188
WASM=target/wasm32-wasip2/release/premiere_mcp.wasm

# shellcheck source=../scripts/mcp_e2e_lib.sh
source "$(dirname "$0")/../../scripts/mcp_e2e_lib.sh"

FIRST_TOOL_NAME=frames_to_timecode
FIRST_TOOL_ARGS='{"frames":30,"fps":30}'
FIRST_TOOL_EXPECT='00:00:01:00'

mcp_build_if_needed "${1:-}"
mcp_harness_start
mcp_harness_start_guard

framework_tests frames_to_timecode timecode_to_frames offset_timecode sequence_duration conform_timecode

echo "== non-drop timecode =="
OUT=$(mcp_call frames_to_timecode '{"frames":1800,"fps":30}')
assert_contains "1800 @ 30 ndf = 00:01:00:00" '00:01:00:00' "$OUT"
OUT=$(mcp_call timecode_to_frames '{"timecode":"00:01:00:00","fps":30}')
assert_contains "roundtrip ndf = 1800" '"frames":1800' "$OUT"

echo "== drop-frame timecode (29.97) =="
# Known SMPTE checkpoints: frame 1800 labels as 00:01:00;02 (;00 and ;01 dropped);
# 17982 = 00:10:00;00 (no drop at the tenth minute); 107892 = 01:00:00;00.
OUT=$(mcp_call frames_to_timecode '{"frames":1800,"fps":29.97,"drop_frame":true}')
assert_contains "1800 @ 29.97 df = 00:01:00;02" '00:01:00;02' "$OUT"
OUT=$(mcp_call frames_to_timecode '{"frames":17982,"fps":29.97,"drop_frame":true}')
assert_contains "17982 @ 29.97 df = 00:10:00;00" '00:10:00;00' "$OUT"
OUT=$(mcp_call frames_to_timecode '{"frames":107892,"fps":29.97,"drop_frame":true}')
assert_contains "107892 @ 29.97 df = 01:00:00;00" '01:00:00;00' "$OUT"
OUT=$(mcp_call frames_to_timecode '{"frames":30,"fps":29.97,"drop_frame":true}')
assert_contains "30 @ 29.97 df = 00:00:01;00" '00:00:01;00' "$OUT"
# Roundtrips (drop-frame inferred from ';').
OUT=$(mcp_call timecode_to_frames '{"timecode":"00:01:00;02","fps":29.97}')
assert_contains "roundtrip 00:01:00;02 = 1800" '"frames":1800' "$OUT"
OUT=$(mcp_call timecode_to_frames '{"timecode":"01:00:00;00","fps":29.97}')
assert_contains "roundtrip 01:00:00;00 = 107892" '"frames":107892' "$OUT"
OUT=$(mcp_call timecode_to_frames '{"timecode":"00:10:00;00","fps":29.97}')
assert_contains "roundtrip 00:10:00;00 = 17982" '"frames":17982' "$OUT"

echo "== drop-frame validation =="
# The labels ;00 and ;01 do not exist at non-tenth minute boundaries.
OUT=$(mcp_call timecode_to_frames '{"timecode":"00:01:00;00","fps":29.97}')
assert_contains "invalid df label 00:01:00;00 rejected" 'not a valid drop-frame' "$OUT"
OUT=$(mcp_call timecode_to_frames '{"timecode":"00:10:00;00","fps":29.97}')
assert_contains "valid df label at tenth minute accepted" '"frames"' "$OUT"
# Drop-frame only valid for 29.97/59.94.
OUT=$(mcp_call frames_to_timecode '{"frames":100,"fps":25,"drop_frame":true}')
assert_contains "drop-frame at 25fps rejected" 'only valid for 29.97' "$OUT"
OUT=$(mcp_call frames_to_timecode '{"frames":100,"fps":30,"drop_frame":true}')
assert_contains "drop-frame at exact 30fps rejected" 'fractional NTSC' "$OUT"
# 59.94 drop-frame drops 4 per minute.
OUT=$(mcp_call frames_to_timecode '{"frames":3600,"fps":59.94,"drop_frame":true}')
assert_contains "3600 @ 59.94 df = 00:01:00;04" '00:01:00;04' "$OUT"

echo "== offset_timecode =="
OUT=$(mcp_call offset_timecode '{"timecode":"00:00:10:00","fps":30,"offset_frames":30}')
assert_contains "+30 frames = 00:00:11:00" '00:00:11:00' "$OUT"
OUT=$(mcp_call offset_timecode '{"timecode":"00:00:10:00","fps":30,"offset_frames":-300}')
assert_contains "-300 frames = 00:00:00:00" '00:00:00:00' "$OUT"
OUT=$(mcp_call offset_timecode '{"timecode":"00:00:01:00","fps":30,"offset_frames":-100}')
assert_contains "underflow clamps to 0" '"frames":0' "$OUT"

echo "== sequence_duration =="
OUT=$(mcp_call sequence_duration '{"clip_frames":[30,60,90],"fps":30}')
assert_contains "180 frames total" '"total_frames":180' "$OUT"
assert_contains "180 frames = 00:00:06:00" '00:00:06:00' "$OUT"
OUT=$(mcp_call sequence_duration '{"clip_frames":[],"fps":30}')
assert_contains "empty sequence = 0 frames" '"total_frames":0' "$OUT"

echo "== conform_timecode =="
# 1 second of 24fps footage (24 frames) conformed to 30fps = 30 frames.
OUT=$(mcp_call conform_timecode '{"timecode":"00:00:01:00","from_fps":24,"to_fps":30}')
assert_contains "1s @24 -> 30 = 00:00:01:00" '00:00:01:00' "$OUT"
assert_contains "conform yields 30 frames" '"frames":30' "$OUT"
OUT=$(mcp_call conform_timecode '{"timecode":"00:00:10:00","from_fps":30,"to_fps":24}')
assert_contains "10s @30 -> 24 preserves duration" '00:00:10:00' "$OUT"

echo "== adversarial =="
OUT=$(mcp_call frames_to_timecode '{"frames":10,"fps":0}')
assert_contains "fps=0 rejected" '-32602' "$OUT"
OUT=$(mcp_call timecode_to_frames '{"timecode":"aa:bb:cc:dd","fps":30}')
assert_contains "non-numeric timecode rejected" 'invalid timecode field' "$OUT"
OUT=$(mcp_call timecode_to_frames '{"timecode":"00:00:00:30","fps":30}')
assert_contains "frame field == fps rejected" 'out of range' "$OUT"
OUT=$(mcp_call timecode_to_frames '{"timecode":"1:2:3","fps":30}')
assert_contains "3-field timecode rejected" 'four fields' "$OUT"

guard_tests
mcp_harness_report
