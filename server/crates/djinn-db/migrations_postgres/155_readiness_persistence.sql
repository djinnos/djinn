-- Migration 155: durable Agent Readiness S1 persistence.
-- Additive only.  Lifecycle writes are guarded here, rather than relying on
-- callers to remember correlation and immutability rules.

CREATE TABLE readiness_runs (
    id VARCHAR(36) PRIMARY KEY,
    project_id VARCHAR(36) NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    idempotency_key VARCHAR(255) NOT NULL,
    status VARCHAR(32) NOT NULL DEFAULT 'identifying',
    repository_snapshot VARCHAR(255) NOT NULL,
    skill_name VARCHAR(255) NOT NULL,
    skill_version VARCHAR(64) NOT NULL,
    expected_area_count INTEGER NULL,
    created_at VARCHAR(64) NOT NULL DEFAULT to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
    completed_at VARCHAR(64) NULL,
    CONSTRAINT readiness_runs_status_check CHECK (status IN ('identifying','analyzing','aggregating','completed','completed_with_errors','failed')),
    CONSTRAINT readiness_runs_expected_area_count_check CHECK (expected_area_count IS NULL OR expected_area_count >= 0),
    CONSTRAINT readiness_runs_terminal_check CHECK ((status IN ('completed','completed_with_errors','failed')) = (completed_at IS NOT NULL)),
    CONSTRAINT readiness_runs_project_idempotency_key UNIQUE (project_id, idempotency_key)
);
CREATE UNIQUE INDEX readiness_runs_one_active_project_idx ON readiness_runs(project_id) WHERE status IN ('identifying','analyzing','aggregating');
CREATE INDEX readiness_runs_project_latest_idx ON readiness_runs(project_id, created_at DESC);

CREATE TABLE readiness_composition_areas (
    id VARCHAR(36) PRIMARY KEY,
    run_id VARCHAR(36) NOT NULL REFERENCES readiness_runs(id) ON DELETE CASCADE,
    area_key VARCHAR(128) NOT NULL,
    composition JSONB NOT NULL DEFAULT '{}'::jsonb,
    path_scopes JSONB NOT NULL DEFAULT '[]'::jsonb,
    frozen_at VARCHAR(64) NOT NULL DEFAULT to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
    status VARCHAR(32) NOT NULL DEFAULT 'pending',
    CONSTRAINT readiness_areas_status_check CHECK (status IN ('pending','running','succeeded','failed','timed_out','invalid')),
    CONSTRAINT readiness_areas_run_key UNIQUE (run_id, area_key),
    CONSTRAINT readiness_areas_id_run UNIQUE (id, run_id)
);
CREATE INDEX readiness_areas_run_detail_idx ON readiness_composition_areas(run_id, area_key);

CREATE TABLE readiness_area_attempts (
    id VARCHAR(36) PRIMARY KEY,
    run_id VARCHAR(36) NOT NULL REFERENCES readiness_runs(id) ON DELETE CASCADE,
    area_id VARCHAR(36) NOT NULL,
    attempt_number INTEGER NOT NULL,
    correlation_key VARCHAR(255) NOT NULL,
    status VARCHAR(32) NOT NULL DEFAULT 'running',
    payload_digest VARCHAR(128) NULL,
    created_at VARCHAR(64) NOT NULL DEFAULT to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
    terminal_at VARCHAR(64) NULL,
    CONSTRAINT readiness_attempts_area_run_fk FOREIGN KEY (area_id, run_id) REFERENCES readiness_composition_areas(id, run_id) ON DELETE CASCADE,
    CONSTRAINT readiness_attempts_number_check CHECK (attempt_number > 0),
    CONSTRAINT readiness_attempts_status_check CHECK (status IN ('running','succeeded','failed','timed_out','invalid','superseded')),
    CONSTRAINT readiness_attempts_terminal_check CHECK ((status IN ('succeeded','failed','timed_out','invalid','superseded')) = (terminal_at IS NOT NULL)),
    CONSTRAINT readiness_attempts_area_number UNIQUE (area_id, attempt_number),
    CONSTRAINT readiness_attempts_correlation_key UNIQUE (correlation_key),
    CONSTRAINT readiness_attempts_id_run_area UNIQUE (id, run_id, area_id)
);
CREATE INDEX readiness_attempts_area_idx ON readiness_area_attempts(area_id, attempt_number DESC);

CREATE TABLE readiness_guardrail_findings (
    id VARCHAR(36) PRIMARY KEY,
    run_id VARCHAR(36) NOT NULL,
    area_id VARCHAR(36) NOT NULL,
    attempt_id VARCHAR(36) NOT NULL,
    guardrail_key VARCHAR(255) NOT NULL,
    severity VARCHAR(32) NOT NULL,
    accepted BOOLEAN NOT NULL DEFAULT FALSE,
    evidence JSONB NOT NULL DEFAULT '{}'::jsonb,
    created_at VARCHAR(64) NOT NULL DEFAULT to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
    CONSTRAINT readiness_findings_attempt_correlation_fk FOREIGN KEY (attempt_id, run_id, area_id) REFERENCES readiness_area_attempts(id, run_id, area_id) ON DELETE CASCADE,
    CONSTRAINT readiness_findings_severity_check CHECK (severity IN ('info','low','medium','high','critical')),
    CONSTRAINT readiness_findings_attempt_guardrail UNIQUE (attempt_id, guardrail_key)
);
CREATE INDEX readiness_findings_attempt_idx ON readiness_guardrail_findings(attempt_id);

CREATE TABLE readiness_remediation_suggestions (
    id VARCHAR(36) PRIMARY KEY,
    run_id VARCHAR(36) NOT NULL REFERENCES readiness_runs(id) ON DELETE CASCADE,
    dedupe_key VARCHAR(255) NOT NULL,
    suggestion JSONB NOT NULL,
    created_at VARCHAR(64) NOT NULL DEFAULT to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
    CONSTRAINT readiness_suggestions_run_dedupe UNIQUE (run_id, dedupe_key)
);
CREATE INDEX readiness_suggestions_run_idx ON readiness_remediation_suggestions(run_id);

CREATE TABLE readiness_run_events (
    id VARCHAR(36) PRIMARY KEY,
    run_id VARCHAR(36) NOT NULL REFERENCES readiness_runs(id) ON DELETE CASCADE,
    event_kind VARCHAR(64) NOT NULL,
    payload JSONB NOT NULL DEFAULT '{}'::jsonb,
    created_at VARCHAR(64) NOT NULL DEFAULT to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')
);
CREATE INDEX readiness_events_run_detail_idx ON readiness_run_events(run_id, created_at, id);

CREATE OR REPLACE FUNCTION readiness_reject_completed_child_write() RETURNS trigger AS $$
BEGIN
  IF EXISTS (SELECT 1 FROM readiness_runs WHERE id = COALESCE(NEW.run_id, OLD.run_id) AND status IN ('completed','completed_with_errors','failed')) THEN
    RAISE EXCEPTION 'readiness run is terminal';
  END IF;
  RETURN COALESCE(NEW, OLD);
END; $$ LANGUAGE plpgsql;

CREATE OR REPLACE FUNCTION readiness_area_freeze_guard() RETURNS trigger AS $$
BEGIN
  IF OLD.area_key IS DISTINCT FROM NEW.area_key OR OLD.composition IS DISTINCT FROM NEW.composition OR OLD.path_scopes IS DISTINCT FROM NEW.path_scopes OR OLD.frozen_at IS DISTINCT FROM NEW.frozen_at OR OLD.run_id IS DISTINCT FROM NEW.run_id THEN
    RAISE EXCEPTION 'readiness composition area is frozen';
  END IF;
  RETURN NEW;
END; $$ LANGUAGE plpgsql;

CREATE OR REPLACE FUNCTION readiness_finding_immutable_guard() RETURNS trigger AS $$
BEGIN
  IF TG_OP = 'DELETE' OR OLD.accepted THEN RAISE EXCEPTION 'accepted readiness finding is immutable'; END IF;
  RETURN NEW;
END; $$ LANGUAGE plpgsql;

CREATE OR REPLACE FUNCTION readiness_event_append_only_guard() RETURNS trigger AS $$
BEGIN
  RAISE EXCEPTION 'readiness run events are append-only';
END; $$ LANGUAGE plpgsql;

CREATE OR REPLACE FUNCTION readiness_terminal_run_guard() RETURNS trigger AS $$
BEGIN
  IF OLD.status IN ('completed','completed_with_errors','failed') THEN RAISE EXCEPTION 'completed readiness run is immutable'; END IF;
  IF NEW.status IN ('completed','completed_with_errors') AND (NEW.expected_area_count IS NULL OR NEW.expected_area_count <> (SELECT count(*) FROM readiness_composition_areas WHERE run_id=NEW.id) OR EXISTS (SELECT 1 FROM readiness_composition_areas WHERE run_id=NEW.id AND status NOT IN ('succeeded','failed','timed_out','invalid'))) THEN
    RAISE EXCEPTION 'readiness run cannot complete before expected areas are terminal';
  END IF;
  RETURN NEW;
END; $$ LANGUAGE plpgsql;

CREATE TRIGGER readiness_runs_terminal_guard BEFORE UPDATE ON readiness_runs FOR EACH ROW EXECUTE FUNCTION readiness_terminal_run_guard();
CREATE TRIGGER readiness_areas_freeze_guard BEFORE UPDATE ON readiness_composition_areas FOR EACH ROW EXECUTE FUNCTION readiness_area_freeze_guard();
CREATE TRIGGER readiness_findings_immutable_update BEFORE UPDATE ON readiness_guardrail_findings FOR EACH ROW EXECUTE FUNCTION readiness_finding_immutable_guard();
CREATE TRIGGER readiness_findings_immutable_delete BEFORE DELETE ON readiness_guardrail_findings FOR EACH ROW EXECUTE FUNCTION readiness_finding_immutable_guard();
CREATE TRIGGER readiness_events_append_only_update BEFORE UPDATE OR DELETE ON readiness_run_events FOR EACH ROW EXECUTE FUNCTION readiness_event_append_only_guard();
CREATE TRIGGER readiness_areas_terminal_guard BEFORE INSERT OR UPDATE OR DELETE ON readiness_composition_areas FOR EACH ROW EXECUTE FUNCTION readiness_reject_completed_child_write();
CREATE TRIGGER readiness_attempts_terminal_guard BEFORE INSERT OR UPDATE OR DELETE ON readiness_area_attempts FOR EACH ROW EXECUTE FUNCTION readiness_reject_completed_child_write();
CREATE TRIGGER readiness_findings_terminal_guard BEFORE INSERT OR UPDATE OR DELETE ON readiness_guardrail_findings FOR EACH ROW EXECUTE FUNCTION readiness_reject_completed_child_write();
CREATE TRIGGER readiness_suggestions_terminal_guard BEFORE INSERT OR UPDATE OR DELETE ON readiness_remediation_suggestions FOR EACH ROW EXECUTE FUNCTION readiness_reject_completed_child_write();
CREATE TRIGGER readiness_events_terminal_guard BEFORE INSERT OR UPDATE OR DELETE ON readiness_run_events FOR EACH ROW EXECUTE FUNCTION readiness_reject_completed_child_write();
