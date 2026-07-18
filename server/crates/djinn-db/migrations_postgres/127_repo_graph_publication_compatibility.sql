-- Carry publication compatibility validation forward without changing the
-- already-applied graph-generation expand migration.
CREATE EXTENSION IF NOT EXISTS pgcrypto;

-- Exact legacy INSERT .. ON CONFLICT updates inherit the existing row's
-- generation_id in the UPDATE trigger input. Rotate that identity, while an
-- explicit identity on a new unmarked row remains reserved-writer-only.
CREATE OR REPLACE FUNCTION repo_graph_cache_publish_before()
RETURNS TRIGGER LANGUAGE plpgsql AS $$
DECLARE
    v_reserved_generation UUID;
    v_marked BOOLEAN;
BEGIN
    IF TG_OP = 'INSERT' THEN
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
    ELSIF TG_OP = 'UPDATE' THEN
        NEW.generation_id := gen_random_uuid();
    ELSIF NEW.generation_id IS NOT NULL THEN
        RAISE EXCEPTION 'repo graph generation marker does not match publication generation';
    ELSE
        NEW.generation_id := gen_random_uuid();
    END IF;

    NEW.built_at := repo_graph_next_built_at(NEW.project_id);
    RETURN NEW;
END;
$$;

-- A marked publication is valid only when transport_sha256 is the SHA-256 of
-- the complete byte stream formed by concatenating chunks in chunk_index order.
CREATE OR REPLACE FUNCTION repo_graph_validate_and_advance_current()
RETURNS TRIGGER LANGUAGE plpgsql AS $$
DECLARE
    v_required BOOLEAN;
    v_artifact repo_graph_galaxy_artifact%ROWTYPE;
    v_chunks INTEGER;
    v_bytes BIGINT;
    v_bad INTEGER;
    v_transport_sha256 VARCHAR;
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
                OR jsonb_typeof(v_artifact.chunk_hashes -> c.chunk_index) IS DISTINCT FROM 'string'
                OR c.sha256 IS DISTINCT FROM (v_artifact.chunk_hashes ->> c.chunk_index));
        SELECT encode(
                   digest(COALESCE(string_agg(c.bytes, ''::bytea ORDER BY c.chunk_index), ''::bytea), 'sha256'),
                   'hex'
               )
          INTO v_transport_sha256
          FROM repo_graph_galaxy_chunk c
         WHERE c.generation_id = NEW.generation_id
           AND c.artifact_id = v_artifact.artifact_id;
        IF v_chunks <> v_artifact.chunk_count
           OR v_bytes <> v_artifact.byte_count
           OR v_bad <> 0
           OR v_transport_sha256 IS DISTINCT FROM v_artifact.transport_sha256 THEN
            RAISE EXCEPTION 'repo graph galaxy artifact chunks are incomplete or invalid';
        END IF;
    END IF;

    INSERT INTO repo_graph_current(project_id, generation_id)
    VALUES (NEW.project_id, NEW.generation_id)
    ON CONFLICT (project_id) DO UPDATE SET generation_id = EXCLUDED.generation_id;
    RETURN NULL;
END;
$$;
