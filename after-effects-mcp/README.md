# after-effects-mcp

An MCP server of **After Effects helper tools** — frame/timecode conversion,
keyframe easing, expression generation, effect-template lookup, and color
conversion — built as a WebAssembly component for Cosmonic Desktop.

These tools are **pure compute**: they do the fiddly motion-graphics
arithmetic and code generation an artist reaches for constantly, with no host
application or network access. (The upstream reference server drives a *live*
After Effects instance over a polling bridge; this example distills its
hard-won domain knowledge — easing constants, effect match-names, color
conventions — into a self-contained, deployable component.)

Built with [mcp-server-template-rs](https://github.com/cosmonic-labs/mcp-server-template-rs):
rmcp 3.x, MCP spec 2026-07-28, exports `wasi:http/handler@0.3.0`.

## Tools

| Tool | Purpose |
|---|---|
| `frames_to_timecode` | Frame number → `HH:MM:SS:FF` (non-drop) at a given fps |
| `timecode_to_frames` | `HH:MM:SS:FF` → frame number |
| `interpolate` | Eased value between two keyframes at `t` (linear, ease-in/out, cubic, back) |
| `expression` | Generate an AE expression (wiggle, loopOut, time-drive, bounce) |
| `effect_template` | Expand a named effect to its ADBE match-name(s) and defaults |
| `hex_to_rgb` | Hex color → AE 0..1 RGBA floats and 0..255 values |

All tools return `structuredContent` (machine-readable) alongside text.

## Build

```console
$ cargo build --release
```

Produces `target/wasm32-wasip2/release/after_effects_mcp.wasm`, which exports
`wasi:http/handler@0.3.0`.

## Deploy on Cosmonic Desktop

Deployment is via [Cosmonic Desktop](https://cosmonic.com/docs/desktop):
apply [`deploy/workload.yaml`](deploy/workload.yaml) for the published image,
or promote a local build and apply `workload.yaml` — see the
[repo README](../README.md#running-an-example-on-cosmonic-desktop) for the
full walkthrough. Then call it through the ingress:

```console
$ curl -X POST http://after-effects-mcp.localhost:8200/ \
    -H 'Content-Type: application/json' \
    -H 'Accept: application/json, text/event-stream' \
    -H 'MCP-Protocol-Version: 2026-07-28' \
    -H 'Mcp-Method: tools/call' -H 'Mcp-Name: frames_to_timecode' \
    -d '{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"frames_to_timecode","arguments":{"frames":48,"fps":24},"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{}}}}'
```

## Test

```console
$ ./scripts/e2e.sh
```

Builds, serves under wasmtime, and runs the shared framework suite (protocol,
spec enforcement, robustness, Host guard) plus tool-specific and adversarial
cases (fps ≤ 0, out-of-range frame fields, non-UTF-8-safe truncation, unknown
enum variants, hex edge cases).
