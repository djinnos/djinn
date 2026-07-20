# Proposal MCP Output Ergonomics — Migration Guide (z5lq)

## Summary

`proposal_list` and `proposal_show` now return compact-by-default payloads to
keep MCP prompt context lean.  Callers that need the full proposal body text
must opt in explicitly.

---

## `proposal_list`

**Before:** every list row included the full `body` string.

**After (default):** rows are bounded summaries with identity, lifecycle/time, workflow fields, optional `list_summary`, and integer `ac_total`/`ac_met`. They omit body, excerpt metadata, criteria, and detail bookkeeping.

| Field | Type | Description |
|---|---|---|
| `ac_total` | integer | All stored criteria, including legacy strings |
| `ac_met` | integer | Object criteria explicitly marked `met: true` |

### Migration

Request optional list fields independently. Omitted and `false` are equivalent; `include_bodies: true` also implies excerpt metadata:

```json
{ "include_excerpts": true, "include_acceptance_criteria": true }
```

Use `proposal_show` as the sole complete deep dive for body format, revision, closure, evidence, and other omitted detail.

---

## `proposal_show`

### Field selection (`fields`)

**Before:** the response always included every section
(`proposal`, `targets`, `feedback`, `signoffs`, `revisions`,
`debate`, `epics`, `gate_status`).

**After (default):** all sections are still returned unless you pass
`fields` to select a subset.  Omitted sections are absent from the
response and their data is not loaded.

```json
{ "fields": ["proposal", "targets", "revisions"] }
```

Invalid field names return a validation error listing the accepted values.

### Revision body verbosity (`revision_bodies`)

**Before:** revision entries always included the full `body` string.

**After (default):** revision entries use `excerpt` mode — each entry has
`body_excerpt` (first 512 chars) and `body_truncated`, while `body` is `null`.

| Value     | `body`    | `body_excerpt` | `body_truncated` | Notes                        |
|-----------|-----------|----------------|-------------------|------------------------------|
| `excerpt` | `null`    | ✅             | ✅                | **Default**                  |
| `full`    | ✅        | ✅             | ✅                | Full text + excerpt metadata |
| `omit`    | `null`    | `null`         | `null`            | No body data at all          |

### Migration

Pass `revision_bodies: "full"` to restore the full revision body text:

```json
{ "revision_bodies": "full" }
```

When `fields` omits `"revisions"`, the `revision_bodies` parameter is ignored.

The current proposal body (the `body` field under the `proposal` section) is
always full when the `proposal` field is selected — it is never excerpted.

---

## Payload budget guarantees

- **`proposal_list`** with 50 rows of 4,096-character bodies stays ≤ 32,768 bytes by default (summary mode). Full bodies are opt-in.
- **`proposal_show`** with 25 revisions of 4,096-character bodies stays ≤ 64 KiB
  by default (excerpt mode).  Full revision bodies are available with
  `revision_bodies: "full"`.

---

## Sibling list tool audit

| Tool          | Bounded? | Notes |
|---------------|----------|-------|
| `task_list`   | ⚠️ No   | List rows include full `description` and `design` strings with no length cap. Potential follow-up: add excerpt/default-truncation similar to proposal_list. |
| `epic_list`   | ⚠️ No   | List rows include full `description` string. Same potential follow-up. |
| `memory_list` | ✅ Yes  | Uses `list_compact` — returns compact summaries without full note content. |
| `session_list`| ✅ Yes  | Returns only session metadata (id, model, agent_type, status, tokens). No unbounded text fields. |

The `task_list` and `epic_list` findings are documented but out of scope for
this task.  A separate planner note should be filed if the same
summary-default principle is desired for those tools.

---

## Excerpt helper semantics

Excerpts are capped at exactly **512 Unicode scalar values** (not bytes).
No ellipsis is appended.  The `body_truncated` boolean is `true` when the
stored content exceeded 512 scalars, `false` otherwise.
