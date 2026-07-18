-- Additive graph-publication expand migration.  The old repo_graph_cache
-- (project_id, commit_sha) upsert surface intentionally remains authoritative
-- for compatibility; triggers below turn each successful publication into an
-- immutable generation.

ALTER TABLE repo_graph_cache
    ADD COLUMN IF NOT EXISTS generation_id UUID;

CREATE TABLE repo_graph_publish_clock (
    project_id    VARCHAR(36) NOT NULL PRIMARY KEY,
    last_built_at TIMESTAMPTZ NOT NULL,
    CONSTRAINT fk_repo_graph_publish_clock_project
        FOREIGN KEY (project_id) REFERENCES projects(id) ON DELETE CASCADE
);

CREATE TABLE repo_graph_generation (
    generation_id UUID NOT NULL PRIMARY KEY,
    project_id    VARCHAR(36) NOT NULL,
    commit_sha    VARCHAR(64) NOT NULL,
    graph_blob    BYTEA NOT NULL,
    built_at      VARCHAR(64) NOT NULL,
    publish_seq   BIGINT GENERATED ALWAYS AS IDENTITY UNIQUE,
    artifact_required BOOLEAN NOT NULL DEFAULT FALSE,
    CONSTRAINT fk_repo_graph_generation_project
        FOREIGN KEY (project_id) REFERENCES projects(id) ON DELETE CASCADE,
    CONSTRAINT uq_repo_graph_generation_project_generation
        UNIQUE (project_id, generation_id)
);
CREATE INDEX repo_graph_generation_project_publish_seq
    ON repo_graph_generation (project_id, publish_seq DESC);
CREATE INDEX repo_graph_generation_project_commit_publish_seq
    ON repo_graph_generation (project_id, commit_sha, publish_seq DESC);

CREATE TABLE repo_graph_current (
    project_id    VARCHAR(36) NOT NULL PRIMARY KEY,
    generation_id UUID NOT NULL,
    CONSTRAINT fk_repo_graph_current_generation
        FOREIGN KEY (project_id, generation_id)
        REFERENCES repo_graph_generation(project_id, generation_id)
        ON DELETE CASCADE
);

CREATE TABLE repo_graph_galaxy_artifact (
    artifact_id         UUID NOT NULL DEFAULT gen_random_uuid(),
    generation_id       UUID NOT NULL,
    graph_content_hash  VARCHAR(128) NOT NULL,
    transport_sha256    VARCHAR(128) NOT NULL,
    chunk_count         INTEGER NOT NULL,
    byte_count          BIGINT NOT NULL,
    -- The ordered SHA-256 values are part of the manifest.  The deferred
    -- validator compares every chunk to this immutable manifest.
    chunk_hashes        JSONB NOT NULL DEFAULT '[]'::jsonb,
    PRIMARY KEY (artifact_id),
    CONSTRAINT uq_repo_graph_galaxy_artifact_generation UNIQUE (generation_id),
    CONSTRAINT uq_repo_graph_galaxy_artifact_identity UNIQUE (generation_id, artifact_id),
    CONSTRAINT fk_repo_graph_galaxy_artifact_generation
        FOREIGN KEY (generation_id) REFERENCES repo_graph_generation(generation_id)
        ON DELETE CASCADE,
    CONSTRAINT repo_graph_galaxy_artifact_hashes_distinct
        CHECK (graph_content_hash <> transport_sha256),
    CONSTRAINT repo_graph_galaxy_artifact_counts_nonnegative
        CHECK (chunk_count >= 0 AND byte_count >= 0),
    CONSTRAINT repo_graph_galaxy_artifact_chunk_hashes_array
        CHECK (jsonb_typeof(chunk_hashes) = 'array'
               AND jsonb_array_length(chunk_hashes) = chunk_count)
);

CREATE TABLE repo_graph_galaxy_chunk (
    generation_id UUID NOT NULL,
    artifact_id   UUID NOT NULL,
    chunk_index   INTEGER NOT NULL,
    byte_count    INTEGER NOT NULL,
    sha256        VARCHAR(128) NOT NULL,
    bytes         BYTEA NOT NULL,
    PRIMARY KEY (generation_id, artifact_id, chunk_index),
    CONSTRAINT fk_repo_graph_galaxy_chunk_artifact
        FOREIGN KEY (generation_id, artifact_id)
        REFERENCES repo_graph_galaxy_artifact(generation_id, artifact_id)
        ON DELETE CASCADE,
    CONSTRAINT repo_graph_galaxy_chunk_index_nonnegative CHECK (chunk_index >= 0),
    CONSTRAINT repo_graph_galaxy_chunk_size_nonnegative CHECK (byte_count >= 0),
    CONSTRAINT repo_graph_galaxy_chunk_size_matches_bytes
        CHECK (byte_count = octet_length(bytes)),
    CONSTRAINT repo_graph_galaxy_chunk_max_bytes CHECK (octet_length(bytes) <= 262144)
);
CREATE INDEX repo_graph_galaxy_chunk_artifact_order
    ON repo_graph_galaxy_chunk (artifact_id, chunk_index);

-- pg_locks cannot distinguish session advisory locks from transaction advisory
-- locks. This token is inserted only after the xact lock is acquired, is bound
-- to the xid/backend, and is removed by a deferred trigger at commit.
CREATE TABLE repo_graph_publish_lock_token (
    project_id          VARCHAR(36) NOT NULL,
    transaction_id      BIGINT NOT NULL,
    backend_pid         INTEGER NOT NULL,
    reserved_generation UUID,
    PRIMARY KEY (project_id, transaction_id, backend_pid),
    CONSTRAINT fk_repo_graph_publish_lock_token_project
        FOREIGN KEY (project_id) REFERENCES projects(id) ON DELETE CASCADE
);

CREATE OR REPLACE FUNCTION repo_graph_release_publish_lock_token()
RETURNS TRIGGER LANGUAGE plpgsql AS $$
BEGIN
    DELETE FROM repo_graph_publish_lock_token
     WHERE project_id = NEW.project_id
       AND transaction_id = NEW.transaction_id
       AND backend_pid = NEW.backend_pid;
    RETURN NULL;
END;
$$;

CREATE CONSTRAINT TRIGGER repo_graph_release_publish_lock_token
AFTER INSERT ON repo_graph_publish_lock_token
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW EXECUTE FUNCTION repo_graph_release_publish_lock_token();

CREATE OR REPLACE FUNCTION repo_graph_acquire_publish_lock(
    p_project_id VARCHAR,
    p_reserved_generation UUID DEFAULT NULL
) RETURNS VOID LANGUAGE plpgsql AS $$
DECLARE
    v_transaction_id BIGINT := txid_current();
    v_existing UUID;
BEGIN
    PERFORM pg_advisory_xact_lock(hashtextextended(p_project_id, 0));
    SELECT reserved_generation INTO v_existing
      FROM repo_graph_publish_lock_token
     WHERE project_id = p_project_id
       AND transaction_id = v_transaction_id
       AND backend_pid = pg_backend_pid()
     FOR UPDATE;
    IF FOUND THEN
        IF p_reserved_generation IS NOT NULL
           AND v_existing IS DISTINCT FROM p_reserved_generation THEN
            RAISE EXCEPTION 'repo graph generation marker does not match publication generation';
        END IF;
    ELSE
        INSERT INTO repo_graph_publish_lock_token
            (project_id, transaction_id, backend_pid, reserved_generation)
        VALUES (p_project_id, v_transaction_id, pg_backend_pid(), p_reserved_generation);
    END IF;
END;
$$;

-- A reservation records its UUID in the transaction-owned token instead of a
-- caller-settable custom GUC.
CREATE OR REPLACE FUNCTION repo_graph_reserve_generation(
    p_project_id VARCHAR,
    p_generation_id UUID
) RETURNS VOID LANGUAGE plpgsql AS $$
BEGIN
    IF (get_byte(uuid_send(p_generation_id), 6) >> 4) <> 7 THEN
        RAISE EXCEPTION 'repo graph generation marker must be UUIDv7';
    END IF;
    PERFORM repo_graph_acquire_publish_lock(p_project_id, p_generation_id);
    IF EXISTS (SELECT 1 FROM repo_graph_cache WHERE generation_id = p_generation_id)
       OR EXISTS (SELECT 1 FROM repo_graph_generation WHERE generation_id = p_generation_id) THEN
        RAISE EXCEPTION 'repo graph generation marker already exists';
    END IF;
END;
$$;

CREATE OR REPLACE FUNCTION repo_graph_generation_reject_update()
RETURNS TRIGGER LANGUAGE plpgsql AS $$
BEGIN
    RAISE EXCEPTION 'repo graph generations are immutable';
END;
$$;

-- Do not use pg_locks or a custom GUC as lock evidence: the former conflates
-- session and transaction locks while the latter is caller-settable.
CREATE OR REPLACE FUNCTION repo_graph_assert_publish_lock(p_project_id VARCHAR)
RETURNS VOID LANGUAGE plpgsql AS $$
BEGIN
    IF NOT EXISTS (
        SELECT 1
          FROM repo_graph_publish_lock_token
         WHERE project_id = p_project_id
           AND transaction_id = txid_current()
           AND backend_pid = pg_backend_pid()
    ) THEN
        RAISE EXCEPTION 'repo graph cache UPDATE requires its project publication lock';
    END IF;
END;
$$;

CREATE OR REPLACE FUNCTION repo_graph_next_built_at(p_project_id VARCHAR)
RETURNS VARCHAR LANGUAGE plpgsql AS $$
DECLARE
    v_next TIMESTAMPTZ;
BEGIN
    -- The caller either acquired this lock on INSERT before ON CONFLICT picks
    -- a branch, or proved ownership on UPDATE.  The clock row lock makes this
    -- rollback-safe and strictly increasing per project.
    INSERT INTO repo_graph_publish_clock(project_id, last_built_at)
    VALUES (p_project_id, clock_timestamp())
    ON CONFLICT (project_id) DO UPDATE
       SET last_built_at = GREATEST(
           repo_graph_publish_clock.last_built_at + interval '1 microsecond',
           clock_timestamp())
    RETURNING last_built_at INTO v_next;
    RETURN to_char(v_next AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.US"Z"');
END;
$$;

CREATE OR REPLACE FUNCTION repo_graph_cache_publish_before()
RETURNS TRIGGER LANGUAGE plpgsql AS $$
DECLARE
    v_reserved_generation UUID;
    v_marked BOOLEAN;
BEGIN
    IF TG_OP = 'INSERT' THEN
        -- This is intentionally before conflict resolution. An INSERT which
        -- lands in DO UPDATE has already acquired the transaction lock.
        PERFORM repo_graph_acquire_publish_lock(NEW.project_id);
    ELSE
        PERFORM repo_graph_assert_publish_lock(NEW.project_id);
    END IF;

    SELECT reserved_generation INTO v_reserved_generation
      FROM repo_graph_publish_lock_token
     WHERE project_id = NEW.project_id
       AND transaction_id = txid_current()
       AND backend_pid = pg_backend_pid();
    IF v_reserved_generation IS NOT NULL
       AND NEW.generation_id IS DISTINCT FROM v_reserved_generation THEN
        RAISE EXCEPTION 'repo graph generation marker does not match publication generation';
    END IF;
    v_marked := v_reserved_generation IS NOT NULL
                AND NEW.generation_id = v_reserved_generation;

    IF v_marked THEN
        IF (get_byte(uuid_send(NEW.generation_id), 6) >> 4) <> 7 THEN
            RAISE EXCEPTION 'repo graph generation marker must be UUIDv7';
        END IF;
        IF EXISTS (SELECT 1 FROM repo_graph_cache c WHERE c.generation_id = NEW.generation_id)
           OR EXISTS (SELECT 1 FROM repo_graph_generation g WHERE g.generation_id = NEW.generation_id) THEN
            RAISE EXCEPTION 'repo graph generation marker already exists';
        END IF;
    ELSE
        -- Never allow a default, stale row, or unmarked explicit value to
        -- turn an exact legacy upsert into a mutable generation.
        NEW.generation_id := gen_random_uuid();
    END IF;

    NEW.built_at := repo_graph_next_built_at(NEW.project_id);
    RETURN NEW;
END;
$$;

CREATE OR REPLACE FUNCTION repo_graph_cache_publish_after()
RETURNS TRIGGER LANGUAGE plpgsql AS $$
DECLARE
    v_marked BOOLEAN;
BEGIN
    SELECT reserved_generation = NEW.generation_id INTO v_marked
      FROM repo_graph_publish_lock_token
     WHERE project_id = NEW.project_id
       AND transaction_id = txid_current()
       AND backend_pid = pg_backend_pid();
    v_marked := COALESCE(v_marked, FALSE);
    INSERT INTO repo_graph_generation
        (generation_id, project_id, commit_sha, graph_blob, built_at, artifact_required)
    VALUES (NEW.generation_id, NEW.project_id, NEW.commit_sha, NEW.graph_blob, NEW.built_at, v_marked);
    RETURN NULL;
END;
$$;

CREATE OR REPLACE FUNCTION repo_graph_validate_and_advance_current()
RETURNS TRIGGER LANGUAGE plpgsql AS $$
DECLARE
    v_required BOOLEAN;
    v_artifact repo_graph_galaxy_artifact%ROWTYPE;
    v_chunks INTEGER;
    v_bytes BIGINT;
    v_bad INTEGER;
BEGIN
    SELECT artifact_required INTO v_required
      FROM repo_graph_generation WHERE generation_id = NEW.generation_id;
    IF NOT FOUND THEN
        RAISE EXCEPTION 'repo graph generation missing for compatibility publication';
    END IF;

    IF v_required THEN
        SELECT * INTO v_artifact FROM repo_graph_galaxy_artifact
         WHERE generation_id = NEW.generation_id;
        IF NOT FOUND THEN
            RAISE EXCEPTION 'marked repo graph publication requires a galaxy artifact';
        END IF;
        SELECT count(*), COALESCE(sum(byte_count), 0)
          INTO v_chunks, v_bytes
          FROM repo_graph_galaxy_chunk
         WHERE generation_id = NEW.generation_id
           AND artifact_id = v_artifact.artifact_id;
        SELECT count(*) INTO v_bad
          FROM repo_graph_galaxy_chunk c
         WHERE c.generation_id = NEW.generation_id
           AND c.artifact_id = v_artifact.artifact_id
           AND (c.chunk_index < 0
                OR c.chunk_index >= v_artifact.chunk_count
                -- JSON null and all non-string manifest entries are invalid,
                -- even if their text rendering happens to match a chunk hash.
                OR jsonb_typeof(v_artifact.chunk_hashes -> c.chunk_index) IS DISTINCT FROM 'string'
                OR c.sha256 IS DISTINCT FROM (v_artifact.chunk_hashes ->> c.chunk_index));
        IF v_chunks <> v_artifact.chunk_count
           OR v_bytes <> v_artifact.byte_count
           OR v_bad <> 0 THEN
            RAISE EXCEPTION 'repo graph galaxy artifact chunks are incomplete or invalid';
        END IF;
    END IF;

    INSERT INTO repo_graph_current(project_id, generation_id)
    VALUES (NEW.project_id, NEW.generation_id)
    ON CONFLICT (project_id) DO UPDATE SET generation_id = EXCLUDED.generation_id;
    RETURN NULL;
END;
$$;

-- Backfill predates the triggers so no historical row is treated as a new
-- publication.  The order is deterministic even for empty/equal/skewed old
-- text timestamps, and only this normalized order is used by legacy readers.
DO $$
DECLARE
    p RECORD;
    r RECORD;
    v_generation UUID;
    v_built_at VARCHAR;
BEGIN
    FOR p IN SELECT DISTINCT project_id FROM repo_graph_cache ORDER BY project_id LOOP
        PERFORM pg_advisory_xact_lock(hashtextextended(p.project_id, 0));
        FOR r IN SELECT ctid, commit_sha, graph_blob
                   FROM repo_graph_cache
                  WHERE project_id = p.project_id
                  ORDER BY built_at, commit_sha, encode(graph_blob, 'hex'), ctid LOOP
            v_generation := gen_random_uuid();
            v_built_at := repo_graph_next_built_at(p.project_id);
            UPDATE repo_graph_cache
               SET generation_id = v_generation, built_at = v_built_at
             WHERE ctid = r.ctid;
            INSERT INTO repo_graph_generation
                (generation_id, project_id, commit_sha, graph_blob, built_at)
            VALUES (v_generation, p.project_id, r.commit_sha, r.graph_blob, v_built_at);
        END LOOP;
        INSERT INTO repo_graph_current(project_id, generation_id)
        SELECT project_id, generation_id
          FROM repo_graph_cache
         WHERE project_id = p.project_id
         ORDER BY built_at DESC, commit_sha DESC
         LIMIT 1
        ON CONFLICT (project_id) DO UPDATE SET generation_id = EXCLUDED.generation_id;
    END LOOP;
END;
$$;

ALTER TABLE repo_graph_cache
    ALTER COLUMN generation_id SET NOT NULL;
ALTER TABLE repo_graph_cache
    ADD CONSTRAINT uq_repo_graph_cache_generation_id UNIQUE (generation_id),
    ADD CONSTRAINT fk_repo_graph_cache_generation
        FOREIGN KEY (project_id, generation_id)
        REFERENCES repo_graph_generation(project_id, generation_id)
        ON DELETE CASCADE
        -- The immutable mirror is inserted by the AFTER publication trigger,
        -- so this compatibility-to-generation edge must check at commit.
        DEFERRABLE INITIALLY DEFERRED;
CREATE INDEX repo_graph_cache_project_built_at
    ON repo_graph_cache (project_id, built_at DESC);

CREATE TRIGGER repo_graph_cache_publish_before
BEFORE INSERT OR UPDATE ON repo_graph_cache
FOR EACH ROW EXECUTE FUNCTION repo_graph_cache_publish_before();
CREATE TRIGGER repo_graph_cache_publish_after
AFTER INSERT OR UPDATE ON repo_graph_cache
FOR EACH ROW EXECUTE FUNCTION repo_graph_cache_publish_after();
CREATE TRIGGER repo_graph_generation_immutable
BEFORE UPDATE ON repo_graph_generation
FOR EACH ROW EXECUTE FUNCTION repo_graph_generation_reject_update();
CREATE CONSTRAINT TRIGGER repo_graph_cache_validate_and_advance_current
AFTER INSERT OR UPDATE ON repo_graph_cache
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW EXECUTE FUNCTION repo_graph_validate_and_advance_current();
