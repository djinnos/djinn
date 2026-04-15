-- ADR-055 Dolt/MySQL schema snapshot for djinn-db.
--
-- This snapshot is the full-port of the SQLite refinery migration chain into
-- MySQL 8.0 / Dolt dialect. It omits SQLite-only constructs (FTS5 shadow
-- tables + triggers, sqlite-vec virtual tables) in favour of native MySQL
-- equivalents (FULLTEXT index, Qdrant for vector search).
--
-- Ground truth: crates/djinn-db/migrations/*.sql (the rebuilt shapes used at
-- runtime by the Rust repository layer). The legacy top-level schema.sql
-- file is partially stale and is NOT the source used here.
--
-- Every table uses CREATE TABLE IF NOT EXISTS so that partial-apply recovery
-- is possible when the schema version marker is bumped.

-- ── settings & projects ──────────────────────────────────────────────────────
CREATE TABLE IF NOT EXISTS settings (
    `key`      VARCHAR(191) NOT NULL PRIMARY KEY,
    `value`    LONGTEXT NOT NULL,
    updated_at VARCHAR(64) NOT NULL DEFAULT ""
);

CREATE TABLE IF NOT EXISTS projects (
    id                  VARCHAR(36)  NOT NULL PRIMARY KEY,
    name                VARCHAR(255) NOT NULL,
    path                VARCHAR(512) NOT NULL,
    created_at          VARCHAR(64)  NOT NULL DEFAULT (DATE_FORMAT(NOW(3), '%Y-%m-%dT%H:%i:%s.%fZ')),
    target_branch       VARCHAR(255) NOT NULL DEFAULT 'main',
    auto_merge          BOOLEAN      NOT NULL DEFAULT TRUE,
    sync_enabled        BOOLEAN      NOT NULL DEFAULT FALSE,
    sync_remote         VARCHAR(512) NULL,
    verification_rules  LONGTEXT     NOT NULL,
    UNIQUE KEY uq_projects_name (name),
    UNIQUE KEY uq_projects_path (path)
);

-- ── epics ────────────────────────────────────────────────────────────────────
CREATE TABLE IF NOT EXISTS epics (
    id                  VARCHAR(36)  NOT NULL PRIMARY KEY,
    project_id          VARCHAR(36)  NOT NULL,
    short_id            VARCHAR(32)  NOT NULL,
    title               VARCHAR(512) NOT NULL,
    description         LONGTEXT     NOT NULL,
    emoji               VARCHAR(32)  NOT NULL DEFAULT '',
    color               VARCHAR(32)  NOT NULL DEFAULT '',
    `status`            VARCHAR(64)  NOT NULL DEFAULT 'drafting',
    owner               VARCHAR(255) NOT NULL DEFAULT '',
    created_at          VARCHAR(64)  NOT NULL DEFAULT (DATE_FORMAT(NOW(3), '%Y-%m-%dT%H:%i:%s.%fZ')),
    updated_at          VARCHAR(64)  NOT NULL DEFAULT (DATE_FORMAT(NOW(3), '%Y-%m-%dT%H:%i:%s.%fZ')),
    closed_at           VARCHAR(64)  NULL,
    memory_refs         LONGTEXT     NOT NULL,
    auto_breakdown      BOOLEAN      NOT NULL DEFAULT TRUE,
    originating_adr_id  VARCHAR(191) NULL,
    UNIQUE KEY uq_epics_project_short_id (project_id, short_id),
    CONSTRAINT fk_epics_project FOREIGN KEY (project_id) REFERENCES projects(id) ON DELETE CASCADE
);

CREATE INDEX epics_project_id ON epics(project_id);

-- ── tasks ────────────────────────────────────────────────────────────────────
CREATE TABLE IF NOT EXISTS tasks (
    id                                 VARCHAR(36)  NOT NULL PRIMARY KEY,
    project_id                         VARCHAR(36)  NOT NULL,
    short_id                           VARCHAR(32)  NOT NULL,
    epic_id                            VARCHAR(36)  NULL,
    title                              VARCHAR(512) NOT NULL,
    description                        LONGTEXT     NOT NULL,
    design                             LONGTEXT     NOT NULL,
    issue_type                         VARCHAR(64)  NOT NULL DEFAULT 'task',
    `status`                           VARCHAR(64)  NOT NULL DEFAULT 'open',
    priority                           INT          NOT NULL DEFAULT 0,
    owner                              VARCHAR(255) NOT NULL DEFAULT '',
    labels                             LONGTEXT     NOT NULL,
    acceptance_criteria                LONGTEXT     NOT NULL,
    reopen_count                       INT          NOT NULL DEFAULT 0,
    continuation_count                 INT          NOT NULL DEFAULT 0,
    verification_failure_count         INT          NOT NULL DEFAULT 0,
    total_reopen_count                 INT          NOT NULL DEFAULT 0,
    total_verification_failure_count   INT          NOT NULL DEFAULT 0,
    intervention_count                 INT          NOT NULL DEFAULT 0,
    last_intervention_at               VARCHAR(64)  NULL,
    created_at                         VARCHAR(64)  NOT NULL DEFAULT (DATE_FORMAT(NOW(3), '%Y-%m-%dT%H:%i:%s.%fZ')),
    updated_at                         VARCHAR(64)  NOT NULL DEFAULT (DATE_FORMAT(NOW(3), '%Y-%m-%dT%H:%i:%s.%fZ')),
    closed_at                          VARCHAR(64)  NULL,
    close_reason                       LONGTEXT     NULL,
    merge_commit_sha                   VARCHAR(64)  NULL,
    memory_refs                        LONGTEXT     NOT NULL,
    merge_conflict_metadata            LONGTEXT     NULL,
    agent_type                         VARCHAR(64)  NULL,
    pr_url                             VARCHAR(1024) NULL,
    UNIQUE KEY uq_tasks_project_short_id (project_id, short_id),
    CONSTRAINT fk_tasks_project FOREIGN KEY (project_id) REFERENCES projects(id) ON DELETE CASCADE,
    CONSTRAINT fk_tasks_epic    FOREIGN KEY (epic_id)    REFERENCES epics(id)    ON DELETE SET NULL
);

CREATE INDEX tasks_project_id ON tasks(project_id);
CREATE INDEX tasks_epic_id    ON tasks(epic_id);
CREATE INDEX tasks_status     ON tasks(`status`);
CREATE INDEX tasks_priority   ON tasks(priority, created_at);

-- ── blockers ────────────────────────────────────────────────────────────────
CREATE TABLE IF NOT EXISTS blockers (
    task_id          VARCHAR(36) NOT NULL,
    blocking_task_id VARCHAR(36) NOT NULL,
    PRIMARY KEY (task_id, blocking_task_id),
    CONSTRAINT fk_blockers_task     FOREIGN KEY (task_id)          REFERENCES tasks(id) ON DELETE CASCADE,
    CONSTRAINT fk_blockers_blocking FOREIGN KEY (blocking_task_id) REFERENCES tasks(id) ON DELETE CASCADE
);

CREATE INDEX blockers_blocking_task_id ON blockers(blocking_task_id);

-- ── activity_log ────────────────────────────────────────────────────────────
-- Append-only audit trail. task_id has no FK so log entries survive task
-- deletion. `archived` toggles soft-hide without deletion.
CREATE TABLE IF NOT EXISTS activity_log (
    id          VARCHAR(36)  NOT NULL PRIMARY KEY,
    task_id     VARCHAR(36)  NULL,
    actor_id    VARCHAR(255) NOT NULL DEFAULT '',
    actor_role  VARCHAR(64)  NOT NULL DEFAULT '',
    event_type  VARCHAR(128) NOT NULL,
    payload     LONGTEXT     NOT NULL,
    archived    BOOLEAN      NOT NULL DEFAULT FALSE,
    created_at  VARCHAR(64)  NOT NULL DEFAULT (DATE_FORMAT(NOW(3), '%Y-%m-%dT%H:%i:%s.%fZ'))
);

CREATE INDEX activity_log_task_id     ON activity_log(task_id);
CREATE INDEX activity_log_created_at  ON activity_log(created_at);
CREATE INDEX activity_log_archived    ON activity_log(archived);

-- ── notes ───────────────────────────────────────────────────────────────────
CREATE TABLE IF NOT EXISTS notes (
    id            VARCHAR(36)   NOT NULL PRIMARY KEY,
    project_id    VARCHAR(36)   NOT NULL,
    permalink     VARCHAR(255)  NOT NULL,
    title         VARCHAR(512)  NOT NULL,
    file_path     VARCHAR(1024) NOT NULL,
    storage       VARCHAR(32)   NOT NULL DEFAULT 'file',
    note_type     VARCHAR(64)   NOT NULL DEFAULT '',
    folder        VARCHAR(255)  NOT NULL DEFAULT '',
    tags          LONGTEXT      NOT NULL,
    content       LONGTEXT      NOT NULL,
    created_at    VARCHAR(64)   NOT NULL DEFAULT (DATE_FORMAT(NOW(3), '%Y-%m-%dT%H:%i:%s.%fZ')),
    updated_at    VARCHAR(64)   NOT NULL DEFAULT (DATE_FORMAT(NOW(3), '%Y-%m-%dT%H:%i:%s.%fZ')),
    last_accessed VARCHAR(64)   NOT NULL DEFAULT (DATE_FORMAT(NOW(3), '%Y-%m-%dT%H:%i:%s.%fZ')),
    access_count  BIGINT        NOT NULL DEFAULT 0,
    confidence    DOUBLE        NOT NULL DEFAULT 1.0,
    abstract      LONGTEXT      NULL,
    overview      LONGTEXT      NULL,
    scope_paths   LONGTEXT      NOT NULL,
    content_hash  CHAR(64)      NULL,
    UNIQUE KEY uq_notes_project_permalink (project_id, permalink),
    CONSTRAINT fk_notes_project FOREIGN KEY (project_id) REFERENCES projects(id) ON DELETE CASCADE
);

CREATE INDEX notes_project_id            ON notes(project_id);
CREATE INDEX notes_folder                ON notes(folder);
CREATE INDEX notes_type                  ON notes(note_type);
CREATE INDEX notes_updated_at            ON notes(updated_at);
CREATE INDEX notes_project_last_accessed ON notes(project_id, last_accessed);
CREATE INDEX notes_project_content_hash  ON notes(project_id, content_hash);
CREATE INDEX notes_project_folder_title  ON notes(project_id, folder, title);

-- MySQL/Dolt replacement for the SQLite FTS5 shadow table + triggers.
ALTER TABLE notes ADD FULLTEXT KEY notes_ft (title, content, tags);

-- ── note_embeddings + meta (vector bytes; Qdrant holds the ANN index) ───────
CREATE TABLE IF NOT EXISTS note_embeddings (
    note_id        VARCHAR(36) NOT NULL PRIMARY KEY,
    embedding      LONGBLOB    NOT NULL,
    embedding_dim  INT         NOT NULL,
    updated_at     VARCHAR(64) NOT NULL DEFAULT "",
    CONSTRAINT fk_note_embeddings_note FOREIGN KEY (note_id) REFERENCES notes(id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS note_embedding_meta (
    note_id         VARCHAR(36)  NOT NULL PRIMARY KEY,
    content_hash    CHAR(64)     NOT NULL,
    embedded_at     VARCHAR(64)  NOT NULL,
    model_version   VARCHAR(255) NOT NULL,
    embedding_dim   INT          NOT NULL,
    extension_state VARCHAR(64)  NOT NULL DEFAULT 'pending',
    branch          VARCHAR(255) NOT NULL DEFAULT 'main',
    CONSTRAINT fk_note_embedding_meta_note FOREIGN KEY (note_id) REFERENCES notes(id) ON DELETE CASCADE
);

CREATE INDEX idx_note_embedding_meta_model_version ON note_embedding_meta(model_version);
CREATE INDEX idx_note_embedding_meta_embedded_at   ON note_embedding_meta(embedded_at);
CREATE INDEX idx_note_embedding_meta_branch        ON note_embedding_meta(branch);

-- ── note_links ──────────────────────────────────────────────────────────────
CREATE TABLE IF NOT EXISTS note_links (
    id           VARCHAR(36)  NOT NULL PRIMARY KEY,
    source_id    VARCHAR(36)  NOT NULL,
    target_id    VARCHAR(36)  NULL,
    target_raw   VARCHAR(512) NOT NULL,
    display_text VARCHAR(512) NULL,
    UNIQUE KEY uq_note_links_source_target_raw (source_id, target_raw),
    CONSTRAINT fk_note_links_source FOREIGN KEY (source_id) REFERENCES notes(id) ON DELETE CASCADE,
    CONSTRAINT fk_note_links_target FOREIGN KEY (target_id) REFERENCES notes(id) ON DELETE SET NULL
);

CREATE INDEX note_links_source ON note_links(source_id);
CREATE INDEX note_links_target ON note_links(target_id);

-- ── note_associations (ADR-023 Hebbian co-access) ──────────────────────────
CREATE TABLE IF NOT EXISTS note_associations (
    note_a_id       VARCHAR(36) NOT NULL,
    note_b_id       VARCHAR(36) NOT NULL,
    weight          DOUBLE      NOT NULL DEFAULT 0.01,
    co_access_count BIGINT      NOT NULL DEFAULT 1,
    last_co_access  VARCHAR(64) NOT NULL,
    PRIMARY KEY (note_a_id, note_b_id),
    CONSTRAINT fk_note_associations_a FOREIGN KEY (note_a_id) REFERENCES notes(id) ON DELETE CASCADE,
    CONSTRAINT fk_note_associations_b FOREIGN KEY (note_b_id) REFERENCES notes(id) ON DELETE CASCADE,
    CONSTRAINT chk_note_association_order CHECK (note_a_id < note_b_id)
);

CREATE INDEX idx_note_associations_a      ON note_associations(note_a_id);
CREATE INDEX idx_note_associations_b      ON note_associations(note_b_id);
CREATE INDEX idx_note_associations_weight ON note_associations(weight);

-- ── consolidation_run_metrics (provenance table moved below sessions) ─────
CREATE TABLE IF NOT EXISTS consolidation_run_metrics (
    id                          VARCHAR(36) NOT NULL PRIMARY KEY,
    project_id                  VARCHAR(36) NOT NULL,
    note_type                   VARCHAR(64) NOT NULL,
    `status`                    VARCHAR(64) NOT NULL,
    scanned_note_count          INT         NOT NULL,
    candidate_cluster_count     INT         NOT NULL,
    consolidated_cluster_count  INT         NOT NULL,
    consolidated_note_count     INT         NOT NULL,
    source_note_count           INT         NOT NULL,
    started_at                  VARCHAR(64) NOT NULL,
    completed_at                VARCHAR(64) NULL,
    error_message               LONGTEXT    NULL,
    CONSTRAINT fk_consolidation_metrics_project FOREIGN KEY (project_id) REFERENCES projects(id) ON DELETE CASCADE
);

CREATE INDEX idx_consolidation_run_metrics_project_note_type_started_at
    ON consolidation_run_metrics(project_id, note_type, started_at);

-- ── task / epic ⇄ note refs ─────────────────────────────────────────────────
CREATE TABLE IF NOT EXISTS task_memory_refs (
    task_id     VARCHAR(36) NOT NULL,
    note_id     VARCHAR(36) NOT NULL,
    relation    VARCHAR(64) NOT NULL DEFAULT 'context',
    created_at  VARCHAR(64) NOT NULL DEFAULT "",
    PRIMARY KEY (task_id, note_id),
    CONSTRAINT fk_task_memory_refs_task FOREIGN KEY (task_id) REFERENCES tasks(id) ON DELETE CASCADE,
    CONSTRAINT fk_task_memory_refs_note FOREIGN KEY (note_id) REFERENCES notes(id) ON DELETE CASCADE
);

CREATE INDEX idx_task_memory_refs_note_id ON task_memory_refs(note_id);

CREATE TABLE IF NOT EXISTS epic_memory_refs (
    epic_id     VARCHAR(36) NOT NULL,
    note_id     VARCHAR(36) NOT NULL,
    relation    VARCHAR(64) NOT NULL DEFAULT 'context',
    created_at  VARCHAR(64) NOT NULL DEFAULT "",
    PRIMARY KEY (epic_id, note_id),
    CONSTRAINT fk_epic_memory_refs_epic FOREIGN KEY (epic_id) REFERENCES epics(id) ON DELETE CASCADE,
    CONSTRAINT fk_epic_memory_refs_note FOREIGN KEY (note_id) REFERENCES notes(id) ON DELETE CASCADE
);

CREATE INDEX idx_epic_memory_refs_note_id ON epic_memory_refs(note_id);

-- ── sessions ────────────────────────────────────────────────────────────────
CREATE TABLE IF NOT EXISTS sessions (
    id              VARCHAR(36)  NOT NULL PRIMARY KEY,
    project_id      VARCHAR(36)  NOT NULL,
    task_id         VARCHAR(36)  NULL,
    model_id        VARCHAR(255) NOT NULL,
    agent_type      VARCHAR(64)  NOT NULL,
    started_at      VARCHAR(64)  NOT NULL DEFAULT (DATE_FORMAT(NOW(3), '%Y-%m-%dT%H:%i:%s.%fZ')),
    ended_at        VARCHAR(64)  NULL,
    `status`        VARCHAR(64)  NOT NULL,
    tokens_in       BIGINT       NOT NULL DEFAULT 0,
    tokens_out      BIGINT       NOT NULL DEFAULT 0,
    worktree_path   VARCHAR(1024) NULL,
    event_taxonomy  LONGTEXT     NULL,
    CONSTRAINT fk_sessions_project FOREIGN KEY (project_id) REFERENCES projects(id) ON DELETE CASCADE,
    CONSTRAINT fk_sessions_task    FOREIGN KEY (task_id)    REFERENCES tasks(id)    ON DELETE SET NULL
);

CREATE INDEX idx_sessions_project_id ON sessions(project_id);
CREATE INDEX idx_sessions_task_id    ON sessions(task_id);
CREATE INDEX idx_sessions_status     ON sessions(`status`);
CREATE INDEX idx_sessions_started_at ON sessions(started_at);

-- ── session_messages ────────────────────────────────────────────────────────
CREATE TABLE IF NOT EXISTS session_messages (
    id            VARCHAR(36) NOT NULL PRIMARY KEY,
    session_id    VARCHAR(36) NOT NULL,
    role          VARCHAR(64) NOT NULL,
    content_json  LONGTEXT    NOT NULL,
    token_count   BIGINT      NULL,
    created_at    VARCHAR(64) NOT NULL DEFAULT "",
    CONSTRAINT fk_session_messages_session FOREIGN KEY (session_id) REFERENCES sessions(id) ON DELETE CASCADE
);

CREATE INDEX idx_session_messages_session_id_created_at ON session_messages(session_id, created_at);

-- ── consolidated_note_provenance (needs both notes and sessions) ───────────
CREATE TABLE IF NOT EXISTS consolidated_note_provenance (
    note_id    VARCHAR(36) NOT NULL,
    session_id VARCHAR(36) NOT NULL,
    created_at VARCHAR(64) NOT NULL DEFAULT "",
    PRIMARY KEY (note_id, session_id),
    CONSTRAINT fk_cnp_note    FOREIGN KEY (note_id)    REFERENCES notes(id)    ON DELETE CASCADE,
    CONSTRAINT fk_cnp_session FOREIGN KEY (session_id) REFERENCES sessions(id) ON DELETE CASCADE
);

CREATE INDEX idx_consolidated_note_provenance_session_id ON consolidated_note_provenance(session_id);

-- ── credentials (encrypted provider API keys) ──────────────────────────────
CREATE TABLE IF NOT EXISTS credentials (
    id              VARCHAR(36)  NOT NULL PRIMARY KEY,
    provider_id     VARCHAR(191) NOT NULL,
    key_name        VARCHAR(191) NOT NULL,
    encrypted_value LONGBLOB     NOT NULL,
    created_at      VARCHAR(64)  NOT NULL DEFAULT (DATE_FORMAT(NOW(3), '%Y-%m-%dT%H:%i:%s.%fZ')),
    updated_at      VARCHAR(64)  NOT NULL DEFAULT (DATE_FORMAT(NOW(3), '%Y-%m-%dT%H:%i:%s.%fZ')),
    UNIQUE KEY uq_credentials_key_name (key_name)
);

CREATE INDEX credentials_provider_id ON credentials(provider_id);

-- ── custom_providers (user-registered OpenAI-compatible providers) ─────────
CREATE TABLE IF NOT EXISTS custom_providers (
    id          VARCHAR(36)  NOT NULL PRIMARY KEY,
    name        VARCHAR(255) NOT NULL,
    base_url    VARCHAR(1024) NOT NULL,
    env_var     VARCHAR(128) NOT NULL,
    seed_models LONGTEXT     NOT NULL,
    created_at  VARCHAR(64)  NOT NULL DEFAULT (DATE_FORMAT(NOW(3), '%Y-%m-%dT%H:%i:%s.%fZ'))
);

-- ── model_health (provider/model reachability rollup) ──────────────────────
CREATE TABLE IF NOT EXISTS model_health (
    provider       VARCHAR(128) NOT NULL,
    model          VARCHAR(191) NOT NULL,
    `status`       VARCHAR(64)  NOT NULL,
    latency_ms     BIGINT       NULL,
    success_rate   DOUBLE       NULL,
    sample_size    BIGINT       NULL,
    error_message  LONGTEXT     NULL,
    details        LONGTEXT     NULL,
    checked_at     VARCHAR(64)  NOT NULL DEFAULT (DATE_FORMAT(NOW(3), '%Y-%m-%dT%H:%i:%s.%fZ')),
    PRIMARY KEY (provider, model)
);

-- ── verification_cache (per-commit verification output cache) ──────────────
CREATE TABLE IF NOT EXISTS verification_cache (
    project_id   VARCHAR(36)  NOT NULL,
    commit_sha   VARCHAR(64)  NOT NULL,
    output       LONGTEXT     NOT NULL,
    duration_ms  BIGINT       NULL,
    created_at   VARCHAR(64)  NOT NULL DEFAULT (DATE_FORMAT(NOW(3), '%Y-%m-%dT%H:%i:%s.%fZ')),
    PRIMARY KEY (project_id, commit_sha),
    CONSTRAINT fk_verification_cache_project FOREIGN KEY (project_id) REFERENCES projects(id) ON DELETE CASCADE
);

CREATE INDEX idx_verification_cache_created_at ON verification_cache(created_at);

-- ── verification_results (durable per-step results for task verify runs) ───
-- NOTE: `id` uses UUID() expression default so inserts that omit `id`
-- (see verification_result.rs) still populate a primary-key value.
CREATE TABLE IF NOT EXISTS verification_results (
    id          VARCHAR(36)  NOT NULL PRIMARY KEY DEFAULT (UUID()),
    project_id  VARCHAR(36)  NOT NULL,
    task_id     VARCHAR(36)  NULL,
    run_id      VARCHAR(36)  NOT NULL,
    phase       VARCHAR(32)  NOT NULL,
    step_index  INT          NOT NULL,
    name        VARCHAR(255) NOT NULL,
    command     LONGTEXT     NOT NULL,
    exit_code   INT          NOT NULL,
    stdout      LONGTEXT     NOT NULL,
    stderr      LONGTEXT     NOT NULL,
    duration_ms BIGINT       NOT NULL DEFAULT 0,
    created_at  VARCHAR(64)  NOT NULL DEFAULT (DATE_FORMAT(NOW(3), '%Y-%m-%dT%H:%i:%s.%fZ'))
);

CREATE INDEX idx_verification_results_task    ON verification_results(task_id, created_at);
CREATE INDEX idx_verification_results_project ON verification_results(project_id, created_at);
CREATE INDEX idx_verification_results_run     ON verification_results(run_id);

-- ── repo_map_cache (per-worktree rendered repo map) ────────────────────────
-- Composite primary key with nullable worktree_path: MySQL tolerates NULL in
-- PRIMARY KEY only via UNIQUE KEY, so we use a synthetic surrogate key and a
-- unique index over the composite. Callers ON DUPLICATE KEY UPDATE against the
-- composite unique, so that is what matters.
CREATE TABLE IF NOT EXISTS repo_map_cache (
    project_id        VARCHAR(36)  NOT NULL,
    project_path      VARCHAR(512) NOT NULL,
    worktree_path     VARCHAR(512) NOT NULL DEFAULT '',
    commit_sha        VARCHAR(64)  NOT NULL,
    rendered_map      LONGTEXT     NOT NULL,
    token_estimate    BIGINT       NOT NULL,
    included_entries  BIGINT       NOT NULL,
    graph_artifact    LONGTEXT     NULL,
    created_at        VARCHAR(64)  NOT NULL DEFAULT (DATE_FORMAT(NOW(3), '%Y-%m-%dT%H:%i:%s.%fZ')),
    PRIMARY KEY (project_id, project_path, worktree_path, commit_sha)
);

-- ── repo_graph_cache (canonical SCIP graph cache, ADR-050) ────────────────
CREATE TABLE IF NOT EXISTS repo_graph_cache (
    project_id VARCHAR(36) NOT NULL,
    commit_sha VARCHAR(64) NOT NULL,
    graph_blob LONGBLOB    NOT NULL,
    built_at   VARCHAR(64) NOT NULL DEFAULT "",
    PRIMARY KEY (project_id, commit_sha)
);

-- ── agents (per-project role definitions) ──────────────────────────────────
CREATE TABLE IF NOT EXISTS agents (
    id                       VARCHAR(36)  NOT NULL PRIMARY KEY,
    project_id               VARCHAR(36)  NOT NULL,
    name                     VARCHAR(255) NOT NULL,
    base_role                VARCHAR(64)  NOT NULL,
    description              LONGTEXT     NOT NULL,
    system_prompt_extensions LONGTEXT     NOT NULL,
    model_preference         VARCHAR(255) NULL,
    verification_command     LONGTEXT     NULL,
    mcp_servers              LONGTEXT     NOT NULL,
    skills                   LONGTEXT     NOT NULL,
    is_default               BOOLEAN      NOT NULL DEFAULT FALSE,
    learned_prompt           LONGTEXT     NULL,
    created_at               VARCHAR(64)  NOT NULL DEFAULT (DATE_FORMAT(NOW(3), '%Y-%m-%dT%H:%i:%s.%fZ')),
    updated_at               VARCHAR(64)  NOT NULL DEFAULT (DATE_FORMAT(NOW(3), '%Y-%m-%dT%H:%i:%s.%fZ')),
    UNIQUE KEY uq_agents_project_name (project_id, name),
    CONSTRAINT fk_agents_project FOREIGN KEY (project_id) REFERENCES projects(id) ON DELETE CASCADE
);

CREATE INDEX agents_project_id          ON agents(project_id);
CREATE INDEX agents_base_role           ON agents(project_id, base_role);
CREATE INDEX agents_is_default          ON agents(project_id, is_default);

-- Partial-unique emulation: MySQL does not support WHERE in CREATE INDEX, but
-- Dolt does via INDEX with WHERE. We use a generated column to approximate the
-- SQLite partial unique (project_id, base_role) WHERE is_default = 1.
ALTER TABLE agents
    ADD COLUMN default_key VARCHAR(128) GENERATED ALWAYS AS
        (CASE WHEN is_default = TRUE
              THEN CONCAT(project_id, ':', base_role)
              ELSE NULL END) VIRTUAL;

CREATE UNIQUE INDEX uq_agents_one_default_per_base_role ON agents(default_key);

-- ── learned_prompt_history (architect improvement-loop audit trail) ────────
CREATE TABLE IF NOT EXISTS learned_prompt_history (
    id              VARCHAR(36)  NOT NULL PRIMARY KEY,
    agent_id        VARCHAR(36)  NOT NULL,
    proposed_text   LONGTEXT     NOT NULL,
    action          VARCHAR(32)  NOT NULL,
    metrics_before  LONGTEXT     NULL,
    metrics_after   LONGTEXT     NULL,
    created_at      VARCHAR(64)  NOT NULL DEFAULT (DATE_FORMAT(NOW(3), '%Y-%m-%dT%H:%i:%s.%fZ')),
    CONSTRAINT fk_learned_prompt_history_agent FOREIGN KEY (agent_id) REFERENCES agents(id) ON DELETE CASCADE
);

CREATE INDEX learned_prompt_history_agent_id ON learned_prompt_history(agent_id);
