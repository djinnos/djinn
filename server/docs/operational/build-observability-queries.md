# Build Observability PromQL

These queries use the bounded-label build telemetry exported by Djinn. Evaluate
all queries over a 15-minute range (`[15m]`). They intentionally aggregate away
Cargo's bounded `exit` label and retain only the bounded `role` label where a
role breakdown is useful.

## Cargo check duration

`djinn_cargo_invocation_seconds_bucket` has bounded `kind` (`check`, `clippy`,
`test`, `build`, `other`) and `exit` (`ok`, `fail`, `cancelled`) labels. The
following queries include all check outcomes, so a rising failure rate cannot
silently improve the reported check latency.

### p50

```promql
histogram_quantile(
  0.50,
  sum by (le) (
    rate(djinn_cargo_invocation_seconds_bucket{kind="check"}[15m])
  )
)
```

### p95

```promql
histogram_quantile(
  0.95,
  sum by (le) (
    rate(djinn_cargo_invocation_seconds_bucket{kind="check"}[15m])
  )
)
```

## Admitted build-slot queue wait p95

`djinn_build_slot_queue_wait_seconds_bucket` has the bounded `outcome` label.
Filter to successfully admitted requests; cancelled and shutdown requests do
not describe time until an admitted build can start.

```promql
histogram_quantile(
  0.95,
  sum by (le) (
    rate(djinn_build_slot_queue_wait_seconds_bucket{outcome="admitted"}[15m])
  )
)
```

## Provider-wait share by role

`djinn_agent_session_phase_seconds_total` is a bounded counter with `phase`
(`provider_wait` or `tool_execution`) and `role` (`worker`, `reviewer`,
`planner`, or `refinement`). This expression uses the same `sum by (role)` on
both terms, so Prometheus matches each numerator with its own role's total.

```promql
sum by (role) (
  rate(djinn_agent_session_phase_seconds_total{phase="provider_wait"}[15m])
)
/
(
  sum by (role) (
    rate(djinn_agent_session_phase_seconds_total{phase="provider_wait"}[15m])
  )
  +
  sum by (role) (
    rate(djinn_agent_session_phase_seconds_total{phase="tool_execution"}[15m])
  )
)
```

A role whose provider-wait and tool-execution rates both sum to zero produces
**no sample/series**. This is deliberate: the share is undefined when there was
no measured phase time. Do not add `or vector(0)`, `clamp_min`, or another
zero-coercion fallback to this query.

## Deterministic fixture proof

`fixtures/build-observability-promql.json` contains two endpoint samples for
the fixed 900-second range. Run the repository-local, dependency-free check:

```sh
perl server/scripts/test-build-observability-promql.pl
```

The check evaluates histogram quantiles using Prometheus's bucket interpolation
rule and counter rates from those samples. It proves cargo-check p50 = `1.0s`,
cargo-check p95 = `1.9s`, admitted queue-wait p95 = `0.875s`, provider shares
for `worker` = `0.3` and `reviewer` = `0.5`, and absent results for the
zero-denominator `planner` and `refinement` roles. It starts no Prometheus
server and contacts no external service.
