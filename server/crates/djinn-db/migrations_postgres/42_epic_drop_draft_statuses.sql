-- Drop the `drafting` and `proposed` epic statuses.
--
-- These were the old "proposal-on-an-epic" mechanism (architect-drafted
-- `proposed` shells + a `drafting` staging state). The global proposals layer
-- now owns that pre-execution flow, so an epic is simply `open` → `closed`. The
-- "create without auto-dispatch" use that `drafting` served is already covered
-- by `auto_breakdown = false`. Existing drafting/proposed epics become `open`.

UPDATE epics SET status = 'open' WHERE status IN ('drafting', 'proposed');
