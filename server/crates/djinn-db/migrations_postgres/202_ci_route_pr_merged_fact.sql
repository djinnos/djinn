-- Follow-up to proposal `nafu`: make the per-merged-PR denominator computable.
--
-- ---------------------------------------------------------------------------
-- The defect
-- ---------------------------------------------------------------------------
--
-- `CiRouteReport::lead_sessions_per_merged_pr()` and
-- `worker_reopens_per_merged_pr()` are the two cost bounds the proposal exists
-- to watch, and both divide by `merged_prs`. That denominator was derived from
-- the route's ADJUDICATION column:
--
--     adjudicated = COALESCE(tier2_resolution, terminal_outcome)
--     merged_prs  = COUNT(DISTINCT pr_number) FILTER (WHERE adjudicated = 'merged')
--
-- and `close_routes_for_newer_outcome` writes that column with
--
--     SET tier2_lease_state = 'resolved',
--         tier2_resolution  = COALESCE(tier2_resolution, $4)
--     WHERE ... AND tier2_lease_state = 'open'
--
-- Both halves of that statement exclude exactly the population the numerator
-- counts. Once Lead adjudicates a route, `tier2_lease_state` is `'resolved'`
-- and `tier2_resolution` holds the verdict (`repair_reopened`, say), so a later
-- merge (a) matches no row, because the WHERE demands `'open'`, and (b) would
-- be discarded by the `COALESCE` even if it did. The row's `action_phase` is
-- `'terminal'` by then too, so the sibling `action_phase = 'reserved'` update
-- misses it as well: the merge lands nowhere at all.
--
-- The consequence is not an undercount, it is an empty intersection.
-- `merged_prs` can only ever count routes that NEVER reached a Lead
-- resolution, while `lead_invocations` counts precisely those that did. The
-- two can never describe the same population, so the ratio is structurally
-- uncomputable rather than merely wrong. Production bore this out: 13 PRs
-- merged overnight, 7 routes reached Lead, `merged_prs` read 0 and both ratios
-- read NULL.
--
-- ---------------------------------------------------------------------------
-- Why a column rather than a wider WHERE
-- ---------------------------------------------------------------------------
--
-- "Did this PR eventually merge" and "how did Lead adjudicate this route" are
-- different facts about different subjects -- one is about the pull request,
-- the other about one evidence identity's episode. Storing them in one column
-- means every write has to choose which fact to keep, and the adjudication is
-- the one that must win: it is the audit record for a Lead session that
-- actually ran, and `tier2_resolution` feeds `repair_reopens`,
-- `diagnostic_reopens`, `parks_with_cited_cause`, `supersedes` and
-- `worker_reopens`. Widening the update to overwrite it would fix the
-- denominator by corrupting five numerators.
--
-- So the merge gets its own cell. `pr_merged_at` is stamped by
-- `close_routes_for_newer_outcome` for every row of the PR REGARDLESS of lease
-- state or action phase -- a merge is true of a terminal row exactly as much as
-- of an open one -- and it is write-once (`COALESCE(pr_merged_at, now())`), so
-- a re-poll of an already-merged PR does not move the timestamp. Nothing about
-- adjudication changes: the two statements that resolve an open lease and
-- terminalize a reserved row are untouched.
--
-- A timestamp rather than a boolean because the moment the merge was first
-- observed is the part an operator cannot reconstruct afterwards, and because
-- NULL is then the honest spelling of "not observed merged" instead of a `false`
-- that cannot distinguish "not merged" from "never checked".
--
-- Deliberately NOT added: a matching `pr_passed_at`. The report's `passed` is a
-- count of routes that terminalized on a pass, not of PRs, and no metric
-- divides by it. A column no query reads is dead schema that invites a future
-- reader to wire it to something it was never validated for.
ALTER TABLE ci_route_attempts ADD COLUMN pr_merged_at TIMESTAMPTZ NULL;

-- Backfill the merges that DID land in the adjudication column, i.e. every row
-- whose lease happened to still be open when its PR merged. Those are the only
-- merges the old reading could see, and dropping them on the migration would
-- silently reset history at the cutover.
--
-- `tier2_resolved_at`/`terminalized_at` rather than `now()`: the stamp means
-- "when the merge was observed", and for these rows that instant is recorded.
-- `updated_at` is the last-resort fallback and is NOT NULL, so the stamp is
-- never left NULL on a row this predicate matched.
UPDATE ci_route_attempts
   SET pr_merged_at = COALESCE(tier2_resolved_at, terminalized_at, updated_at)
 WHERE COALESCE(tier2_resolution, terminal_outcome) = 'merged';

COMMENT ON COLUMN ci_route_attempts.pr_merged_at IS
    'When this route''s PR was first observed merged. A fact about the pull '
    'request, independent of how Lead adjudicated the route: it is stamped '
    'regardless of lease state or action phase and never overwrites an '
    'adjudication. NULL means no merge has been observed. This is the '
    'denominator of the per-merged-PR cost ratios.';
