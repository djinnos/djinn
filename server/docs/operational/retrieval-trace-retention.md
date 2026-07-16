# Retrieval-trace retention

`retrieval_traces` are retained for at least the exported
`MINIMUM_RETRIEVAL_TRACE_RETENTION_WINDOW` in
`djinn-db::repositories::retrieval_trace`. Its current value is 30 days and is
the single support contract for retrieval-trace history.

## Ownership

The control-plane/server maintenance caller owns scheduling and invoking
retrieval-trace pruning. It calls `RetrievalTraceRepository::prune_older_than`
with both a cutoff and an explicit UTC reference time. The repository rejects a
cutoff inside the protected window; it does not use database `now()` and it does
not make caller-level logging or fail-open decisions.

Both values must be valid UTC ISO-8601 timestamps. Eligible deletion is scoped
to the requested project and remains strictly earlier than the cutoff. At the
boundary, a trace timestamp equal to the cutoff is retained.

## Report coverage and diagnostics

Sibling epic `fv7a`'s task-run outcomes report has a maximum supported lookback
equal to `MINIMUM_RETRIEVAL_TRACE_RETENTION_WINDOW`. A request older than that
coverage must produce a truthful diagnostic rather than imply complete report
history.

While a trace remains retained, its explicit outcome is preserved. In
particular, `disabled_off` and `disabled_kill_switch` remain queryable as
deliberate suppression, never converted or reclassified as unrecorded. A
deliberate suppression becomes absent only after it is legitimately outside the
retention window and has been pruned.

This contract implements the retention phase in `design/u9hc-roadmap`.
