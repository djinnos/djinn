-- Track credentials the provider rejected as revoked/invalid (a 401 during a
-- task/chat run), so the UI can show the provider as "Disconnected — <reason>"
-- F5-safely (the row is the source of truth, not a transient SSE event) and so
-- dispatch can park the owner's tasks until they reconnect.
--
-- Both columns are nullable with no backfill: an existing row with
-- revoked_at IS NULL is treated as not-revoked. Reconnecting (re-upserting the
-- credential) clears both columns.
ALTER TABLE credentials ADD COLUMN revoked_at TEXT;
ALTER TABLE credentials ADD COLUMN revoked_reason TEXT;
