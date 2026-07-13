-- The kanban board groups its swimlanes by proposal, so epic reads and SSE
-- payloads must carry the epic→proposal linkage without a join per event.
-- `proposal_epics` stays the canonical link table (a proposal graduates into
-- one epic per primary target project); `epics.proposal_id` denormalizes the
-- reverse mapping, which is unique per epic. Written at graduation link time,
-- cleared on unlink; proposal deletion clears it via ON DELETE SET NULL.

ALTER TABLE epics ADD COLUMN proposal_id VARCHAR(36) NULL REFERENCES proposals(id) ON DELETE SET NULL;

CREATE INDEX epics_proposal_id ON epics(proposal_id);

UPDATE epics
   SET proposal_id = pe.proposal_id
  FROM proposal_epics pe
 WHERE pe.epic_id = epics.id;
