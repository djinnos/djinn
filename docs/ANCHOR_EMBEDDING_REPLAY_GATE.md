# Anchor embedding replay validation gate

This runbook is the local, deterministic cut-over gate for x72l anchor-based memory embeddings. It compares the legacy full-content embedding document path with the anchor-preferred embedding document/hash path on fixed representative memory-retrieval cases. It is intentionally reviewable without production traffic, operator credentials, deployed Qdrant, embedding-provider access, Docker, Kubernetes, or any external service.

## What is compared

The fixture lives in `server/crates/djinn-db/src/repositories/note/replay_validation.rs` and contains representative `case`, `pattern`, and `pitfall` notes plus task-style retrieval queries for:

- reviewer/verification retry scope control;
- deterministic parity/replay rollout evidence;
- noisy full-content memory retrieval;
- external-infrastructure proof being moved to runbooks; and
- session-start prompt-budget measurement for anchor retrieval.

For every fixture note, the harness builds two deterministic documents:

1. **Legacy full-content document** — `title + type + tags + full content` via `legacy_embedding_document_text`.
2. **Anchor-preferred document** — `title + retrieval_anchor` via `embedding_document_text`, with the same fallback behavior used by the live embedding hash path.

The harness uses a local deterministic token-overlap scorer instead of a live embedding model or Qdrant. The goal is to replay relevance/rank semantics for the document-shape change, not to prove behavior against production vector traffic.

## Non-regression criteria

A replay passes only when all criteria hold:

- the anchor-preferred top hit is relevant for every query;
- anchor recall@3 is greater than or equal to legacy recall@3 for every query;
- anchor reciprocal rank is greater than or equal to legacy reciprocal rank for every query;
- the bounded report includes only the top 3 hits per query; and
- the measured session-start injected-note payload does not grow.

If any regression appears, keep the default cut-over gated. Do not widen these criteria to hide regressions; update the fixture/report with the failing case and land the semantic fix separately.

## Prompt-budget measurement

The prompt-budget check renders the session-start note payload from the selected note summaries/bodies, not from the embedding document text. Anchors affect retrieval selection and embedding hashes only; they are not appended to injected note bodies. The committed fixture currently expects the anchor flow to leave the rendered injected payload unchanged or smaller than the legacy flow.

## Focused replay command

Run the focused unit tests from the repository root:

```bash
cd server && cargo test -p djinn-db repositories::note::replay_validation --all-features
```

The tests generate the replay report, assert the non-regression criteria, assert the report remains bounded/reviewable, and assert prompt-budget impact is unchanged or reduced.

## Reviewer checklist

- [ ] Inspect `server/crates/djinn-db/src/repositories/note/replay_validation.rs` and confirm the fixture covers representative case/pattern/pitfall notes.
- [ ] Confirm the harness compares `legacy_embedding_document_text` against `embedding_document_text` rather than production traffic.
- [ ] Confirm the report records bounded rank/relevance output and explicit pass/fail reasons.
- [ ] Confirm prompt-budget measurement is based on rendered injected note summaries/bodies and does not include anchor text as extra prompt payload.
- [ ] Run the focused command above, or inspect CI for it, without provisioning Qdrant, external embedding providers, operator credentials, or production traffic.
- [ ] Treat any replay failure as a cut-over blocker until the fixture/report documents the regression and the embedding behavior is fixed.
