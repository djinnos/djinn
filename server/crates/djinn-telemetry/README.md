# djinn-telemetry

`djinn-telemetry` owns the process-global metrics recorder and the small metric facade used by Djinn crates.

## Facade/exporter choice

This crate uses the [`metrics`](https://crates.io/crates/metrics) facade pinned to `0.24.6` with [`metrics-exporter-prometheus`](https://crates.io/crates/metrics-exporter-prometheus) pinned to `0.18.3`.

The `metrics` facade keeps call sites synchronous and dependency-light: hot paths increment counters without importing server state, without `async`, and without holding application locks. `metrics-exporter-prometheus` provides an in-process recorder plus a `PrometheusHandle` that can render Prometheus text format directly, which lets `djinn-server` expose `/metrics` through its existing Axum router without binding the exporter's own HTTP listener.

The exporter dependency disables default features because Djinn only needs recorder installation and text rendering; HTTP listener and push-gateway support remain out of scope for the server-owned `/metrics` route.
