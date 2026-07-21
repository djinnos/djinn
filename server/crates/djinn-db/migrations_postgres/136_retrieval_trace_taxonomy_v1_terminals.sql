-- Additive only: historical and compatibility rows remain NULL/unknown.
ALTER TABLE retrieval_traces
    ADD COLUMN IF NOT EXISTS knowledge_trace_taxonomy_version INT NULL,
    ADD COLUMN IF NOT EXISTS terminal_state VARCHAR(16) NULL,
    ADD COLUMN IF NOT EXISTS terminal_at VARCHAR(64) NULL,
    ADD COLUMN IF NOT EXISTS candidate_count INT NULL,
    ADD COLUMN IF NOT EXISTS injected_count INT NULL,
    ADD COLUMN IF NOT EXISTS confidence_filtered_count INT NULL,
    ADD COLUMN IF NOT EXISTS not_top_k_count INT NULL,
    ADD COLUMN IF NOT EXISTS oversized_skipped_count INT NULL,
    ADD COLUMN IF NOT EXISTS budget_pruned_count INT NULL;
