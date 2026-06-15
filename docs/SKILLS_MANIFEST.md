# Skills manifest runbook

Djinn projects can check in a generated `.djinn/skills.json` manifest so worker prompt materialization and `skill_read` fail closed when materialized skill content is stale or tampered with.

The manifest covers both progressive-disclosure prompt summaries and on-demand skill bodies:

- required skills (`required: true`) remain fully inlined in the `## Available Skills` prompt section and are still verified before prompt materialization;
- non-required skills disclose only their `name` and `description` plus a `skill_read(...)` hint under progressive disclosure, while their full body is verified and served later by `skill_read`;
- directory-style `SKILL.md` skills include non-empty files under `references/` in the effective body, so reference edits also change the manifest.

## Manifest fields

Top-level fields:

- `schema_version`: manifest schema version. Regenerate the manifest when this changes.
- `generated_by`: generator identifier for traceability.
- `skills`: deterministic list of discovered skills, sorted by stable skill id.

Per-skill fields:

- `id`: stable requested skill name used to resolve the skill from `.claude/skills/`, `.opencode/skills/`, or `.djinn/skills/`.
- `name`: resolved display name from frontmatter `name:` or the requested id fallback.
- `description`: frontmatter description shown in progressive-disclosure summaries.
- `required`: frontmatter flag. `true` means the full body is inlined even when progressive disclosure is enabled.
- `trust_level`: optional frontmatter metadata, defaulting to `project`.
- `recommended_for_roles`: optional frontmatter list.
- `tags`: optional frontmatter list.
- `summary_hash`: `sha256:` digest over the canonical progressive-disclosure summary tuple: `name`, `description`, and `required`.
- `content_hash`: `sha256:` digest over the effective served body (`ResolvedSkill.content`) returned by `skill_read` and used for full prompt inlining. For directory `SKILL.md` skills, this includes sorted non-empty `references/` content.
- `source_files`: all files that contribute to the skill, with project-relative POSIX paths, raw file sha256, and a `role` of `skill` or `reference`.

## Generate or update

After changing any skill markdown, frontmatter, or `references/` file, regenerate and commit `.djinn/skills.json`:

```sh
make skills-manifest-generate
```

This runs:

```sh
cd server && cargo run -p djinn-agent --bin djinn-skills-manifest -- generate --root ..
```

## Local and CI drift check

Before opening a PR, verify the checked manifest matches freshly generated output:

```sh
make skills-manifest-check
```

CI runs the same drift guard in `.github/workflows/quality-gate.yml`:

```sh
cd server && cargo run -p djinn-agent --bin djinn-skills-manifest -- check --root ..
```

The check regenerates the manifest in memory and compares the pretty JSON bytes with `.djinn/skills.json`. A mismatch means a skill file, reference file, or manifest schema changed without committing the regenerated manifest.

## Runtime mismatch and tamper behavior

When `.djinn/skills.json` exists, runtime loading verifies every requested skill before using it:

- prompt materialization verifies required and non-required skills before rendering `## Available Skills`;
- `skill_read` verifies again before returning an on-demand body;
- changing `name`, `description`, or `required` triggers a metadata or `summary_hash` failure;
- changing the skill body triggers a `content_hash` failure;
- changing, adding, deleting, or replacing a directory skill `references/` file triggers source-file and/or content-hash drift;
- requesting a skill that is not present in the checked manifest is rejected.

For legacy projects without `.djinn/skills.json`, loading remains permissive. Once the manifest exists, verification fails closed: stale or tampered skills are not silently served to the model. If the mismatch is intentional, run `make skills-manifest-generate` and commit the updated `.djinn/skills.json`; otherwise restore the skill/reference file to the manifested content.
