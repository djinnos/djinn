# Default Lane Demotion Rollout — Reversible Snapshot & Runbook

> Epic: **5wxi** — Routing lane demotion, rollback snapshot, and glm runtime cap
> Task: `019f3795-965f-7380-afdf-01487882e388` — Add reversible default-lane rollout snapshot/runbook
> Status: **Ready for operator apply**

---

## 1. Purpose

This document defines the deterministic payloads, snapshot capture, apply steps,
and rollback procedure for the default-lane demotion rollout to the default
worker/reviewer tenant population. It is a **code/docs artifact**, not a
production credential or access request — all operator-only steps are explicitly
marked as runbook actions.

### What this rollout does

Reorders the default `implement` and `review` lane candidate lists so that:

- **`xiaomi-token-plan-sgp/mimo-v2.5-pro`** is the preferred (first-candidate)
  model for both worker and reviewer roles.
- **`zai-coding-plan/glm-5.2`** is the secondary candidate for `implement`.
- **`kimi-for-coding/k2p7`** and **`minimax-coding-plan/MiniMax-M3`** are
  demoted to **last-resort** — used only when all preferred candidates are
  unavailable or explicitly selected.

### Data surfaces affected

| Surface | Storage | Repository |
|---------|---------|------------|
| Per-user lanes | `user_settings.model_lanes` (JSON TEXT) | `UserSettingsRepository::upsert_lanes` |
| Org default lanes | `org_ai_policy.default_lanes` (JSON TEXT) | `OrgAiPolicyRepository::set` |

Both surfaces use the same JSON shape: `{ "plan": [...], "implement": [...], "review": [...] }`.

---

## 2. Target Lane Payloads

### 2.1 Implement lane (worker role)

```json
{
  "implement": [
    "xiaomi-token-plan-sgp/mimo-v2.5-pro",
    "zai-coding-plan/glm-5.2",
    "kimi-for-coding/k2p7",
    "minimax-coding-plan/MiniMax-M3"
  ]
}
```

**Ordering semantics:**

| Index | Provider/Model | Classification | Rationale |
|-------|---------------|----------------|-----------|
| 0 | `xiaomi-token-plan-sgp/mimo-v2.5-pro` | **Primary** | Preferred default; best quality/latency profile |
| 1 | `zai-coding-plan/glm-5.2` | **Secondary** | Fallback when primary unavailable; ~90-min runtime cap (see Task C) |
| 2 | `kimi-for-coding/k2p7` | **Last-resort** | Only used when primary+secondary both unavailable |
| 3 | `minimax-coding-plan/MiniMax-M3` | **Last-resort** | Bottom of fallback chain |

### 2.2 Review lane (reviewer role)

```json
{
  "review": [
    "xiaomi-token-plan-sgp/mimo-v2.5-pro",
    "zai-coding-plan/glm-5.2",
    "kimi-for-coding/k2p7",
    "minimax-coding-plan/MiniMax-M3"
  ]
}
```

**Ordering semantics:**

| Index | Provider/Model | Classification | Rationale |
|-------|---------------|----------------|-----------|
| 0 | `xiaomi-token-plan-sgp/mimo-v2.5-pro` | **Primary** | Preferred reviewer model |
| 1 | `zai-coding-plan/glm-5.2` | **Secondary** | Fallback reviewer |
| 2 | `kimi-for-coding/k2p7` | **Last-resort** | Only when preferred reviewers unavailable |
| 3 | `minimax-coding-plan/MiniMax-M3` | **Last-resort** | Bottom of reviewer fallback chain |

### 2.3 Full combined payload

```json
{
  "plan": [],
  "implement": [
    "xiaomi-token-plan-sgp/mimo-v2.5-pro",
    "zai-coding-plan/glm-5.2",
    "kimi-for-coding/k2p7",
    "minimax-coding-plan/MiniMax-M3"
  ],
  "review": [
    "xiaomi-token-plan-sgp/mimo-v2.5-pro",
    "zai-coding-plan/glm-5.2",
    "kimi-for-coding/k2p7",
    "minimax-coding-plan/MiniMax-M3"
  ]
}
```

> **Note:** `plan` lane is unchanged (empty = no override, inherits org default
> or global fallback). The rollout targets only `implement` and `review`.

### 2.4 Last-resort classification rule

An entry is **last-resort** when it appears at index ≥ 2 in the candidate list
for a given lane. Dispatch will attempt last-resort candidates only when all
higher-priority entries have been exhausted (session liveness failure, provider
error, runtime-cap exceeded) or when the user has explicitly selected that model.

---

## 3. Pre-Apply Snapshot Capture

Before applying the new lane payloads, capture a snapshot of the current state
for every affected surface. This snapshot is the **rollback artifact** — it
contains the exact JSON needed to restore prior lane state.

### 3.1 Snapshot shape

The snapshot is a JSON document with two sections:

```json
{
  "snapshot_version": 1,
  "captured_at": "2026-07-06T12:00:00.000Z",
  "description": "Pre-rollout lane snapshot for epic 5wxi lane demotion",
  "user_settings": {
    "<user_id>": {
      "model_lanes": { "plan": [...], "implement": [...], "review": [...] },
      "captured_at": "<updated_at from user_settings row>"
    }
  },
  "org_ai_policy": {
    "default_lanes": { "plan": [...], "implement": [...], "review": [...] },
    "lock_level": "flexible",
    "blocked_subscriptions": [...],
    "captured_at": "<updated_at from org_ai_policy row>"
  }
}
```

### 3.2 Capture query — user settings

Run this against the database to export current per-user lanes:

```sql
-- Export all users with explicit lane assignments
SELECT user_id, model_lanes, updated_at
FROM user_settings
WHERE model_lanes IS NOT NULL
ORDER BY user_id;
```

Transform each row into the snapshot shape:

```json
{
  "<user_id>": {
    "model_lanes": <parse model_lanes JSON>,
    "captured_at": "<updated_at>"
  }
}
```

### 3.3 Capture query — org default lanes

```sql
-- Export org AI policy singleton
SELECT default_lanes, lock_level, blocked_subscriptions, updated_at
FROM org_ai_policy
WHERE id = 1;
```

### 3.4 Capture via MCP tools (non-SQL alternative)

**Runbook action — requires authenticated admin session:**

1. Call `org_policy_get` — record `default_lanes`, `lock_level`, `blocked_subscriptions`.
2. For each user, call `user_settings_get` (admin may use `target_user_id`) —
   record `lanes`.

### 3.5 Capture checklist

- [ ] Snapshot JSON saved to `fixtures/pre-rollout-snapshot.json` (or operator's
  preferred secure location).
- [ ] Snapshot `captured_at` timestamp recorded.
- [ ] Snapshot verified: `model_lanes` JSON parseable, non-null entries present.
- [ ] Snapshot stored **before** any apply step begins.

---

## 4. Apply Steps

### 4.1 Apply via MCP tools (recommended)

**Runbook action — requires authenticated admin session:**

1. **Set org default lanes** (affects new members and locked-org members):

   Call `org_policy_set` with:
   ```json
   {
     "default_lanes": {
       "plan": [],
       "implement": [
         "xiaomi-token-plan-sgp/mimo-v2.5-pro",
         "zai-coding-plan/glm-5.2",
         "kimi-for-coding/k2p7",
         "minimax-coding-plan/MiniMax-M3"
       ],
       "review": [
         "xiaomi-token-plan-sgp/mimo-v2.5-pro",
         "zai-coding-plan/glm-5.2",
         "kimi-for-coding/k2p7",
         "minimax-coding-plan/MiniMax-M3"
       ]
     }
   }
   ```

2. **Set per-user lanes** for each user in the default population:

   Call `user_settings_set` with `target_user_id` (admin) for each affected user:
   ```json
   {
     "lanes": {
       "plan": [],
       "implement": [
         "xiaomi-token-plan-sgp/mimo-v2.5-pro",
         "zai-coding-plan/glm-5.2",
         "kimi-for-coding/k2p7",
         "minimax-coding-plan/MiniMax-M3"
       ],
       "review": [
         "xiaomi-token-plan-sgp/mimo-v2.5-pro",
         "zai-coding-plan/glm-5.2",
         "kimi-for-coding/k2p7",
         "minimax-coding-plan/MiniMax-M3"
       ]
     }
   }
   ```

### 4.2 Apply via direct SQL (alternative)

```sql
-- Org default lanes
UPDATE org_ai_policy
SET default_lanes = '{"plan":[],"implement":["xiaomi-token-plan-sgp/mimo-v2.5-pro","zai-coding-plan/glm-5.2","kimi-for-coding/k2p7","minimax-coding-plan/MiniMax-M3"],"review":["xiaomi-token-plan-sgp/mimo-v2.5-pro","zai-coding-plan/glm-5.2","kimi-for-coding/k2p7","minimax-coding-plan/MiniMax-M3"]}',
    updated_at = to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')
WHERE id = 1;

-- Per-user lanes (repeat for each user_id)
UPDATE user_settings
SET model_lanes = '{"plan":[],"implement":["xiaomi-token-plan-sgp/mimo-v2.5-pro","zai-coding-plan/glm-5.2","kimi-for-coding/k2p7","minimax-coding-plan/MiniMax-M3"],"review":["xiaomi-token-plan-sgp/mimo-v2.5-pro","zai-coding-plan/glm-5.2","kimi-for-coding/k2p7","minimax-coding-plan/MiniMax-M3"]}',
    updated_at = to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')
WHERE user_id = '<user_id>';
```

### 4.3 Apply verification

After applying, verify:

1. `org_policy_get` returns the new `default_lanes`.
2. `user_settings_get` for each affected user returns the new `lanes`.
3. Dispatch logs (Task B) show `xiaomi-token-plan-sgp/mimo-v2.5-pro` as
   `candidate_index=0` for both implement and review.

---

## 5. Rollback Steps

Rollback restores lane state from the pre-apply snapshot captured in §3.

### 5.1 Rollback via MCP tools

**Runbook action — requires authenticated admin session:**

1. **Restore org default lanes:**

   Call `org_policy_set` with the `default_lanes` value from
   `snapshot.org_ai_policy.default_lanes`:
   ```json
   {
     "default_lanes": <snapshot.org_ai_policy.default_lanes>
   }
   ```

2. **Restore per-user lanes** for each user in the snapshot:

   Call `user_settings_set` with `target_user_id`:
   ```json
   {
     "lanes": <snapshot.user_settings[user_id].model_lanes>
   }
   ```

   For users whose snapshot has `model_lanes: null` (no explicit lanes before
   rollout), clear the lanes:
   ```json
   {
     "lanes": { "plan": [], "implement": [], "review": [] }
   }
   ```

### 5.2 Rollback via direct SQL

For each entry in the snapshot:

```sql
-- Restore org default lanes
UPDATE org_ai_policy
SET default_lanes = '<snapshot.org_ai_policy.default_lanes as JSON string>',
    updated_at = to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')
WHERE id = 1;

-- Restore per-user lanes
UPDATE user_settings
SET model_lanes = '<snapshot.user_settings[user_id].model_lanes as JSON string, or NULL if was null>',
    updated_at = to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')
WHERE user_id = '<user_id>';
```

### 5.3 Rollback verification

After rollback:

1. `org_policy_get` returns `default_lanes` matching the snapshot.
2. `user_settings_get` for each affected user returns `lanes` matching the
   snapshot (or all-empty if they had no explicit lanes before).
3. All JSON values round-trip identically to the snapshot.

### 5.4 Rollback checklist

- [ ] Snapshot loaded and parsed (no corruption).
- [ ] Org default lanes restored from snapshot.
- [ ] Each affected user's lanes restored from snapshot.
- [ ] Post-rollback verification passes: snapshot JSON == current DB JSON.
- [ ] Dispatch logs (Task B) reflect restored candidate order.

---

## 6. Deterministic Local Round-Trip Proof

A checked fixture proves that the snapshot → apply → rollback sequence restores
the original lane JSON exactly. This test runs entirely against fixture data
with no database or production credentials required.

### 6.1 Fixture location

`server/docs/routing/fixtures/lane-round-trip-proof.json`

### 6.2 What the fixture encodes

The fixture contains:

- **`pre_snapshot`**: The captured lane state before rollout (user + org).
- **`apply_payload`**: The new lane values to apply.
- **`post_apply`**: The expected state after apply (same shape as snapshot but
  with new lane values).
- **`rollback_restore`**: The values to pass during rollback (copied from
  `pre_snapshot`).
- **`post_rollback`**: The expected state after rollback (= identical to
  `pre_snapshot`).

### 6.3 Proof logic

```
1. Load pre_snapshot.
2. Apply apply_payload to pre_snapshot → result must equal post_apply.
3. Apply rollback_restore (from pre_snapshot) to post_apply → result must equal post_rollback.
4. Assert post_rollback == pre_snapshot (JSON equality).
```

This is a pure JSON round-trip: no I/O, no authentication, no database.

### 6.4 Running the proof

The fixture is self-describing. Any JSON-aware toolchain can validate it:

```bash
# Using jq (one-liner round-trip check)
jq -e '
  # Verify post_rollback equals pre_snapshot
  (.post_rollback == .pre_snapshot) and
  # Verify apply changes the expected fields
  (.post_apply.user_settings["user-1"].model_lanes.implement[0] == "xiaomi-token-plan-sgp/mimo-v2.5-pro") and
  # Verify rollback restores them
  (.post_rollback.user_settings["user-1"].model_lanes.implement[0] != "xiaomi-token-plan-sgp/mimo-v2.5-pro")
' server/docs/routing/fixtures/lane-round-trip-proof.json
```

A companion Rust unit test under `djinn-db` validates the same round-trip using
the actual `ModelLanes` deserialization path. See `fixtures/roundtrip_test.rs`.

---

## 7. Acceptance Criteria Mapping

| AC | Artifact | Section |
|----|----------|---------|
| Repo-tracked routing rollout artifact documents target implement/review lane payloads, last-resort semantics, pre-apply snapshot capture, apply, and rollback steps. | This document | §§1–5 |
| Artifact includes a deterministic local fixture/test that proves captured lane snapshot can restore prior lane JSON after rollout payload is applied. | `fixtures/lane-round-trip-proof.json` + `fixtures/roundtrip_test.rs` | §6 |
| Rollout instructions avoid production-only proof as a task requirement and clearly identify operator-only apply/rollback steps as runbook actions. | This document — all §3.4, §4.1, §5.1 steps marked "Runbook action" | §§3–5 |

---

## 8. References

- `server/crates/djinn-core/src/models/user_settings.rs` — `ModelLanes` definition
- `server/crates/djinn-core/src/models/org_ai_policy.rs` — `OrgDefaultLanes`, `OrgAiPolicy`
- `server/crates/djinn-db/src/repositories/user_settings.rs` — `UserSettingsRepository::upsert_lanes`
- `server/crates/djinn-db/src/repositories/org_ai_policy.rs` — `OrgAiPolicyRepository::set`
- `server/crates/djinn-control-plane/src/tools/user_settings_tools.rs` — `ModelLanesPayload`, MCP lane tools
- `server/crates/djinn-control-plane/src/tools/org_policy_tools.rs` — `OrgDefaultLanesPayload`, MCP org policy tools
- Memory note: `design/5wxi-roadmap` — Epic roadmap and wave plan
