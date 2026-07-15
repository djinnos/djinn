# Memory intent planner replay rollout gate

The session-start memory intent planner remains **off by default**. `MemoryIntentPlannerConfig::default()` keeps `enabled` false, and this replay corpus is not authorization to change that setting.

## Required deterministic evidence

Before any separate proposal enables the planner by default, the checked-in corpus at `crates/djinn-agent/tests/fixtures/memory_intent_planner/replay_cases.json` and its table-driven harness must pass. Every case enters the production `assemble_prompt_context` → knowledge-loading boundary with only injected fake attributed-host and planned-search dependencies. The corpus performs no live provider/network/credential call and does not mutate process environment; its ephemeral repository exists solely to exercise the real scope-overlap and packing path.

The gate covers successful typed planning; disabled no-work; timeout; provider error; malformed payload; unknown type; wrong count; Phase-1 query-style invalidity; accounting finalization failure; duplicate collapse; scope-budget exhaustion; and untruncated resume-compaction input. It asserts byte-stable context, scope-first budget/caps/dedupe, planner-query/rank order, and durable attempted-usage outcomes. Every fail-open planner path renders the same scope-only baseline; accounting-finalization failure suppresses planner injection.

Run the focused gate from `server/`:

```bash
cargo test -p djinn-agent --lib memory_intent_planner_replay_tests
```

Passing replay fixtures only establish deterministic safety and regression coverage. A future default-on proposal must additionally provide recall-trace evidence of production effectiveness; this gate does not make an effectiveness claim.

## F6 reconciliation

The planner module records the deliberate F6 reconciliation in its module documentation. It follows F6's pure prompt/parser plus injectable-fake boundary, but does **not** import `djinn_graph::query_planner`: F6 is synchronous code-search expansion with score-union semantics, while this planner has typed async memory queries and must leave the existing memory-search scoring pipeline unchanged.
