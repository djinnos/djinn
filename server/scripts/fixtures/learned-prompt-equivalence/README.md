# Learned-prompt prompt-equivalence fixtures

These fixtures back the `--selftest` mode of
`server/scripts/learned-prompt-equivalence.sh` and demonstrate the three
prompt-equivalence regimes documented in §6 of
`server/docs/learned-prompt-harvest.md`.

Each fixture contains a `pre-assembled.prompt` and a
`post-assembled.prompt` file. The assembled prompts are generated using
the **exact** runtime assembly semantics from
`server/crates/djinn-agent/src/prompts.rs::apply_role_extensions`:

```
out = base
if system_prompt_extensions is non-blank:
    out += "\n\n" + system_prompt_extensions.trim()
if learned_prompt is non-blank:
    out += "\n\n" + learned_prompt.trim()
```

Order: `base → system_prompt_extensions → learned_prompt`.

The `learned_prompt` value (when present) is the
`string_agg(h.proposed_text, E'\n\n---\n\n' ORDER BY h.created_at ASC)`
result from `server/crates/djinn-db/src/repositories/agent.rs`: active
amendments (`action IN ('keep','confirmed')`) joined with the literal
separator `\n\n---\n\n` in `created_at ASC` order.

## `byte-identity/` — fold into `system_prompt_extensions`

- **Disposition:** `fold into project/role system_prompt_extensions`
- **Expected verdict:** `PASS` (byte-identical)
- **Regime:** §6.1

Pre-cutover: `base + "\n\n" + ext + "\n\n" + learned` where `learned`
is two amendments joined by `\n\n---\n\n`.

Post-cutover: `base + "\n\n" + (ext + "\n\n" + learned)` — the learned
text is moved into `system_prompt_extensions` at the same trailing
position previously held by `learned_prompt`. The byte sequence seen by
the model is unchanged.

## `semantic-drift/` — fold into base prompt

- **Disposition:** `fold into base prompt`
- **Expected verdict:** `semantic-rationale-required`
- **Regime:** §6.2

Pre-cutover: `base + "\n\n" + learned` (the amendment appears as a
learned overlay).

Post-cutover: the amendment is folded into the base prompt and **reworded**
for prompt-voice consistency. Byte identity is **not** expected and
**not** required. A semantic rationale must be recorded in §6.2.

## `removed/` — convert to memory note / discard

- **Disposition:** `discard` (or `convert to memory note`)
- **Expected verdict:** `removed`
- **Regime:** §6.3

Pre-cutover: `base + "\n\n" + learned`.

Post-cutover: `base` only — the amendment is intentionally removed from
the assembled prompt. The rationale (stale, duplicate, etc.) is recorded
in §6.3.

---

These fixtures are **not** captured from any live environment. They are
synthetic files that reproduce the runtime assembly semantics so the
helper's `--selftest` can demonstrate byte-identical comparison on a
known-good case and the informational byte diff on intentionally
non-identical cases. A worker environment must not use these as
substitutes for operator-captured pre/post artifacts (see §1.1 / §7 of
the harvest artifact).
