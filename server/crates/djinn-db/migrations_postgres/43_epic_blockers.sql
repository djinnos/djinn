-- Epic-level dependencies (mirror of the task `blockers` table).
--
-- A proposal that graduates into several epics across one or more repos often
-- needs ordering: e.g. a schema epic in repo-A must land before a consumer
-- epic in repo-B. `epic_blockers` records "epic_id is blocked by
-- blocking_epic_id"; the coordinator suppresses an epic's wave-1 planning
-- auto-dispatch while any blocking epic is still open, then re-fires when the
-- last blocker closes (EpicRepository::emit_unblocked_epics → epic.updated).
--
-- ON DELETE CASCADE on both sides so edges disappear with either epic.
CREATE TABLE IF NOT EXISTS epic_blockers (
    epic_id          VARCHAR(36) NOT NULL,
    blocking_epic_id VARCHAR(36) NOT NULL,
    PRIMARY KEY (epic_id, blocking_epic_id),
    CONSTRAINT fk_epic_blockers_epic     FOREIGN KEY (epic_id)          REFERENCES epics(id) ON DELETE CASCADE,
    CONSTRAINT fk_epic_blockers_blocking FOREIGN KEY (blocking_epic_id) REFERENCES epics(id) ON DELETE CASCADE
);

CREATE INDEX epic_blockers_blocking_epic_id ON epic_blockers(blocking_epic_id);
