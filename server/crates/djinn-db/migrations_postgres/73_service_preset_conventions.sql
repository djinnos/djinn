-- 73_service_preset_conventions.sql
--
-- Two codebase-agnostic improvements to injectable backing-service presets:
--
-- 1. Expose a preset's rendered connection string under the conventional
--    `DATABASE_URL` env var (the name most projects/agents reach for), in
--    addition to the bespoke `TEST_POSTGRES_URL`. The `conn_env_var` column is
--    overloaded as a comma-separated LIST of env-var names; the sidecar exports
--    the same connection string under each. `DATABASE_URL` is listed first as
--    the conventional default; `TEST_POSTGRES_URL` is retained for back-compat.
--    Redis (`REDIS_URL`) / RabbitMQ (`AMQP_URL`) keep their single names.
--
-- 2. Per-preset client CLI package. When an image declares a service, the
--    catalog-image build auto-installs that service's command-line client so an
--    agent can reach for `psql` / `redis-cli` by default. Data-driven: a future
--    preset just sets its own `client_package`. NULL = no client to install
--    (e.g. RabbitMQ has no stable common CLI client package).

-- Change 1: multi-name connection env vars for the Postgres preset.
UPDATE service_presets
   SET conn_env_var = 'DATABASE_URL,TEST_POSTGRES_URL'
 WHERE id = 'preset-postgres-18';

-- Change 2: per-preset client CLI package, auto-installed into images that
-- attach the preset.
ALTER TABLE service_presets ADD COLUMN IF NOT EXISTS client_package TEXT;

UPDATE service_presets SET client_package = 'postgresql-client' WHERE id = 'preset-postgres-18';
UPDATE service_presets SET client_package = 'redis-tools'       WHERE id = 'preset-redis-7';
-- RabbitMQ intentionally left NULL: no stable common client package.
