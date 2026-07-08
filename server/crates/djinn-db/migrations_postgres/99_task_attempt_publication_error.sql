-- Migration 99: Add durable github_publication_error to task_attempts (m116).
--
-- When a WorkerDone mirror push succeeds but the subsequent GitHub branch
-- publication fails, the supervisor emits a structured tracing warning but
-- has no durable place to record the concise error string.  This migration
-- adds a nullable `github_publication_error` column so that the publication
-- failure is durable and can be surfaced through task CI head reconciliation
-- reads without scraping free-form log prose.
--
-- Additive only: no existing columns are dropped or renamed.

ALTER TABLE task_attempts
    ADD COLUMN IF NOT EXISTS github_publication_error VARCHAR(2000) NULL;
