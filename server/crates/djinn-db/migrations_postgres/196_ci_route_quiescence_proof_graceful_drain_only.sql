-- Wave 5 of proposal `nafu`: retire `process_terminated` from the
-- `calling`-recovery quiescence vocabulary.
--
-- ---------------------------------------------------------------------------
-- Why a legal-looking value is being removed rather than given a producer
-- ---------------------------------------------------------------------------
--
-- 193 admitted two proofs that a former `calling` owner's provider futures were
-- gone: its own recorded `provider_actions_drained_at` stamp, and "the process
-- died". The first is a fact the former owner writes about itself. The second
-- never had a producer, and after auditing every signal available to a
-- recovering incarnation it cannot have one:
--
--   * The only observable that death could leave behind is the release of the
--     session-scoped coordinator advisory lock, because Postgres releases it
--     automatically when the holding backend goes away.
--   * That release is performed by POSTGRES, at backend termination, which is
--     strictly BEFORE the client process can observe anything. A live process
--     whose lock connection was dropped by a failover, a restart, a
--     `pg_terminate_backend`, or a blip therefore looks exactly like a dead one
--     for as long as it takes that process to notice and exit — and its CI
--     provider futures are still running throughout.
--   * Bounding that interval requires elapsed time, which the proposal's AC5
--     forbids in terms: "elapsed time, owner-lease expiry, cancellation, or
--     advisory-lock release without provider-action drain never authorizes
--     `calling` recovery".
--
-- So an involuntary lock release proves "the connection died", never "the
-- process died", and there is no third observable. A value the schema calls
-- legal but nothing can ever soundly produce is worse than no value at all:
-- the next reader wires a producer to it, which is precisely the elapsed-time
-- inference this wave removed. The remaining vocabulary is one real proof and
-- one honest absence.
--
-- The cost, stated plainly: a coordinator SIGKILLed mid-provider-call leaves
-- its charged `calling` row unrecoverable by anyone. The row is not silent —
-- `recover_calling_owner` writes a `live_owner_deferred` audit row on every
-- startup pass — but nothing terminalizes it. That is the conservative side of
-- the trade, and the one the proposal asks for whenever the evidence is
-- ambiguous.

-- Any row that recorded the retired spelling is rewritten to the absence it
-- now denotes. This is expected to touch zero rows: the CI routing feature is
-- runtime-gated off and no shipped writer ever emitted the value. It is not
-- optional tidiness, though — `CiQuiescenceProof::parse` rejects a spelling
-- outside the CHECK vocabulary, so a surviving row would make
-- `calling_recovery_audit` fail for its whole route rather than for itself.
UPDATE ci_route_calling_recoveries
   SET quiescence_proof = 'none'
 WHERE quiescence_proof = 'process_terminated';

ALTER TABLE ci_route_calling_recoveries
    DROP CONSTRAINT ci_route_calling_recoveries_quiescence_check;

ALTER TABLE ci_route_calling_recoveries
    ADD CONSTRAINT ci_route_calling_recoveries_quiescence_check
    CHECK (quiescence_proof IN ('graceful_drain', 'none'));
