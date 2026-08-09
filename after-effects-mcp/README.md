# after-effects-mcp

An MCP server that **drives a live Adobe After Effects instance** — creating
compositions, layers, keyframes, expressions, masks, cameras and effects, and
rendering frames — built as a WebAssembly component for Cosmonic Desktop.

After Effects has no remote API, so the component reaches it the way a
sandboxed component can: a ScriptUI panel runs *inside* After Effects and polls
this server. A tool call queues a command, the panel claims it on its next
poll, executes it against the ExtendScript DOM, and posts the result back.

```
MCP client ──tools/call──▶ component ──queue──▶ wasi:keyvalue
                               ▲                     │
                               └──result── panel ◀───poll──┘
                                       (inside After Effects)
```

Built with [mcp-server-template-rs](https://github.com/cosmonic-labs/mcp-server-template-rs):
rmcp 3.x, MCP spec 2026-07-28, exports `wasi:http/handler@0.3.0`.

## Setup

1. **Deploy the workload** (see [Deploy](#deploy-on-cosmonic-desktop) below).
2. **Install the panel:** `./install-bridge.sh` — copies
   `bridge/mcp-bridge-auto.jsx` into After Effects' *ScriptUI Panels* folder.
   Use `./install-bridge.sh --remote` to install the copy the running workload
   serves at `/bridge/panel.jsx`, which is guaranteed to match the deployment.
3. **Allow network access:** After Effects → Settings → Scripting &
   Expressions → enable *Allow Scripts to Write Files and Access Network*, then
   restart After Effects.
4. **Open the panel:** Window → `mcp-bridge-auto.jsx`. Leave it open. If your
   deployment uses a different ingress name, set it in the panel's *Host* field.

`bridge_status` tells you whether the panel is connected; `get_help` repeats
these steps and lists effect match names.

## Tools

### Live control (needs the panel)

| Tool | Purpose |
|---|---|
| `bridge_status` | Is the panel connected, and what is it doing? |
| `get_results` | Result of the last command — use after a call times out |
| `get_help` | Setup steps, effect match names, batching guidance |
| `get_project_info` / `list_compositions` / `get_layer_info` | Read the project |
| `create_composition` / `set_composition_properties` / `delete_composition` | Compositions |
| `create_text_layer` / `create_shape_layer` / `create_solid_layer` / `add_image_layer` / `create_camera` | Create layers |
| `set_layer_properties` / `batch_set_layer_properties` | Transform, timing, blend mode, text |
| `duplicate_layer` / `delete_layer` | Layer lifecycle |
| `set_layer_keyframe` / `set_layer_expression` | Animation |
| `set_layer_mask` | Masks, by rectangle or arbitrary outline |
| `apply_effect` / `apply_effect_template` | Effects |
| `run_batch` | Many commands in one round trip, one undo group |
| `save_frame_png` / `save_project` | Render a frame; save the project |
| `bridge_test_effects` | Smoke-test the bridge |
| `run_script` | Escape hatch onto the panel's dispatch table |

### Helpers (no panel required)

| Tool | Purpose |
|---|---|
| `frames_to_timecode` | Frame number → `HH:MM:SS:FF` (non-drop) at a given fps |
| `timecode_to_frames` | `HH:MM:SS:FF` → frame number |
| `interpolate` | Eased value between two keyframes at `t` |
| `expression` | Generate an AE expression (wiggle, loopOut, time-drive, bounce) |
| `effect_template` | Expand a named effect to its ADBE match-name(s) and defaults |
| `hex_to_rgb` | Hex color → AE 0..1 RGBA floats and 0..255 values |

These answer from the request alone, so they keep working with the panel
closed. Use them to work out keyframe values, easing and colours *before*
sending anything to After Effects.

All tools return `structuredContent` alongside text.

## Working with it effectively

- **Batch.** Every live tool call costs at least one ~2s poll cycle. Building a
  scene one call at a time is dominated by that latency; `run_batch` collapses
  a whole scene into one cycle and one undo group. Roughly 100 commands per
  batch.
- **Verify visually.** `save_frame_png` renders a frame to disk — the only way
  to actually see what was built.
- **Prefer names to indices.** `compName` beats `compIndex`: project item
  indices shift whenever footage is imported.
- **Two failure modes are distinguished on purpose.** A command queued with no
  panel listening comes back as a tool *error* saying so — the command is
  queued but nothing happened and nothing will until someone opens the panel.
  A command that outran its wait comes back as `status: "pending"`, because the
  panel may still be working; fetch it later with `get_results`.

## Build

```console
$ cargo build --release
```

Produces `target/wasm32-wasip2/release/after_effects_mcp.wasm`, which exports
`wasi:http/handler@0.3.0` and imports `wasi:keyvalue/store@0.2.0-draft`.

## Deploy on Cosmonic Desktop

Deployment is via [Cosmonic Desktop](https://cosmonic.com/docs/desktop): apply
[`deploy/workload.yaml`](deploy/workload.yaml) for the published image, or
promote a local build and apply `workload.yaml` — see the
[repo README](../README.md#running-an-example-on-cosmonic-desktop).

Two things differ from a pure-compute example:

- **`wasi:keyvalue` is required.** The bridge queue connects two separate HTTP
  requests — a tool call and a panel poll — which may land on different
  instances, so it cannot live in component memory. Without the `keyvalue`
  `hostInterface` the component will not instantiate.
- **`poolSize: 4`**, so a tool call blocked waiting for a result never starves
  the panel's polls. (The `/bridge/*` routes are also served without the MCP
  request lock, so a poll is never queued behind an in-flight tool call.)

Desktop's default bucket is host-wide, so `MCP_BRIDGE_KEY_PREFIX` namespaces
this server's keys. Change it if you run a second After Effects bridge on the
same host, or they will steal each other's queued commands.

Check it is up:

```console
$ curl -H 'Host: after-effects-mcp.localhost' http://127.0.0.1:8200/healthz
ok
```

## Endpoints

| Route | Used by |
|---|---|
| `POST /` | MCP streamable HTTP transport |
| `GET /bridge/command` | Panel poll — claims the pending command |
| `POST /bridge/result` | Panel result report |
| `GET /bridge/panel.jsx` | Panel source, for `install-bridge.sh --remote` |
| `GET /healthz` | Health check |

## Test

```console
$ ./scripts/e2e.sh
```

No copy of After Effects is needed. `wasmtime serve` provides every WASI
interface the component needs except `wasi:keyvalue`, so the harness composes
[`testing/kv-stub`](testing/kv-stub) in with `wac plug` and then drives the
bridge endpoints with curl, playing the part of the panel: poll, claim, answer.
Covers the shared framework suite (protocol, spec enforcement, robustness, Host
guard), the pure-compute tools and their adversarial cases, and the full bridge
protocol — panel handshake, supersession, round trip, both panel failure
spellings, and argument mapping.
