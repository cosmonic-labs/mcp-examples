# Cosmonic MCP examples

Four example **Model Context Protocol (MCP) servers**, each built as a
**WebAssembly component** with
[mcp-server-template-rs](https://github.com/cosmonic-labs/mcp-server-template-rs)
and deployable to [Cosmonic Desktop](https://cosmonic.com). They are worked
examples of the MCP-server "factory": the same template, the same guardrails,
four different shapes of server.

Every example:

- speaks **MCP 2026-07-28** (stateless streamable HTTP, header routing, structured output),
- exports **`wasi:http/handler@0.3.0`** (WASI p3),
- ships a hermetic **`scripts/e2e.sh`** test suite (run in CI),
- deploys to Cosmonic Desktop with a `workload.yaml` (local builds) and a
  `deploy/workload.yaml` (the published image).

## The four examples

| Example | What it shows | Tools |
|---|---|---|
| [**after-effects-mcp**](after-effects-mcp/) | Pure-compute helper server | frame/timecode conversion, keyframe easing, AE expression generation, effect-template lookup, hex→RGBA |
| [**premiere-mcp**](premiere-mcp/) | Pure-compute with tricky domain math | SMPTE **drop-frame** timecode, offset, sequence duration, frame-rate conform |
| [**sec-edgar-mcp**](sec-edgar-mcp/) | **Outbound HTTP**, no key, caching | SEC EDGAR company facts & filing history (ticker→CIK, `SEC_USER_AGENT`) |
| [**fred-mcp**](fred-mcp/) | **Outbound HTTP with an API key** | FRED economic-data search, series metadata & observations (`FRED_API_KEY`) |

Together they cover the whole framework surface: pure compute, domain-math
correctness, outbound APIs, caching, secrets, and the concurrency/robustness
patterns the template enforces (bounded buffers, deadlines, panic-free numeric
code, DNS-rebinding and SSRF guards).

The reusable test harness is [`scripts/mcp_e2e_lib.sh`](scripts/mcp_e2e_lib.sh):
each example's `e2e.sh` sources it for the framework-level checks (protocol,
spec enforcement, robustness, Host guard) and adds its own tool cases.

## Build & test any example

Prerequisites: Rust 1.90+ with the wasip2 target (`rustup target add
wasm32-wasip2`); the e2e suite additionally needs `wasm-tools`,
`wasmtime` ≥ 46 (test harness only), `python3`, and `curl`.

```console
$ cd after-effects-mcp        # or any example
$ cargo build --release       # -> target/wasm32-wasip2/release/<name>.wasm
$ ./scripts/e2e.sh            # runs the hermetic test suite
```

`cargo test` is **not** used — the build target is wasm, so `scripts/e2e.sh`
is the test entry point.

## Running an example on Cosmonic Desktop

Deployment is via [Cosmonic Desktop](https://cosmonic.com/docs/desktop),
which runs WASI p3 components natively and routes ingress by HTTP `Host`
header. Each example ships two manifests: `workload.yaml` for a locally
built/promoted image, and `deploy/workload.yaml` for the published image.
The daemon's control socket is
`~/Library/Application Support/Cosmonic/cosmonicd.sock`.

1. **Register** the project directory (it has a `.wash/config.yaml`):

   ```console
   $ SOCK="$HOME/Library/Application Support/Cosmonic/cosmonicd.sock"
   $ curl --unix-socket "$SOCK" -X POST http://localhost/v1/projects \
       -H 'Content-Type: application/json' \
       -d '{"path":"'"$PWD"'"}'
   ```

2. **Promote** — build and push to the daemon's built-in registry. This
   returns a **digest-pinned image reference**; use it in the workload.

   ```console
   $ curl --unix-socket "$SOCK" -X POST \
       http://localhost/v1/projects/<name>/promote \
       -H 'Content-Type: application/json' \
       -d '{"ref":"oci-registry.localhost:8200/<name>:0.1.0","insecure":true}'
   ```

   > Note: at the time of writing the `cosmonic_promote` MCP tool sends the
   > wrong field name (`reference` instead of `ref`); the socket call above is
   > the reliable path.

3. **Apply** the workload — `workload.yaml` with the digest-pinned image
   from promote, or `deploy/workload.yaml` for the published image — via the
   Cosmonic Desktop UI, the `cosmonic_apply_workload` MCP tool, or
   `POST /v1/workloads`. Key fields:

   ```yaml
   spec:
     hostInterfaces:
       - namespace: wasi
         package: http
         interfaces: ["handler"]        # p3 handler — NOT incoming-handler
         config: { host: <name>.localhost }
     components:
       - name: mcp
         image: oci-registry.localhost:8200/<name>:0.1.0@sha256:…
         localResources:
           environment:
             config:
               MCP_ALLOWED_HOSTS: "<name>.localhost"   # matches the ingress host
           allowedHosts: [ … ]          # outbound allow-list (deny-all if empty)
   ```

4. **Call it** through the ingress (routes by `Host`):

   ```console
   $ curl -X POST http://<name>.localhost:8200/ \
       -H 'Content-Type: application/json' \
       -H 'Accept: application/json, text/event-stream' \
       -H 'MCP-Protocol-Version: 2026-07-28' \
       -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2026-07-28","capabilities":{},"clientInfo":{"name":"you","version":"0"}}}'
   ```

   A successful response is SSE-framed (`data: {…"protocolVersion":"2026-07-28"…}`).
   Tool calls add `Mcp-Method: tools/call`, `Mcp-Name: <tool>`, and a
   per-request `_meta` (see each example's README for a full call).

### Deployment notes

- **Host header guard**: the template accepts only localhost by default. On
  Desktop, set `MCP_ALLOWED_HOSTS` to the workload's ingress host, or requests
  are rejected with `Forbidden`.
- **Outbound allow-list**: `allowedHosts` is deny-all when empty. List every
  upstream host a tool dials (e.g. `www.sec.gov`, `data.sec.gov`,
  `api.stlouisfed.org`).
- **Secrets**: for API keys, register a Cosmonic secret and reference it with
  `secretFrom` rather than inlining the value — see `fred-mcp`.
- **Don't overwrite existing workloads**: apply is idempotent by
  `namespace/name`, and the ingress `host` is global. Pick a fresh
  namespace/name/host if one is taken.

## Building your own

These examples are the reference implementations for the
**`building-mcp-servers`** skill that ships in the
[template repo](https://github.com/cosmonic-labs/mcp-server-template-rs/tree/main/skills/building-mcp-servers).
Start from the template, follow the skill's phases (scaffold → tools →
gates → e2e → deploy), and consult its living "Pitfalls" list — most of which
was learned building exactly these four servers.

## License

Apache-2.0
