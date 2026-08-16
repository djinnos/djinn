//! Integration coverage for the **bounded** stranded-ready gate exclusions.
//!
//! # The defect these tests pin
//!
//! `stranded_ready_section` excludes a task whose non-dispatch is already
//! explained by a visible gate — a future `dispatch_state.cooldown_until`, a
//! tripped backoff ladder, a revoked owner credential. Those exclusions used to
//! have no time bound at all, and every one of them is a claim about
//! *transience*. A cooldown that is always 30 minutes in the future is not a
//! transient condition; it is a permanent one wearing a transient condition's
//! clothes, and the exclusion turned this section's silence into a guarantee
//! rather than an observation.
//!
//! # What actually happened (2026-08-12 → 2026-08-16)
//!
//! A user's `implement` lane held exactly one model, the model-health breaker
//! hard-disabled it, and every coordinator tick resolved that only candidate →
//! `breaker_open` → `failover_chain_exhausted` → a durable `cooldown_until` at
//! the ~30-minute ladder ceiling. That path deliberately does NOT advance
//! `dispatch_failure_streak` (the task is not at fault), so the state refreshed
//! itself forever and the board sat dead for four days.
//!
//! **The check did fire.** `stranded_ready` emitted critical findings for those
//! tasks throughout, because the cooldown lapsed briefly before each tick
//! re-armed it and some samples landed in that gap. What the unbounded
//! exclusion produced was a *sawtooth blind window* covering most of every
//! ~30-minute cycle — the finding surfaced despite the exclusion, not because
//! of it, and only because the sampling cadence happened to beat the refresh
//! cadence. Nothing guarantees that: a provider-stated `Retry-After` is
//! explicitly allowed to exceed the ladder ceiling (up to
//! `PROVIDER_RETRY_AFTER_MAX`, 6h), and under that shape there is no gap to
//! sample and the same code path is silent for the entire outage.
//!
//! So the tests below never assert "the old code reported nothing during the
//! incident" — that is false. They assert the thing that IS true and is the
//! actual defect: **while a gate is continuously in force, the unbounded
//! exclusion hides the task at every sampled instant, without limit.**
//!
//! (Delivery of the findings that did fire — hundreds of persisted `critical`
//! rows that reached no human — is a separate problem and is not what this
//! change addresses.)

use super::*;

/// 6× the 30-minute base threshold. Mirrors
/// `board_health::GATE_EXCLUSION_BOUND_MINUTES`; asserted against the value the
/// section publishes so the two cannot drift apart silently.
const BOUND_MINUTES: i64 = 180;

/// Seed an open, unclaimed task in its own project.
async fn ready_task(db: &Database, repo: &TaskRepository, title: &str, age: &str) -> Task {
    let project = create_test_project(db).await;
    let epic = create_test_epic(db, &project.id).await;
    let task = repo
        .create_fixture_in_project(
            &project.id,
            Some(&epic.id),
            title,
            "desc",
            "design",
            "task",
            1,
            "worker",
            Some("open"),
            None,
        )
        .await
        .unwrap();
    backdate_task_updated_at(db, &task.id, age).await;
    task
}

/// Write the exact `dispatch_state` shape the breaker-open path leaves behind:
/// a cooldown deadline `minutes_ahead` in the future, an inflight model, and —
/// critically — `failure_streak = 0`, because "the breaker is open for EVERY
/// candidate" is not the task's fault and does not advance the streak.
async fn arm_breaker_cooldown(db: &Database, task_id: &str, model_id: &str, minutes_ahead: i64) {
    sqlx::query(
        "INSERT INTO dispatch_state \
             (task_id, failure_streak, cooldown_until, last_dispatched_role, inflight_model_id) \
         VALUES ($1, 0, now() AT TIME ZONE 'utc' + make_interval(mins => $2::int), \
                 'worker', $3) \
         ON CONFLICT (task_id) DO UPDATE SET \
             cooldown_until = EXCLUDED.cooldown_until, \
             failure_streak = EXCLUDED.failure_streak",
    )
    .bind(task_id)
    .bind(minutes_ahead)
    .bind(model_id)
    .execute(db.pool())
    .await
    .unwrap();
}

/// The model-health rollup row the breaker writes when it hard-disables a model.
async fn hard_disable_model(db: &Database, provider: &str, model: &str) {
    sqlx::query("INSERT INTO model_health (provider, model, status) VALUES ($1, $2, 'down')")
        .bind(provider)
        .bind(model)
        .execute(db.pool())
        .await
        .unwrap();
}

async fn stranded_section(repo: &TaskRepository) -> serde_json::Value {
    repo.board_health(24).await.unwrap()["stranded_ready"].clone()
}

fn finding_for<'a>(section: &'a serde_json::Value, task_id: &str) -> Option<&'a serde_json::Value> {
    section["findings"]
        .as_array()
        .expect("stranded_ready.findings is an array")
        .iter()
        .find(|f| f["id"] == task_id)
}

// ── The incident shape ──────────────────────────────────────────────────────

/// **The regression.** A ready task whose `cooldown_until` is continuously held
/// in the future — re-armed before every sample, exactly as the coordinator
/// re-armed it on every tick — and whose `failure_streak` stays 0.
///
/// Against the unbounded exclusion this task is invisible at *every* sampled
/// instant, for as long as the gate keeps being re-armed: `breaker_open` is
/// true, so `stranded_ready_section` returns `None` before it ever computes a
/// strand age. Four days of strand and three consecutive samples do not change
/// that; there is no quantity of elapsed time the old code consults.
///
/// Past `GATE_EXCLUSION_BOUND_MINUTES` it must be reported, naming the breaker
/// gate and the model.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_continuously_rearmed_breaker_cooldown_is_reported_past_the_bound() {
    let db = create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let repo = TaskRepository::new(db.clone(), event_bus_for(&tx));

    // Four days of strand, matching the 2026-08-12 → 2026-08-16 window.
    let task = ready_task(&db, &repo, "Single-model lane task", "5760 minutes").await;
    hard_disable_model(&db, "openai", "gpt-5.6-terra").await;

    // Sample three times, re-arming the cooldown to the ladder ceiling before
    // each one. This is the sawtooth: at no sampled instant is the deadline in
    // the past, so a `breaker_open` exclusion with no time bound never lets the
    // task through, no matter how long the strand runs.
    for _ in 0..3 {
        arm_breaker_cooldown(&db, &task.id, "openai/gpt-5.6-terra", 30).await;
        let section = stranded_section(&repo).await;

        // The primary claim, asserted first so a regression fails on the thing
        // that matters rather than on a companion field.
        let finding = finding_for(&section, &task.id).unwrap_or_else(|| {
            panic!(
                "a task suppressed by a continuously re-armed breaker cooldown for 5760 \
                 minutes must be reported; section was {section}"
            )
        });
        assert_eq!(
            section["gate_exclusion_bound_minutes"], BOUND_MINUTES,
            "the section must publish the bound it applied"
        );

        // Severity comes from the same strand ladder as every other finding.
        assert_eq!(finding["severity"], "critical");
        assert!(
            finding["elapsed_minutes"].as_i64().unwrap_or(0) >= 5_760,
            "elapsed_minutes: {}",
            finding["elapsed_minutes"]
        );

        // WHICH gate was overridden, and the evidence behind it.
        let escalation = &finding["gate_escalation"];
        assert_eq!(escalation["escalated"], true);
        assert_eq!(
            escalation["overridden_gates"],
            serde_json::json!(["breaker_cooldown"]),
            "the breaker cooldown is the gate that was overridden, and only it"
        );
        assert_eq!(escalation["bound_minutes"], BOUND_MINUTES);
        assert_eq!(escalation["bound_multiple"], 6);
        assert_eq!(
            escalation["suppressed_minutes"], finding["elapsed_minutes"],
            "the suppression clock must be the SAME strand clock, not a second one"
        );
        assert_eq!(
            escalation["evidence"]["inflight_model_id"],
            "openai/gpt-5.6-terra"
        );
        assert_eq!(
            escalation["evidence"]["failure_streak"], 0,
            "the breaker-open path does not advance the streak; the evidence must say so"
        );
        assert_eq!(escalation["evidence"]["last_dispatched_role"], "worker");
        assert!(
            escalation["evidence"]["cooldown_until"]
                .as_str()
                .is_some_and(|cd| cd.ends_with('Z')),
            "the deadline that was suppressing the finding must be carried: {}",
            escalation["evidence"]["cooldown_until"]
        );
        let summary = escalation["summary"].as_str().unwrap_or_default();
        assert!(
            summary.contains("breaker cooldown") && summary.contains("openai/gpt-5.6-terra"),
            "an operator must read the gate and the model in one line: {summary}"
        );

        // The gate is also named in the machine-readable reasons.
        let gate = &finding["dispatch_gate"];
        assert_eq!(
            gate["breaker_open"], true,
            "the gate evidence still reports the breaker as open — it is real"
        );
        assert_eq!(gate["rate_limited"], false);
        assert_eq!(gate["gate_verdict"], "blocked");
        let reasons = gate["reasons"].as_array().unwrap();
        assert!(
            reasons.contains(&serde_json::json!("breaker_cooldown_sustained_past_bound")),
            "expected breaker_cooldown_sustained_past_bound, got {reasons:?}"
        );
    }
}

// ── Transient gates must stay silent ────────────────────────────────────────

/// A gate that is doing its job must still suppress the finding. A task 30
/// minutes into a strand with an hour of cooldown ahead of it is a normal
/// backoff, not an alarm.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_transient_breaker_cooldown_is_still_excluded() {
    let db = create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let repo = TaskRepository::new(db.clone(), event_bus_for(&tx));

    // Past the 30-minute stranded threshold — so it is only the gate keeping it
    // out — but nowhere near the 180-minute bound.
    let task = ready_task(&db, &repo, "Briefly backing off", "31 minutes").await;
    arm_breaker_cooldown(&db, &task.id, "openai/gpt-5.6-terra", 60).await;

    let section = stranded_section(&repo).await;
    assert!(
        finding_for(&section, &task.id).is_none(),
        "a 31-minute strand behind a live cooldown is a normal backoff and must stay silent"
    );
}

/// The same, at the shortest cooldown the ladder produces (60 seconds). A blip
/// must never produce a finding.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_sixty_second_cooldown_blip_is_still_excluded() {
    let db = create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let repo = TaskRepository::new(db.clone(), event_bus_for(&tx));

    let task = ready_task(&db, &repo, "Sixty-second blip", "45 minutes").await;
    arm_breaker_cooldown(&db, &task.id, "openai/gpt-5.6-terra", 1).await;

    let section = stranded_section(&repo).await;
    assert!(
        finding_for(&section, &task.id).is_none(),
        "a 60-second cooldown must not raise an alarm"
    );
}

// ── The boundary itself ─────────────────────────────────────────────────────

/// One minute under the bound is excluded; exactly at the bound and one minute
/// past it are reported. All three share a single `board_health` call so they
/// are measured against one clock reading.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_bound_is_inclusive_at_exactly_six_times_the_threshold() {
    let db = create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let repo = TaskRepository::new(db.clone(), event_bus_for(&tx));

    let under = ready_task(&db, &repo, "One minute under", "179 minutes").await;
    let at = ready_task(&db, &repo, "Exactly at the bound", "180 minutes").await;
    let over = ready_task(&db, &repo, "One minute over", "181 minutes").await;
    for task in [&under, &at, &over] {
        arm_breaker_cooldown(&db, &task.id, "openai/gpt-5.6-terra", 30).await;
    }

    let section = stranded_section(&repo).await;

    assert!(
        finding_for(&section, &under.id).is_none(),
        "179 minutes is under the 180-minute bound and must stay excluded"
    );
    for task in [&at, &over] {
        let finding = finding_for(&section, &task.id)
            .unwrap_or_else(|| panic!("'{}' must be reported at/past the bound", task.title));
        assert_eq!(finding["gate_escalation"]["escalated"], true);
        assert_eq!(
            finding["gate_escalation"]["overridden_gates"],
            serde_json::json!(["breaker_cooldown"])
        );
    }
}

// ── The other bounded gates ─────────────────────────────────────────────────

/// The rate-limit ladder is the second unbounded exclusion. A `failure_streak`
/// that has been ≥ 3 for hours is not a backoff, it is a stuck task.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_sustained_rate_limit_backoff_is_reported_past_the_bound() {
    let db = create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let repo = TaskRepository::new(db.clone(), event_bus_for(&tx));

    let task = ready_task(&db, &repo, "Ladder-pinned task", "600 minutes").await;
    sqlx::query(
        "INSERT INTO dispatch_state (task_id, failure_streak, last_dispatched_role) \
         VALUES ($1, 7, 'worker')",
    )
    .bind(&task.id)
    .execute(db.pool())
    .await
    .unwrap();

    let section = stranded_section(&repo).await;
    let finding = finding_for(&section, &task.id)
        .expect("a task rate-limited for ten hours must be reported");

    let escalation = &finding["gate_escalation"];
    assert_eq!(
        escalation["overridden_gates"],
        serde_json::json!(["rate_limit_backoff"]),
        "the rate-limit ladder is the gate that was overridden"
    );
    assert_eq!(escalation["evidence"]["failure_streak"], 7);
    assert!(
        escalation["evidence"]["cooldown_until"].is_null(),
        "no cooldown was set; the evidence must not invent one"
    );
    assert!(
        escalation["summary"]
            .as_str()
            .unwrap_or_default()
            .contains("failure_streak=7"),
        "summary: {}",
        escalation["summary"]
    );

    let gate = &finding["dispatch_gate"];
    assert_eq!(gate["rate_limited"], true);
    assert_eq!(gate["breaker_open"], false);
    assert!(
        gate["reasons"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!(
                "rate_limit_backoff_sustained_past_bound"
            ))
    );
}

/// A `failure_streak` that has been tripped only briefly is still excluded.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_transient_rate_limit_backoff_is_still_excluded() {
    let db = create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let repo = TaskRepository::new(db.clone(), event_bus_for(&tx));

    let task = ready_task(&db, &repo, "Briefly rate-limited", "40 minutes").await;
    sqlx::query("INSERT INTO dispatch_state (task_id, failure_streak) VALUES ($1, 3)")
        .bind(&task.id)
        .execute(db.pool())
        .await
        .unwrap();

    let section = stranded_section(&repo).await;
    assert!(
        finding_for(&section, &task.id).is_none(),
        "a 40-minute rate-limit backoff is transient and must stay silent"
    );
}

/// A revoked owner credential is bounded for the same reason as the breaker: no
/// human chose it, no human owns it, and nobody is watching a per-task
/// credential status. It is an environmental condition that will not heal on
/// its own.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_sustained_revoked_owner_credential_is_reported_past_the_bound() {
    let db = create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let repo = TaskRepository::new(db.clone(), event_bus_for(&tx));

    let task = ready_task(&db, &repo, "Credential-dead task", "1440 minutes").await;
    let user_id = revoked_credential_user(&db).await;
    sqlx::query("UPDATE tasks SET created_by_user_id = $1 WHERE id = $2")
        .bind(&user_id)
        .bind(&task.id)
        .execute(db.pool())
        .await
        .unwrap();

    let section = stranded_section(&repo).await;
    let finding = finding_for(&section, &task.id)
        .expect("a task whose owner has been credential-dead for a day must be reported");

    let escalation = &finding["gate_escalation"];
    assert_eq!(
        escalation["overridden_gates"],
        serde_json::json!(["owner_credential"])
    );
    assert_eq!(escalation["evidence"]["has_owner_credential"], true);
    assert_eq!(finding["dispatch_gate"]["credential_available"], false);
    assert!(
        finding["dispatch_gate"]["reasons"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!(
                "owner_credential_revoked_sustained_past_bound"
            ))
    );
}

/// The credential gate still suppresses a freshly-revoked credential — an
/// operator rotating a key must not trip an alarm on the way through.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_recently_revoked_owner_credential_is_still_excluded() {
    let db = create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let repo = TaskRepository::new(db.clone(), event_bus_for(&tx));

    let task = ready_task(&db, &repo, "Key being rotated", "35 minutes").await;
    let user_id = revoked_credential_user(&db).await;
    sqlx::query("UPDATE tasks SET created_by_user_id = $1 WHERE id = $2")
        .bind(&user_id)
        .bind(&task.id)
        .execute(db.pool())
        .await
        .unwrap();

    let section = stranded_section(&repo).await;
    assert!(
        finding_for(&section, &task.id).is_none(),
        "a 35-minute-old credential revocation is a rotation, not an outage"
    );
}

// ── The gate that is deliberately NOT bounded ───────────────────────────────

/// A manual dispatch pause is the one human-authored gate in the list. It is
/// deliberately **unbounded**: a person decided this work should stop, that
/// decision has its own operator surface, and it is expected to outlive any
/// threshold. A release freeze that starts alarming after three hours trains
/// operators to ignore this check, which is how a monitor dies.
///
/// This test exists so that choice is a decision on the record rather than an
/// oversight — change it here, deliberately, or not at all.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_manual_dispatch_pause_is_never_escalated() {
    let db = create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let project = create_test_project(&db).await;
    let epic = create_test_epic(&db, &project.id).await;
    let repo = TaskRepository::new(db.clone(), event_bus_for(&tx));

    let task = repo
        .create_fixture_in_project(
            &project.id,
            Some(&epic.id),
            "Paused by an operator",
            "desc",
            "design",
            "task",
            1,
            "worker",
            Some("open"),
            None,
        )
        .await
        .unwrap();
    let session = create_test_session(&db, &project.id, &task.id).await;
    sqlx::query("UPDATE sessions SET status = 'paused' WHERE id = $1")
        .bind(&session.id)
        .execute(db.pool())
        .await
        .unwrap();
    // Ten times the bound.
    backdate_task_updated_at(&db, &task.id, "1800 minutes").await;

    let section = stranded_section(&repo).await;
    assert!(
        finding_for(&section, &task.id).is_none(),
        "a deliberate operator pause must stay silent indefinitely, at any age"
    );
}

// ── Invariants ──────────────────────────────────────────────────────────────

/// A task no gate is suppressing carries an explicit `null` escalation, not a
/// missing key and not a truthy stub.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_ungated_stranded_task_carries_a_null_escalation() {
    let db = create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let repo = TaskRepository::new(db.clone(), event_bus_for(&tx));

    let task = ready_task(&db, &repo, "Plainly stranded", "600 minutes").await;

    let section = stranded_section(&repo).await;
    let finding = finding_for(&section, &task.id).expect("a plainly stranded task is reported");
    assert!(
        finding["gate_escalation"].is_null(),
        "no gate was suppressing this task, so nothing was escalated: {}",
        finding["gate_escalation"]
    );
}

/// Escalation must not mutate anything. The section is a read-only observation
/// and the whole point is that it does not touch the dispatch state it reports.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn escalating_a_gate_mutates_nothing() {
    let db = create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let repo = TaskRepository::new(db.clone(), event_bus_for(&tx));

    let task = ready_task(&db, &repo, "Read-only check", "5760 minutes").await;
    arm_breaker_cooldown(&db, &task.id, "openai/gpt-5.6-terra", 30).await;

    let before: (String, i32, i64) = sqlx::query_as(
        "SELECT t.status, ds.failure_streak, \
                (SELECT COUNT(*) FROM activity_log)::BIGINT \
           FROM tasks t JOIN dispatch_state ds ON ds.task_id = t.id WHERE t.id = $1",
    )
    .bind(&task.id)
    .fetch_one(db.pool())
    .await
    .unwrap();

    // Confirm the escalation actually fired, so this is not a vacuous read-only
    // assertion over a code path that never ran.
    let section = stranded_section(&repo).await;
    assert_eq!(
        finding_for(&section, &task.id).expect("escalated")["gate_escalation"]["escalated"],
        true
    );
    stranded_section(&repo).await;

    let after: (String, i32, i64) = sqlx::query_as(
        "SELECT t.status, ds.failure_streak, \
                (SELECT COUNT(*) FROM activity_log)::BIGINT \
           FROM tasks t JOIN dispatch_state ds ON ds.task_id = t.id WHERE t.id = $1",
    )
    .bind(&task.id)
    .fetch_one(db.pool())
    .await
    .unwrap();

    assert_eq!(before, after, "the stranded-ready section must not mutate");
}

/// Seed a user whose only credential is revoked, with no org-shared fallback.
async fn revoked_credential_user(db: &Database) -> String {
    let user_id = uuid::Uuid::now_v7().to_string();
    sqlx::query("INSERT INTO users (id, github_id, github_login) VALUES ($1, $2, $3)")
        .bind(&user_id)
        .bind((uuid::Uuid::now_v7().as_u128() & 0x7fff_ffff_ffff) as i64)
        .bind(format!("user-{user_id}"))
        .execute(db.pool())
        .await
        .unwrap();
    let cred_id = uuid::Uuid::now_v7().to_string();
    sqlx::query(
        "INSERT INTO credentials \
         (id, provider_id, key_name, encrypted_value, owner_user_id, revoked_at) \
         VALUES ($1, 'anthropic', $2, '\\x00'::bytea, $3, '2025-01-01T00:00:00.000Z')",
    )
    .bind(&cred_id)
    .bind(format!("key-{cred_id}"))
    .bind(&user_id)
    .execute(db.pool())
    .await
    .unwrap();
    user_id
}
