-- Persist the authoritative current attempt instead of inferring it from query order.
ALTER TABLE readiness_composition_areas
    ADD COLUMN current_attempt_id VARCHAR(36) NULL;

ALTER TABLE readiness_composition_areas
    ADD CONSTRAINT readiness_areas_current_attempt_fk
    FOREIGN KEY (current_attempt_id, run_id, id)
    REFERENCES readiness_area_attempts(id, run_id, area_id)
    DEFERRABLE INITIALLY DEFERRED;

UPDATE readiness_composition_areas area
SET current_attempt_id = attempt.id
FROM readiness_area_attempts attempt
WHERE attempt.area_id = area.id
  AND attempt.run_id = area.run_id
  AND attempt.attempt_number = (
      SELECT max(candidate.attempt_number)
      FROM readiness_area_attempts candidate
      WHERE candidate.area_id = area.id
  );

CREATE INDEX readiness_areas_current_attempt_idx
    ON readiness_composition_areas(run_id, area_key, current_attempt_id);
