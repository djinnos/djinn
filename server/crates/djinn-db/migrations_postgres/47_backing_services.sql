-- 47_backing_services.sql
--
-- On-demand backing services (Phase C): a worker requests a Postgres/RabbitMQ/
-- Redis instance via MCP at test time; the control plane provisions a separate
-- ephemeral pod + ClusterIP Service for the task, returns connection info, and
-- tears it down with the task. Which presets a project MAY request is declared
-- on its selected IMAGE (capability); a per-project kill-switch is the policy.
--
-- All additive + INERT until an image declares allowed presets AND a worker
-- requests one — so shipping the schema/code changes nothing on the live system.

CREATE TABLE IF NOT EXISTS service_presets (
    id           VARCHAR(36)  NOT NULL PRIMARY KEY,
    name         VARCHAR(255) NOT NULL,
    service_type VARCHAR(64)  NOT NULL,   -- postgres | rabbitmq | redis
    image        VARCHAR(512) NOT NULL,
    port         INT          NOT NULL,
    -- Container env (e.g. POSTGRES_PASSWORD).
    env          JSONB        NOT NULL DEFAULT '{}'::jsonb,
    -- {cpu_request, memory_request, cpu_limit, memory_limit}.
    resources    JSONB        NOT NULL DEFAULT '{}'::jsonb,
    -- Connection string template with {host}/{port} placeholders.
    conn_template VARCHAR(512) NOT NULL,
    -- Env var the worker exports for this connection (e.g. TEST_POSTGRES_URL).
    conn_env_var  VARCHAR(128) NOT NULL,
    created_at   VARCHAR(64)  NOT NULL DEFAULT to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
    updated_at   VARCHAR(64)  NOT NULL DEFAULT to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
    CONSTRAINT uq_service_presets_name UNIQUE (name)
);

-- Capability: which presets an image's tasks MAY request.
CREATE TABLE IF NOT EXISTS image_allowed_presets (
    image_id  VARCHAR(36) NOT NULL,
    preset_id VARCHAR(36) NOT NULL,
    PRIMARY KEY (image_id, preset_id),
    CONSTRAINT fk_iap_image  FOREIGN KEY (image_id)  REFERENCES images(id)          ON DELETE CASCADE,
    CONSTRAINT fk_iap_preset FOREIGN KEY (preset_id) REFERENCES service_presets(id) ON DELETE CASCADE
);

-- Provisioning state machine — one instance per service-type per task (idempotent).
CREATE TABLE IF NOT EXISTS service_instances (
    id           VARCHAR(36) NOT NULL PRIMARY KEY,
    task_run_id  VARCHAR(36) NOT NULL,
    project_id   VARCHAR(36) NOT NULL,
    preset_id    VARCHAR(36) NOT NULL,
    service_type VARCHAR(64) NOT NULL,
    status       VARCHAR(32) NOT NULL,  -- requested | provisioning | ready | failed | released
    pod_name     VARCHAR(255) NULL,
    service_name VARCHAR(255) NULL,
    secret_ref   VARCHAR(255) NULL,     -- reserved: conn info as a task-scoped secret
    conn_env_var VARCHAR(128) NULL,
    conn_string  VARCHAR(512) NULL,     -- rendered connection string (for idempotent re-request)
    error        TEXT         NULL,
    created_at   VARCHAR(64)  NOT NULL DEFAULT to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
    ready_at     VARCHAR(64)  NULL,
    released_at  VARCHAR(64)  NULL,
    CONSTRAINT uq_service_instances_task_type UNIQUE (task_run_id, service_type)
);

-- Per-project authorization kill-switch (no concurrency count — a project never
-- needs >1 of the same service; the bound is concurrent tasks × ≤1/type).
CREATE TABLE IF NOT EXISTS service_policy (
    project_id     VARCHAR(36) NOT NULL PRIMARY KEY,
    allow_services BOOLEAN     NOT NULL DEFAULT TRUE,
    updated_at     VARCHAR(64) NOT NULL DEFAULT to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
    CONSTRAINT fk_service_policy_project FOREIGN KEY (project_id) REFERENCES projects(id) ON DELETE CASCADE
);

CREATE INDEX service_instances_task_run ON service_instances(task_run_id);
CREATE INDEX service_instances_status ON service_instances(status);

-- Seed the default presets. RabbitMQ is the no-management alpine variant
-- (lighter); Postgres caps shared_buffers so the 64Mi default /dev/shm doesn't
-- block startup. Resources are requests+limits per service.
INSERT INTO service_presets (id, name, service_type, image, port, env, resources, conn_template, conn_env_var) VALUES
 ('preset-postgres-18', 'Postgres 18', 'postgres', 'postgres:18-alpine', 5432,
  '{"POSTGRES_PASSWORD":"postgres","POSTGRES_USER":"postgres","POSTGRES_DB":"app_test","POSTGRES_INITDB_ARGS":"-c shared_buffers=64MB"}'::jsonb,
  '{"cpu_request":"100m","memory_request":"256Mi","cpu_limit":"500m","memory_limit":"512Mi"}'::jsonb,
  'postgres://postgres:postgres@{host}:5432/app_test?sslmode=disable', 'TEST_POSTGRES_URL'),
 ('preset-redis-7', 'Redis 7', 'redis', 'redis:7-alpine', 6379,
  '{}'::jsonb,
  '{"cpu_request":"50m","memory_request":"32Mi","cpu_limit":"200m","memory_limit":"128Mi"}'::jsonb,
  'redis://{host}:6379', 'REDIS_URL'),
 ('preset-rabbitmq-4', 'RabbitMQ 4', 'rabbitmq', 'rabbitmq:4-alpine', 5672,
  '{}'::jsonb,
  '{"cpu_request":"100m","memory_request":"256Mi","cpu_limit":"500m","memory_limit":"512Mi"}'::jsonb,
  'amqp://guest:guest@{host}:5672', 'AMQP_URL')
ON CONFLICT (id) DO NOTHING;
