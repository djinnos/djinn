//! The handoff warning must read the REAL invocation (v1) authority.
//!
//! `publish_handoff_warning` is the only producer of the
//! `build admission handoff warning active` log line and of the
//! `djinn_build_admission_handoff_warning{reason}` gauge family. It used to build
//! its invocation-side input as `InvocationAuthorityObservation::default()` —
//! `enforcing: false`, hard-coded — as did every other production
//! `evaluate_handoff` call site. The only caller that supplied the real
//! projection was a `#[cfg(test)]` harness in `djinn-coordinator`.
//!
//! Because `warning_reason` classifies `ForwardOverlap`, `RollbackOverlap` and
//! `InvocationPrimary` as `stale_epoch` whenever the invocation authority is not
//! enforcing, a genuinely healthy cutover emitted a permanent, unclearable
//! `stale_epoch` — byte-identical, in both the log and the gauge, to a genuine
//! incomplete epoch or illegal mode combination. The runbook's stale-epoch
//! detector was pinned at 1 and could never read 0.
//!
//! # The trap these tests are shaped to avoid
//!
//! Every pre-existing test around this code stayed green under the defect: the
//! unit tests pass the observation in by hand (so they never exercise the
//! production wiring), and no test asserted the *absence* of a warning for a
//! healthy row driven through the production seam. Asserting "a warning fires in
//! ForwardOverlap" is exactly the assertion that let this ship. So the anchor
//! test here asserts `None` for a healthy row, and the stale case asserts that
//! the genuine signal is still distinguishable from it.

use super::build_admission_config_tests::{
    BUILD_ADMISSION_TELEMETRY_LOCK, admission, handoff_repository,
    state_for_admission_config_with_db,
};
use super::*;
use djinn_db::{
    AdmissionHandoffPhase, AdmissionHandoffRepository, AdmissionHandoffRow, V0Mode, V1Mode,
};
use djinn_supervisor::services::{InvocationLiftDecision, evaluate_invocation_lift};

/// The acknowledgements a phase requires to be *complete* at its epoch.
fn required_authorities(phase: AdmissionHandoffPhase) -> Vec<AdmissionHandoffAuthority> {
    match phase {
        AdmissionHandoffPhase::EmergencyPrimary => vec![AdmissionHandoffAuthority::Emergency],
        AdmissionHandoffPhase::ForwardOverlap | AdmissionHandoffPhase::RollbackOverlap => vec![
            AdmissionHandoffAuthority::Emergency,
            AdmissionHandoffAuthority::Invocation,
        ],
        AdmissionHandoffPhase::InvocationPrimary => vec![AdmissionHandoffAuthority::Invocation],
    }
}

async fn acknowledge_current_phase(repository: &AdmissionHandoffRepository) -> AdmissionHandoffRow {
    let row = repository.read().await.expect("read").expect("seeded row");
    for authority in required_authorities(row.phase) {
        repository
            .acknowledge(authority, row.epoch)
            .await
            .expect("acknowledge");
    }
    repository.read().await.expect("read").expect("row")
}

/// Drive the durable row to `phase` with exactly `v0`/`v1` recorded and every
/// acknowledgement that phase requires taken at the resulting epoch — i.e. a
/// *complete* row, through the real repository transitions only.
async fn arm(
    repository: &AdmissionHandoffRepository,
    phase: AdmissionHandoffPhase,
    v0: V0Mode,
    v1: V1Mode,
) -> AdmissionHandoffRow {
    loop {
        let row = acknowledge_current_phase(repository).await;
        if row.phase == phase {
            break;
        }
        let next = match row.phase {
            AdmissionHandoffPhase::EmergencyPrimary => AdmissionHandoffPhase::ForwardOverlap,
            AdmissionHandoffPhase::ForwardOverlap => AdmissionHandoffPhase::InvocationPrimary,
            AdmissionHandoffPhase::InvocationPrimary => AdmissionHandoffPhase::RollbackOverlap,
            AdmissionHandoffPhase::RollbackOverlap => AdmissionHandoffPhase::EmergencyPrimary,
        };
        repository
            .advance(row.epoch, next, &[])
            .await
            .expect("advance");
    }
    // Record the modes last: `set_modes_and_cap` bumps the epoch and clears both
    // acknowledgements, so they must be re-taken afterwards.
    let epoch = repository.read().await.expect("read").expect("row").epoch;
    repository
        .set_modes_and_cap(epoch, v0, v1, Some(3))
        .await
        .expect("set modes");
    acknowledge_current_phase(repository).await
}

/// The value of one `djinn_build_admission_handoff_warning{reason=...}` series
/// in the process-global registry `publish_handoff_warning` writes to.
fn warning_gauge(reason: &str) -> f64 {
    let rendered = djinn_telemetry::render().expect("render telemetry");
    let needle = format!("djinn_build_admission_handoff_warning{{reason=\"{reason}\"}}");
    rendered
        .lines()
        .find(|line| line.starts_with(&needle))
        .unwrap_or_else(|| panic!("missing sample {needle} in:\n{rendered}"))
        .rsplit_once(' ')
        .and_then(|(_, value)| value.parse::<f64>().ok())
        .unwrap_or_else(|| panic!("sample should end with a number for {needle}"))
}

fn enforce_config() -> BuildAdmissionConfig {
    BuildAdmissionConfig {
        mode: BuildAdmissionMode::Enforce,
        cap: 3,
        pod_limit: None,
    }
}

/// The regression that was missing: a *healthy* forward overlap warns about
/// nothing.
///
/// Production ran exactly this row — `forward_overlap`, v0 `Enforce`, v1
/// `Enforce`, both acknowledgements at the current epoch, task-run pods logging
/// `decision=Lift authority=Armed` — and the server warned `stale_epoch` every
/// five minutes for as long as the cutover lasted.
#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn healthy_forward_overlap_publishes_no_warning() {
    let _telemetry_guard = BUILD_ADMISSION_TELEMETRY_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    // The recorder must be installed BEFORE the gauge is written: without it,
    // `set_handoff_warning` writes into no recorder at all and every later read
    // returns the pre-registered zero, which would make the assertions below
    // pass for the wrong reason.
    djinn_telemetry::init().expect("telemetry initializes");
    let db = Database::open_in_memory().expect("test database");
    let state = state_for_admission_config_with_db(db, enforce_config());
    let repository = handoff_repository(&state);
    let row = arm(
        &repository,
        AdmissionHandoffPhase::ForwardOverlap,
        V0Mode::Enforce,
        V1Mode::Enforce,
    )
    .await;

    // Ground truth, from the launcher's own projection: this row DOES lift the
    // cgroup quota, so the invocation authority is genuinely enforcing.
    assert_eq!(
        evaluate_invocation_lift(Ok(Some(row.clone()))),
        InvocationLiftDecision::Lift,
        "the armed forward overlap must lift: {row:?}"
    );
    assert_eq!(
        admission(&state).mode(),
        BuildAdmissionMode::Enforce,
        "the emergency authority is enforcing too"
    );

    // Pin the detector at 1 first, so the assertions below prove the publish
    // actually CLEARED it rather than merely never having set it.
    djinn_telemetry::build_admission::set_handoff_warning(Some("stale_epoch"));
    assert_eq!(warning_gauge("stale_epoch"), 1.0);

    assert_eq!(
        state.publish_handoff_warning().await,
        None,
        "both authorities enforce a complete forward overlap: nothing is wrong"
    );
    // The runbook's detector must be able to read zero.
    assert_eq!(warning_gauge("stale_epoch"), 0.0);
    assert_eq!(warning_gauge("unexpected_overlap"), 0.0);
    assert_eq!(warning_gauge("epoch_unreadable"), 0.0);
}

/// A healthy rollback overlap is the same both-enforcing shape and likewise
/// warns about nothing.
#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn healthy_rollback_overlap_publishes_no_warning() {
    let _telemetry_guard = BUILD_ADMISSION_TELEMETRY_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    // The recorder must be installed BEFORE the gauge is written: without it,
    // `set_handoff_warning` writes into no recorder at all and every later read
    // returns the pre-registered zero, which would make the assertions below
    // pass for the wrong reason.
    djinn_telemetry::init().expect("telemetry initializes");
    let db = Database::open_in_memory().expect("test database");
    let state = state_for_admission_config_with_db(db, enforce_config());
    let repository = handoff_repository(&state);
    let row = arm(
        &repository,
        AdmissionHandoffPhase::RollbackOverlap,
        V0Mode::Enforce,
        V1Mode::Enforce,
    )
    .await;
    assert_eq!(
        evaluate_invocation_lift(Ok(Some(row))),
        InvocationLiftDecision::Lift
    );
    // Pin the detector at 1 first, so the assertions below prove the publish
    // actually CLEARED it rather than merely never having set it.
    djinn_telemetry::build_admission::set_handoff_warning(Some("stale_epoch"));
    assert_eq!(warning_gauge("stale_epoch"), 1.0);

    assert_eq!(state.publish_handoff_warning().await, None);
    assert_eq!(warning_gauge("stale_epoch"), 0.0);
}

/// The committed cutover: v1 alone enforces and the emergency authority has
/// been released by the production seam that observes it.
#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn committed_invocation_primary_publishes_no_warning() {
    let _telemetry_guard = BUILD_ADMISSION_TELEMETRY_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    // The recorder must be installed BEFORE the gauge is written: without it,
    // `set_handoff_warning` writes into no recorder at all and every later read
    // returns the pre-registered zero, which would make the assertions below
    // pass for the wrong reason.
    djinn_telemetry::init().expect("telemetry initializes");
    let db = Database::open_in_memory().expect("test database");
    let state = state_for_admission_config_with_db(db, enforce_config());
    let repository = handoff_repository(&state);
    let row = arm(
        &repository,
        AdmissionHandoffPhase::InvocationPrimary,
        V0Mode::Observe,
        V1Mode::Enforce,
    )
    .await;
    assert_eq!(
        evaluate_invocation_lift(Ok(Some(row))),
        InvocationLiftDecision::Lift
    );

    // The real seam the periodic leader loop ticks releases the emergency
    // authority on a committed invocation-primary row.
    let controller = admission(&state).clone();
    state.finalize_build_admission_handoff(&controller).await;
    assert_eq!(
        controller.mode(),
        BuildAdmissionMode::Off,
        "a committed invocation-primary row releases the emergency authority"
    );

    // Pin the detector at 1 first, so the assertions below prove the publish
    // actually CLEARED it rather than merely never having set it.
    djinn_telemetry::build_admission::set_handoff_warning(Some("stale_epoch"));
    assert_eq!(warning_gauge("stale_epoch"), 1.0);

    assert_eq!(
        state.publish_handoff_warning().await,
        None,
        "exactly one authority enforces the committed cutover: nothing is wrong"
    );
    assert_eq!(warning_gauge("stale_epoch"), 0.0);
    assert_eq!(warning_gauge("unexpected_overlap"), 0.0);
}

/// The genuine signal must survive: a forward overlap the invocation authority
/// has not acknowledged at the current epoch is still `stale_epoch`.
///
/// Without this, "no warning ever fires" would satisfy the tests above.
#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn unacknowledged_invocation_epoch_still_publishes_stale_epoch() {
    let _telemetry_guard = BUILD_ADMISSION_TELEMETRY_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    // The recorder must be installed BEFORE the gauge is written: without it,
    // `set_handoff_warning` writes into no recorder at all and every later read
    // returns the pre-registered zero, which would make the assertions below
    // pass for the wrong reason.
    djinn_telemetry::init().expect("telemetry initializes");
    let db = Database::open_in_memory().expect("test database");
    let state = state_for_admission_config_with_db(db, enforce_config());
    let repository = handoff_repository(&state);
    arm(
        &repository,
        AdmissionHandoffPhase::ForwardOverlap,
        V0Mode::Enforce,
        V1Mode::Enforce,
    )
    .await;

    // Re-record the same modes: the epoch advances and BOTH acknowledgements are
    // cleared. Take only the emergency one, leaving the invocation authority
    // behind the current epoch — the operator-visible shape of a v1 that has not
    // come back after an `epoch advance`.
    let epoch = repository.read().await.expect("read").expect("row").epoch;
    repository
        .set_modes_and_cap(epoch, V0Mode::Enforce, V1Mode::Enforce, Some(3))
        .await
        .expect("set modes");
    let row = repository.read().await.expect("read").expect("row");
    repository
        .acknowledge(AdmissionHandoffAuthority::Emergency, row.epoch)
        .await
        .expect("emergency ack");
    let row = repository.read().await.expect("read").expect("row");
    assert_eq!(row.phase, AdmissionHandoffPhase::ForwardOverlap);
    assert_eq!(
        row.invocation_ack_epoch, None,
        "the invocation authority is genuinely behind"
    );
    assert_eq!(
        evaluate_invocation_lift(Ok(Some(row))),
        InvocationLiftDecision::Unleased,
        "an incomplete epoch does not lift, so v1 is genuinely not enforcing"
    );

    assert_eq!(
        state.publish_handoff_warning().await,
        Some(HandoffWarningReason::StaleEpoch),
        "a genuinely incomplete epoch must still raise the stale-epoch alarm"
    );
    assert_eq!(warning_gauge("stale_epoch"), 1.0);
}
