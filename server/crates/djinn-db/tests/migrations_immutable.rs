//! Enforce that committed Postgres migrations are canonically named with
//! strictly-increasing version prefixes. sqlx records a per-file checksum
//! in `_sqlx_migrations` on first apply and will refuse to restart if a
//! previously-applied file is later mutated — that runtime guarantee is
//! verified in integration tests that exercise the live Postgres server;
//! this unit test just pins the file-naming contract.

use std::fs;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

fn migrations_dir(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(name)
}

fn canonical_entries(dir: &Path) -> Vec<(u64, String)> {
    let mut out: Vec<(u64, String)> = Vec::new();
    for entry in fs::read_dir(dir).expect("migrations dir readable") {
        let entry = entry.unwrap();
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.ends_with(".sql") {
            continue;
        }
        let stem = name.trim_end_matches(".sql");
        let (version_str, _) = stem
            .split_once('_')
            .unwrap_or_else(|| panic!("migration `{name}` does not follow `{{N}}_{{slug}}.sql`"));
        let version: u64 = version_str
            .parse()
            .unwrap_or_else(|_| panic!("migration `{name}` has non-integer version prefix"));
        out.push((version, name));
    }
    out.sort();
    out
}

#[test]
fn postgres_migration_names_are_canonical_and_increasing() {
    let entries = canonical_entries(&migrations_dir("migrations_postgres"));
    assert!(
        !entries.is_empty(),
        "must have at least one postgres migration"
    );
    let mut last = 0_u64;
    for (v, _) in &entries {
        assert!(
            *v > last,
            "postgres migration versions must strictly increase; saw {v} after {last}"
        );
        last = *v;
    }
}

#[test]
fn postgres_migration_150_is_immutable() {
    assert_migration_unchanged(
        "150_service_preset_wrapper_identity.sql",
        "c341372f7b745f384175ef412d1d40a7c809b8194930cf16533eb638d49f3dc1",
    );
}

#[test]
fn postgres_migration_167_is_immutable() {
    assert_migration_unchanged(
        "167_launcher_authority_mode.sql",
        "137a03e948625947052b6aeed64dea53efa6a99184c30fd832815619e26edae7",
    );
}

/// 193 is pinned because `scripts/check-migrations-immutable.sh` structurally
/// cannot catch an edit to it from a branch in the `nafu` stack.
///
/// That guard diffs `origin/main...HEAD`, i.e. against the MERGE BASE. Wave 1
/// added 193 on its own branch and was squash-merged to main afterwards, so
/// every branch stacked on wave 1 forked from a commit where 193 did not yet
/// exist. The three-dot diff therefore classifies 193 as *Added* by the branch
/// (`--diff-filter=MRD` deliberately excludes `A`) and the guard reports OK
/// while the file is, in fact, being edited after main already had it. That is
/// exactly what happened: wave 5 grew 193 by +107/-1 and the guard stayed green.
///
/// A content hash does not care about the merge base, so it holds where the
/// diff-based guard cannot. Corrective DDL goes in a NEW migration --
/// `195_ci_route_lead_rejection_and_rollback_reports.sql` is where the wave-5
/// delta actually lives.
#[test]
fn postgres_migration_193_is_immutable() {
    assert_migration_unchanged(
        "193_ci_route_attempts.sql",
        "f8d980da51aabd745bc89f163109ba23ee1198a76d5ae8d2271290f4321d906c",
    );
}

/// Assert one migration file still hashes to the digest recorded when it
/// merged.
///
/// Factored out because the pins below say the same thing about four files and
/// the remediation instruction is the part worth stating once, carefully: sqlx
/// stores a per-file checksum in `_sqlx_migrations`, so a mutated file is not a
/// review nit — every database that already applied it refuses to boot.
fn assert_migration_unchanged(file: &str, expected: &str) {
    let bytes = fs::read(migrations_dir("migrations_postgres").join(file))
        .unwrap_or_else(|error| panic!("migration `{file}` readable: {error}"));
    assert!(
        !bytes.is_empty(),
        "vacuity: migration `{file}` is empty, so its digest asserts nothing"
    );
    let digest = format!("{:x}", Sha256::digest(&bytes));

    assert_eq!(
        digest, expected,
        "migration `{file}` was modified after it merged to main. sqlx stores \
         its checksum in `_sqlx_migrations` and every database that already \
         applied it will refuse to boot. Restore it byte-for-byte \
         (`git checkout origin/main -- \
         server/crates/djinn-db/migrations_postgres/{file}`) and express the \
         change as a new migration instead."
    );
}

/// 194 is pinned for the same reason 193 is, and it is not this proposal's.
///
/// `scripts/check-migrations-immutable.sh` diffs `origin/main...HEAD`, so a
/// migration that a branch forked *before* is classified as `Added` by that
/// branch and excluded by `--diff-filter=MRD`. Every migration added after a
/// long-lived branch's fork point is therefore editable from that branch with
/// the guard green. A content hash does not care about the merge base.
#[test]
fn postgres_migration_194_is_immutable() {
    assert_migration_unchanged(
        "194_liveness_evidence_trigger_identity.sql",
        "25bbe45b0d677dfb4ab155def5b58dd5c649a536a3a658fa7d984b754d861ada",
    );
}

/// 195 carries the `ci_route_attempts` wave-5 delta, and is the migration the
/// `nafu` stack is most likely to reach for.
///
/// It is where 193's corrective DDL was moved to *because* 193 had already
/// merged — which makes 195 the next file a wave is tempted to grow rather than
/// supersede. It also holds two `NULLS NOT DISTINCT` clauses (the evidence
/// identity index and the incomplete-hold identity index) whose deletion is
/// invisible to every sequential fixture; the behavioural half of that contract
/// is `ci_route_attempt_tests::one_pr_head_identity_holds_one_streak_across_concurrent_polls`,
/// and this pin is the half that says the clause may not be removed by editing
/// the applied migration.
///
/// Named `ci_route_attempts_…` deliberately: 195's DDL is `ALTER TABLE
/// ci_route_attempts` plus the two relations that table's routing contract
/// depends on, so the name is honest AND the pin is reachable from the
/// proposal's `cargo test -p djinn-db ci_route_attempt` filter. A migration
/// guard no acceptance command runs is a guard that fails after the fact.
#[test]
fn ci_route_attempts_migration_195_is_immutable() {
    assert_migration_unchanged(
        "195_ci_route_lead_rejection_and_rollback_reports.sql",
        "8aa11a5b1277a4fc41b4b35b7869f4be914c083b087a3016b377e5f701ff2ddb",
    );
}

/// 196 retires `process_terminated` from the `calling`-recovery quiescence
/// vocabulary.
///
/// Pinned because reinstating a value in an applied `CHECK` constraint by
/// editing this file is exactly the shape the branch-diff guard cannot see, and
/// because the value it removes is the one an operator under pressure would
/// most want back — it is the only spelling of "the process died" the schema
/// ever had, and the migration's own comment explains at length why nothing can
/// soundly produce it.
#[test]
fn postgres_migration_196_is_immutable() {
    assert_migration_unchanged(
        "196_ci_route_quiescence_proof_graceful_drain_only.sql",
        "1a4ef8d00cc4723c6ddc1e6d7e3bf6bcb39269252d8880c20e4a5ac1c55cc72b",
    );
}

/// 202 adds `ci_route_attempts.pr_merged_at` and backfills it from the merges
/// the old `adjudicated` reading could see.
///
/// Pinned for the reason 193/194/195 are — the branch-diff guard classifies a
/// migration added after a long-lived branch's fork point as `Added` and
/// `--diff-filter=MRD` skips it — and for one specific to this file: the
/// backfill is a one-shot `UPDATE`, so an edit to it is invisible on every
/// database that already ran it and silently different on every database that
/// has not. That is the failure mode where two deployments of the same commit
/// report different `merged_prs`, which is worse than either being wrong.
///
/// Named `ci_route_attempts_…` so the pin is reachable from the proposal's
/// `cargo test -p djinn-db ci_route_attempt` acceptance filter, matching 195.
#[test]
fn ci_route_attempts_migration_202_is_immutable() {
    assert_migration_unchanged(
        "202_ci_route_pr_merged_fact.sql",
        "8a691152174d3f53630867b38379ee8add861be831e917eb61c5fff29bdb639e",
    );
}
