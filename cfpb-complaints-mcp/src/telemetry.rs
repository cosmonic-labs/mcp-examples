//! Observability wiring.
//!
//! Baseline (always on): structured JSON logs on **stderr** with tracing
//! spans. The wasmCloud / Cosmonic Desktop host captures component stderr and,
//! when OpenTelemetry is enabled on the host
//! (`WASMCLOUD_OBSERVABILITY_ENABLED=true` / Cosmonic Control's
//! `opentelemetry.workload`), exports it to the configured OTLP collector as
//! structured log records. Nothing in the guest talks to a collector directly,
//! which keeps the component portable and its capability surface minimal.
//!
//! W3C trace context: the host propagates `traceparent` on incoming HTTP
//! requests. [`request_span`] records it on the request span so guest logs can
//! be correlated with the host's own traces in the collector.
//!
//! (The upstream template also offers a `wasi-otel` feature that reports guest
//! spans natively through `wasi:otel@0.2.0-rc.2`. It is omitted here to keep
//! the dependency graph to crates.io releases; stderr logs plus `traceparent`
//! cover Desktop's observability story.)

use std::sync::Once;

/// Initializes the tracing subscriber exactly once.
///
/// Log level is controlled with the standard `RUST_LOG` environment variable
/// (via `wasi:cli/environment`), defaulting to `info`.
pub fn init() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        use tracing_subscriber::layer::SubscriberExt as _;
        use tracing_subscriber::util::SubscriberInitExt as _;
        use tracing_subscriber::EnvFilter;

        let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
        let fmt_layer = tracing_subscriber::fmt::layer()
            .with_writer(std::io::stderr)
            .with_ansi(false)
            .json()
            .flatten_event(true)
            .with_current_span(true);

        tracing_subscriber::registry()
            .with(filter)
            .with(fmt_layer)
            .init();
    });
}

/// Creates the root span for one MCP HTTP exchange.
///
/// Records the HTTP method, path, and (if present) the W3C `traceparent`
/// header injected by the host, so stderr log records can be joined with the
/// host's OTLP traces.
pub fn request_span<B>(request: &http::Request<B>) -> tracing::Span {
    let traceparent = request
        .headers()
        .get("traceparent")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    let span = tracing::info_span!(
        "mcp.request",
        http.method = %request.method(),
        http.path = %request.uri().path(),
        mcp.method = request
            .headers()
            .get("mcp-method")
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default(),
        traceparent,
    );

    span
}
