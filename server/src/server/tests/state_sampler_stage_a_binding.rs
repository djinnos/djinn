//! Executable binding between the Stage A row of `docs/STATE_SAMPLER_AUDIT.md`
//! and the production identity gate that row documents (task `03im`).
//!
//! `validate_startup_row_resolves` (`djinn-coordinator`'s
//! `state_sampler_audit_tests`) checks that a startup row's cited paths exist,
//! that its documented symbols are defined, and that each link of its call
//! chain is a real call. It does not read the row's decision rule — which is
//! how commit `5df6e3425` inverted Stage A's rule (a NULL identity now
//! interrupts, a terminal ledger row interrupts too) while the row kept saying
//! "blank/null identity ... preserve" and the audit stayed green in 0.03 s.
//! That was a recurrence one commit after `cv5r` was created to stop it.
//!
//! So the Stage A rule is no longer only English. The row names its destructive
//! and preserving `StageAIdentity` variants as sets, and this test compares
//! those sets against the sets [`stage_a_identity_is_destructive`] actually
//! produces, over every variant the enum declares. Either side moving without
//! the other reddens this test.
//!
//! Stage C's half of the binding lives in `djinn-coordinator`
//! (`startup_audit_stage_c_admitted_set_matches_the_code`); it cannot live here
//! and Stage A's cannot live there, because `djinn-server` depends on
//! `djinn-coordinator` and `StageAIdentity` is a `djinn-server` type.

use std::collections::HashSet;

use crate::server::state::{StageAIdentity, stage_a_identity_is_destructive};

const AUDIT: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../docs/STATE_SAMPLER_AUDIT.md"
));

/// The Stage A gate's own source, read for the variant list rather than the
/// rule: a variant added to the enum must appear in the document too.
const STATE_MODULE: &str = include_str!("../state/mod.rs");

const STAGE_A_SAMPLER_ID: &str = "startup-stage-a-session-interrupt";
/// Column five of the nine-column schema: "positive exact-owner absence proof".
const ABSENCE_PROOF_COLUMN: usize = 4;

/// Exhaustive by construction: adding a variant stops this compiling.
fn name(identity: StageAIdentity) -> &'static str {
    match identity {
        StageAIdentity::Unresolved => "Unresolved",
        StageAIdentity::Null => "Null",
        StageAIdentity::Malformed => "Malformed",
        StageAIdentity::MissingLedger => "MissingLedger",
        StageAIdentity::UnrecognizedStatus => "UnrecognizedStatus",
        StageAIdentity::Terminal => "Terminal",
        StageAIdentity::NonTerminal => "NonTerminal",
    }
}

const EVERY_IDENTITY: [StageAIdentity; 7] = [
    StageAIdentity::Unresolved,
    StageAIdentity::Null,
    StageAIdentity::Malformed,
    StageAIdentity::MissingLedger,
    StageAIdentity::UnrecognizedStatus,
    StageAIdentity::Terminal,
    StageAIdentity::NonTerminal,
];

/// The variant names an `enum <name> { .. }` block declares in `source`.
fn declared_variants(source: &str, enum_name: &str) -> Vec<String> {
    let header = format!("enum {enum_name} {{");
    let start = source
        .find(&header)
        .unwrap_or_else(|| panic!("`{enum_name}` is not declared in the cited source"));
    let body = &source[start + header.len()..];
    let end = body
        .find("\n}")
        .unwrap_or_else(|| panic!("`{enum_name}` has no closing brace at file scope"));
    body[..end]
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with("//") && !line.starts_with('#'))
        .map(|line| {
            let variant = line.trim_end_matches(',');
            assert!(
                !variant.is_empty()
                    && variant
                        .chars()
                        .all(|character| character.is_alphanumeric() || character == '_'),
                "`{enum_name}` declares `{variant}`, which this contract cannot read as a plain \
                 unit variant"
            );
            variant.to_owned()
        })
        .collect()
}

/// The nine cells of the audit inventory row carrying `sampler_id`.
fn audit_row(sampler_id: &str) -> Vec<&'static str> {
    let inventory = AUDIT
        .split_once("## Inventory\n")
        .expect("audit has an inventory section")
        .1;
    let row = inventory
        .lines()
        .filter(|line| line.starts_with('|'))
        .find(|line| line.contains(&format!("`{sampler_id}`")))
        .unwrap_or_else(|| panic!("audit has no `{sampler_id}` row"));
    let cells: Vec<_> = row
        .trim()
        .strip_prefix('|')
        .and_then(|body| body.strip_suffix('|'))
        .expect("inventory row is a Markdown table row")
        .split('|')
        .map(str::trim)
        .collect();
    assert_eq!(
        cells.len(),
        9,
        "`{sampler_id}` must keep proposal 5mzy's nine-column schema"
    );
    cells
}

/// The `<key> = {A, B, C}` set an audit cell names, in the document's own words.
fn documented_variant_set(cell: &str, key: &str) -> HashSet<String> {
    let opening = format!("{key} = {{");
    let start = cell
        .find(&opening)
        .unwrap_or_else(|| panic!("audit cell names no `{key}` variant set: {cell}"));
    let body = &cell[start + opening.len()..];
    let end = body
        .find('}')
        .unwrap_or_else(|| panic!("`{key}` variant set is not closed in the audit cell"));
    let named: Vec<_> = body[..end]
        .split(',')
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .map(str::to_owned)
        .collect();
    assert!(!named.is_empty(), "`{key}` names no variant at all");
    let unique: HashSet<_> = named.iter().cloned().collect();
    assert_eq!(
        unique.len(),
        named.len(),
        "`{key}` names a variant twice: {named:?}"
    );
    unique
}

/// The identity set the document calls destructive must be exactly the set the
/// production gate authorizes, and the two documented sets together must cover
/// every variant the enum declares.
#[test]
fn startup_audit_stage_a_destructive_set_matches_the_code() {
    let declared: HashSet<_> = declared_variants(STATE_MODULE, "StageAIdentity")
        .into_iter()
        .collect();
    let enumerated: HashSet<_> = EVERY_IDENTITY
        .iter()
        .map(|identity| name(*identity).to_owned())
        .collect();
    assert_eq!(
        enumerated, declared,
        "this contract does not cover every `StageAIdentity` variant the code declares"
    );

    // The most favourable evidence Stage A can ever hold: the census proved
    // this exact run destructively gone, and the worker is not connected.
    let census_gone = HashSet::from(["run-gone"]);
    let destructive: HashSet<String> = EVERY_IDENTITY
        .iter()
        .filter(|identity| {
            stage_a_identity_is_destructive(Some("run-gone"), **identity, &census_gone, false)
        })
        .map(|identity| name(*identity).to_owned())
        .collect();
    let preserving: HashSet<String> = EVERY_IDENTITY
        .iter()
        .filter(|identity| {
            !stage_a_identity_is_destructive(Some("run-gone"), **identity, &census_gone, false)
        })
        .map(|identity| name(*identity).to_owned())
        .collect();

    let cell = audit_row(STAGE_A_SAMPLER_ID)[ABSENCE_PROOF_COLUMN];
    assert_eq!(
        documented_variant_set(cell, "stage_a_destructive"),
        destructive,
        "the Stage A audit row's destructive set does not match the identities \
         `stage_a_identity_is_destructive` interrupts on"
    );
    assert_eq!(
        documented_variant_set(cell, "stage_a_preserving"),
        preserving,
        "the Stage A audit row's preserving set does not match the identities \
         `stage_a_identity_is_destructive` preserves"
    );
}

/// The row's qualifiers are load-bearing prose, so they are checked too: a
/// connected session always preserves, and `NonTerminal` is destructive only
/// for the exact run the census proved gone.
#[test]
fn startup_audit_stage_a_qualifiers_hold() {
    let census_gone = HashSet::from(["run-gone"]);

    for identity in EVERY_IDENTITY {
        assert!(
            !stage_a_identity_is_destructive(Some("run-gone"), identity, &census_gone, true),
            "the Stage A row claims a connected session always preserves, but `{}` \
             was destructive with `connected == true`",
            name(identity)
        );
    }

    let empty: HashSet<&str> = HashSet::new();
    assert!(
        !stage_a_identity_is_destructive(
            Some("run-gone"),
            StageAIdentity::NonTerminal,
            &empty,
            false
        ),
        "`NonTerminal` must need a census `Gone` witness for its exact run"
    );
    assert!(
        !stage_a_identity_is_destructive(
            Some("run-other"),
            StageAIdentity::NonTerminal,
            &census_gone,
            false
        ),
        "`NonTerminal` must not borrow another run's `Gone` witness"
    );
    assert!(
        !stage_a_identity_is_destructive(
            Some("  "),
            StageAIdentity::NonTerminal,
            &census_gone,
            false
        ),
        "a blank identity supplies no exact owner to match against the census"
    );
    assert!(
        stage_a_identity_is_destructive(
            Some("run-gone"),
            StageAIdentity::NonTerminal,
            &census_gone,
            false
        ),
        "`NonTerminal` must interrupt when the census proved that exact run gone"
    );
}
