# Offline extraction replay report

- Corpus: `crates/djinn-slot/tests/fixtures/extraction_replay`
- Cases: 24/24 (1.0000)
- Dedup precision: 1.0000
- Dedup confusion: TP=0 FP=0 TN=24 FN=0
- Revision operations: emitted={}; applied={}; refused={}

This checked-in baseline is produced by the isolated replay path. The replay seam uses the production extraction parser and scoring decisions, then stops before the guarded revision mutation boundary; therefore it cannot write the live corpus. Revision-operation buckets are explicit even when the curated corpus emits no revision proposal.
