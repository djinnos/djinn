-- Migration 10: Per-project runtime environment config — first-class JSON
-- replacing the `.devcontainer/devcontainer.json` read path.
--
-- Authoritative plan: djinn-native environment_config refactor.
-- Schema: see `djinn_stack::environment::EnvironmentConfig`.
--
-- JSONB holds the structured config; DEFAULT '{}'::jsonb means existing
-- rows get an empty config after this migration applies. The P5 reseed
-- hook uses that emptiness as its trigger — any row where the column is
-- still `{}` gets auto-seeded from the stack detector on first boot
-- after cut-over.

ALTER TABLE projects
    ADD COLUMN environment_config JSONB NOT NULL DEFAULT '{}'::jsonb;
