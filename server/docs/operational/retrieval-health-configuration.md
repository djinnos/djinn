# Retrieval-Health Environment Overrides

This document is the operator-facing configuration reference for the
retrieval-health checks surfaced in `memory_health` and the Doctor. It defines
the canonical environment variables, their defaults and inclusive bounds, the
deprecated compatibility aliases, the deterministic precedence rule, and the
startup-failure behaviour for malformed or out-of-range values.

The retrieval-health check emits the `memory.retrieval_zero_result` finding per
project only when the completed-query count in the window is at least the
configured query floor **and** the zero-result rate is strictly greater than the
configured threshold (equality against the threshold is considered healthy).

## Canonical variables

These are the documented, preferred variable names:

| Variable                                    | Type   | Default | Inclusive bounds | Purpose                                                                 |
|---------------------------------------------|--------|---------|------------------|-------------------------------------------------------------------------|
| `DJINN_RETRIEVAL_HEALTH_WINDOW_HOURS`       | `u64`  | `24`    | `1`..=`168`       | Rolling window over which retrieval counts are aggregated, in hours.    |
| `DJINN_RETRIEVAL_ZERO_RESULT_THRESHOLD`     | `f64`  | `0.50`  | `0.0`..=`1.0`     | Zero-result rate above which a finding is emitted (strictly greater).   |
| `DJINN_RETRIEVAL_MINIMUM_QUERIES`           | `u64`  | `20`    | `1`..=`100000`    | Minimum completed queries in the window before a finding may be emitted. |

The window variable covers up to 7 days (`168` hours) at its upper bound. The
query floor upper bound is `100000`; values above it are rejected.

## Deprecated fallback aliases

Two variables shipped under an earlier naming convention remain supported as
**deprecated fallback aliases**. They are not preferred names — new deployments
and documentation should use the canonical names above exclusively.

| Deprecated alias                                     | Canonical variable                      |
|------------------------------------------------------|-----------------------------------------|
| `DJINN_RETRIEVAL_HEALTH_ZERO_RESULT_THRESHOLD`       | `DJINN_RETRIEVAL_ZERO_RESULT_THRESHOLD` |
| `DJINN_RETRIEVAL_HEALTH_QUERY_FLOOR`                 | `DJINN_RETRIEVAL_MINIMUM_QUERIES`       |

### Precedence when both names are set

Variable selection is deterministic and ordered:

1. **Canonical present** — the canonical value is selected; the deprecated alias
   is **ignored entirely** (it is never parsed).
2. **Canonical absent, alias present** — the deprecated alias value is selected.
3. **Both absent** — the documented default is used.

Because the alias is not parsed when the canonical variable is set, a malformed
or out-of-range alias cannot affect a valid canonical setting. Only the
selected source is validated against the bounds above.

## Startup-failure behaviour

The control plane parses the retrieval-health configuration exactly once at
process startup (`server/src/main.rs`, `RetrievalHealthConfig::from_env`). Any
**selected** value that is malformed (non-numeric) or outside the documented
inclusive bounds causes the server to log the error and **refuse to start**
(`exit(1)`).

Examples of values that prevent startup:

- `DJINN_RETRIEVAL_HEALTH_WINDOW_HOURS=0` — below the minimum of `1`.
- `DJINN_RETRIEVAL_HEALTH_WINDOW_HOURS=169` — above the maximum of `168`.
- `DJINN_RETRIEVAL_ZERO_RESULT_THRESHOLD=1.1` — above the maximum of `1.0`.
- `DJINN_RETRIEVAL_ZERO_RESULT_THRESHOLD=high` — non-numeric.
- `DJINN_RETRIEVAL_MINIMUM_QUERIES=0` — below the minimum of `1`.
- `DJINN_RETRIEVAL_MINIMUM_QUERIES=100001` — above the maximum of `100000`.

The same bounds and malformed-value behaviour applies to the deprecated aliases
when they are the selected source. This makes the failure mode reproducible and
discoverable at startup rather than as a silent degradation at runtime.

## Source of truth

The implementation, bounds constants, and parsing logic live in
`server/crates/djinn-core/src/doctor/checks/retrieval.rs`
(`RetrievalHealthConfig`, `from_env`). The Doctor check and `memory_health`
consume the single startup-parsed config value projected through `AppState`.

## Related

- Proposal `jrcs`: Add always-on retrieval-health metrics.
- Epic `1adc`: Align retrieval-health environment overrides with the graduated
  contract.
- Deploy configuration reference: [`docs/deploy/configuration.md`](../../../docs/deploy/configuration.md).
