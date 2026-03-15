-- Custom provider registry for user-added OpenAI-compatible providers.
--
-- Each row is an unlisted provider the user has registered via provider_add_custom.
-- seed_models is a JSON array of {id, name} objects to pre-populate the model picker.

CREATE TABLE custom_providers (
    id          TEXT NOT NULL PRIMARY KEY,
    name        TEXT NOT NULL,
    base_url    TEXT NOT NULL,
    env_var     TEXT NOT NULL,
    seed_models TEXT NOT NULL DEFAULT '[]',  -- JSON: [{id, name}, ...]
    created_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);
