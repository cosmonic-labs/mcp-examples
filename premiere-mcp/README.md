# premiere-mcp

An MCP server of **Premiere Pro timecode & sequence tools** — SMPTE timecode
conversion with correct **drop-frame** handling, timecode offset, sequence
duration, and frame-rate conform — built as a WebAssembly component for
Cosmonic Desktop.

Pure compute, no host application. The headline feature is getting
**drop-frame timecode** right: at 29.97/59.94 fps, drop-frame *labels* skip two
(or four) frame numbers every minute except every tenth minute, so the naive
`frames = hh*3600*fps + …` is wrong by ~3.6 seconds per hour. These tools
implement the SMPTE conversion exactly (verified against the standard
checkpoints: frame 1800 → `00:01:00;02`, 17982 → `00:10:00;00`, 107892 →
`01:00:00;00`).

Built with [mcp-server-template-rs](https://github.com/cosmonic-labs/mcp-server-template-rs):
rmcp 3.x, MCP spec 2026-07-28, exports `wasi:http/handler@0.3.0`.

## Tools

| Tool | Purpose |
|---|---|
| `frames_to_timecode` | Frame number → SMPTE timecode (`drop_frame` for 29.97/59.94) |
| `timecode_to_frames` | SMPTE timecode → frame number (drop-frame inferred from `;`) |
| `offset_timecode` | Add/subtract frames from a timecode (clamped at 0) |
| `sequence_duration` | Sum per-clip frame durations into a total + timecode |
| `conform_timecode` | Retime a timecode between frame rates, preserving real duration |

`;` in a timecode marks drop-frame (`00:01:00;02`); `:` marks non-drop. The
tools reject impossible drop-frame labels (e.g. `00:01:00;00`, whose frames
`;00`/`;01` are dropped) and refuse `drop_frame` for non-NTSC rates.

## Build

```console
$ cargo build --release
```

## Run locally (wasmtime)

```console
$ wasmtime serve -Sp3,cli,http --addr 127.0.0.1:8080 \
    target/wasm32-wasip2/release/premiere_mcp.wasm
$ curl -X POST http://127.0.0.1:8080/ \
    -H 'Content-Type: application/json' \
    -H 'Accept: application/json, text/event-stream' \
    -H 'MCP-Protocol-Version: 2026-07-28' \
    -H 'Mcp-Method: tools/call' -H 'Mcp-Name: frames_to_timecode' \
    -d '{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"frames_to_timecode","arguments":{"frames":1800,"fps":29.97,"drop_frame":true},"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{}}}}'
# -> "timecode":"00:01:00;02"
```

## Run on Cosmonic Desktop

See the [repo README](../README.md#running-an-example-on-cosmonic-desktop).
Reach it at `http://premiere-mcp.localhost:8200/`.

## Test

```console
$ ./scripts/e2e.sh
```

45 cases: the shared framework suite plus drop-frame checkpoints, roundtrips,
drop-frame validation, offset/duration/conform, and adversarial inputs.
