use super::*;
use std::fs;
use std::path::Path;
use std::time::{Duration, SystemTime};

fn temp_base() -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().to_path_buf();
    (temp, path)
}

fn write_file(path: &Path, content: &[u8]) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

fn touch_at(path: &Path, time: SystemTime) {
    let filetime = filetime::FileTime::from_system_time(time);
    filetime::set_file_times(path, filetime, filetime).unwrap();
}

#[test]
fn inventory_finds_top_level_profile_roots() {
    let (_temp, base) = temp_base();
    let unit_path = base.join("debug/.fingerprint/libcrate-abc123");
    write_file(&unit_path.join("lib-libcrate.json"), b"{}");
    let older = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
    touch_at(&unit_path.join("lib-libcrate.json"), older);

    let inventory = inventory_fingerprint_units(&base).expect("inventory should succeed");
    assert_eq!(inventory.units.len(), 1);
    assert_eq!(inventory.units[0].unit_name, "libcrate-abc123");
    assert_eq!(inventory.units[0].profile_root, base.join("debug"));
    assert_eq!(inventory.units[0].projected_bytes, 2);
    assert_eq!(inventory.units[0].compiled_at_upper_bound, older);
}

#[test]
fn inventory_finds_target_triple_nested_profiles() {
    let (_temp, base) = temp_base();
    let unit_path = base.join("x86_64-unknown-linux-gnu/release/.fingerprint/foo-deadbeef");
    write_file(&unit_path.join("dep-graph.bin"), b"data");
    let older = SystemTime::UNIX_EPOCH + Duration::from_secs(2_000_000);
    touch_at(&unit_path.join("dep-graph.bin"), older);

    let inventory = inventory_fingerprint_units(&base).expect("inventory should succeed");
    assert_eq!(inventory.units.len(), 1);
    assert_eq!(inventory.units[0].unit_name, "foo-deadbeef");
    assert_eq!(
        inventory.units[0].profile_root,
        base.join("x86_64-unknown-linux-gnu/release")
    );
    assert_eq!(inventory.units[0].projected_bytes, 4);
}

#[test]
fn inventory_finds_all_profiles() {
    let (_temp, base) = temp_base();
    for profile in ["debug", "release", "test", "doc"] {
        let unit_path = base.join(format!("{}/.fingerprint/unit-{}", profile, profile));
        write_file(&unit_path.join("f.json"), b"x");
    }

    let inventory = inventory_fingerprint_units(&base).expect("inventory should succeed");
    let names: Vec<_> = inventory
        .units
        .iter()
        .map(|u| u.unit_name.clone())
        .collect();
    assert_eq!(
        names,
        vec!["unit-debug", "unit-doc", "unit-release", "unit-test"]
    );
}

#[test]
fn inventory_skips_profile_root_without_fingerprint() {
    let (_temp, base) = temp_base();
    fs::create_dir_all(base.join("debug/deps")).unwrap();
    write_file(&base.join("debug/deps/lib.o"), b"obj");

    let inventory = inventory_fingerprint_units(&base).expect("inventory should succeed");
    assert!(inventory.units.is_empty());
}

#[test]
fn inventory_is_deterministic_and_sorted() {
    let (_temp, base) = temp_base();
    let unit_b = base.join("release/.fingerprint/b-unit");
    let unit_a = base.join("debug/.fingerprint/a-unit");
    write_file(&unit_b.join("x.json"), b"x");
    write_file(&unit_a.join("x.json"), b"x");

    let inventory = inventory_fingerprint_units(&base).expect("inventory should succeed");
    let names: Vec<_> = inventory
        .units
        .iter()
        .map(|u| u.unit_name.clone())
        .collect();
    assert_eq!(names, vec!["a-unit", "b-unit"]);
}

#[test]
fn inventory_preserves_unknown_files() {
    let (_temp, base) = temp_base();
    write_file(&base.join(".rustc_info.json"), b"{}");
    write_file(&base.join("unknown"), b"?");
    let unit_path = base.join("debug/.fingerprint/u");
    write_file(&unit_path.join("x.json"), b"x");

    let inventory = inventory_fingerprint_units(&base).expect("inventory should succeed");
    assert_eq!(inventory.units.len(), 1);
    assert!(base.join(".rustc_info.json").exists());
    assert!(base.join("unknown").exists());
}

#[test]
fn inventory_fails_closed_on_missing_base() {
    let path = Path::new("/nonexistent/djinn/warm/base");
    let result = inventory_fingerprint_units(path);
    assert!(result.is_err());
}

#[test]
fn inventory_uses_newest_file_mtime() {
    let (_temp, base) = temp_base();
    let unit_path = base.join("debug/.fingerprint/older");
    write_file(&unit_path.join("a.json"), b"a");
    write_file(&unit_path.join("b.json"), b"bb");
    let older = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);
    let newer = SystemTime::UNIX_EPOCH + Duration::from_secs(2_000);
    touch_at(&unit_path.join("a.json"), older);
    touch_at(&unit_path.join("b.json"), newer);

    let inventory = inventory_fingerprint_units(&base).expect("inventory should succeed");
    assert_eq!(inventory.units[0].compiled_at_upper_bound, newer);
    assert_eq!(inventory.units[0].projected_bytes, 3);
}

#[test]
fn inventory_does_not_escape_base() {
    let (_temp, base) = temp_base();
    let sibling = base.parent().unwrap().join("sibling");
    fs::create_dir_all(&sibling).unwrap();
    // A path outside the base that must never be discovered.
    let _ = sibling.join(".fingerprint/outside");

    let inventory = inventory_fingerprint_units(&base).expect("inventory should succeed");
    assert!(inventory.units.is_empty());
}

#[test]
fn inventory_sums_bytes_recursively() {
    let (_temp, base) = temp_base();
    let unit_path = base.join("debug/.fingerprint/nested");
    write_file(&unit_path.join("a.json"), b"aaaa");
    write_file(&unit_path.join("sub/b.json"), b"bbb");

    let inventory = inventory_fingerprint_units(&base).expect("inventory should succeed");
    assert_eq!(inventory.units[0].projected_bytes, 7);
}

#[test]
fn inventory_no_side_effects_no_deletions() {
    let (_temp, base) = temp_base();
    let unit_path = base.join("debug/.fingerprint/unit");
    write_file(&unit_path.join("x.json"), b"x");

    let before = fs::read_dir(&base).unwrap().count();
    let _ = inventory_fingerprint_units(&base).expect("inventory should succeed");
    let after = fs::read_dir(&base).unwrap().count();

    assert_eq!(before, after);
    assert!(unit_path.exists());
}

#[test]
fn inventory_returns_empty_for_profile_root_without_fingerprint() {
    let (_temp, base) = temp_base();
    fs::create_dir_all(base.join("debug")).unwrap();

    let inventory = inventory_fingerprint_units(&base).expect("inventory should succeed");
    assert!(inventory.units.is_empty());
}

#[test]
fn inventory_reports_unit_path_relative_to_base() {
    let (_temp, base) = temp_base();
    let unit_path = base.join("debug/.fingerprint/xyz");
    write_file(&unit_path.join("x.json"), b"x");

    let inventory = inventory_fingerprint_units(&base).expect("inventory should succeed");
    assert_eq!(inventory.units[0].unit_path, unit_path);
}

#[test]
fn inventory_returns_multiple_units_from_same_profile() {
    let (_temp, base) = temp_base();
    for name in ["alpha", "beta", "gamma"] {
        let path = base.join(format!("debug/.fingerprint/{}", name));
        write_file(&path.join("x.json"), b"x");
    }

    let inventory = inventory_fingerprint_units(&base).expect("inventory should succeed");
    assert_eq!(inventory.units.len(), 3);
    let names: Vec<_> = inventory
        .units
        .iter()
        .map(|u| u.unit_name.clone())
        .collect();
    assert_eq!(names, vec!["alpha", "beta", "gamma"]);
}

#[test]
fn inventory_empty_base() {
    let (_temp, base) = temp_base();
    let inventory = inventory_fingerprint_units(&base).expect("inventory should succeed");
    assert!(inventory.units.is_empty());
}

#[test]
fn inventory_uses_non_last_use_timestamp_field_name() {
    // The public API deliberately avoids any language that implies last use
    // or deletability. This test only asserts the field names are conservative.
    let (_temp, base) = temp_base();
    let unit_path = base.join("debug/.fingerprint/old");
    write_file(&unit_path.join("x.json"), b"x");
    let old = SystemTime::UNIX_EPOCH + Duration::from_secs(1);
    touch_at(&unit_path.join("x.json"), old);

    let inventory = inventory_fingerprint_units(&base).expect("inventory should succeed");
    assert_eq!(inventory.units[0].compiled_at_upper_bound, old);
    // There is no `stale` or `last_used_at` field on FingerprintUnitEntry.
}

#[test]
fn inventory_finds_doc_profile() {
    let (_temp, base) = temp_base();
    let unit_path = base.join("doc/.fingerprint/doc-unit");
    write_file(&unit_path.join("x.json"), b"x");

    let inventory = inventory_fingerprint_units(&base).expect("inventory should succeed");
    assert_eq!(inventory.units[0].profile_root, base.join("doc"));
}

#[test]
fn inventory_ignores_files_in_fingerprint_root() {
    let (_temp, base) = temp_base();
    write_file(&base.join("debug/.fingerprint/random-file"), b"?");

    let inventory = inventory_fingerprint_units(&base).expect("inventory should succeed");
    assert!(inventory.units.is_empty());
}

#[test]
fn inventory_fails_closed_on_unit_without_files() {
    let (_temp, base) = temp_base();
    let unit_path = base.join("debug/.fingerprint/empty-unit");
    fs::create_dir_all(&unit_path).unwrap();

    let result = inventory_fingerprint_units(&base);
    assert!(result.is_err());
}

#[test]
fn inventory_uses_different_profile_root_paths() {
    let (_temp, base) = temp_base();
    for profile in ["debug", "release"] {
        let unit_path = base.join(format!(
            "x86_64-unknown-linux-gnu/{}/.fingerprint/p",
            profile
        ));
        write_file(&unit_path.join("x.json"), b"x");
    }

    let inventory = inventory_fingerprint_units(&base).expect("inventory should succeed");
    let roots: Vec<_> = inventory
        .units
        .iter()
        .map(|u| u.profile_root.clone())
        .collect();
    assert!(roots.contains(&base.join("x86_64-unknown-linux-gnu/debug")));
    assert!(roots.contains(&base.join("x86_64-unknown-linux-gnu/release")));
}

#[test]
fn inventory_preserves_unknown_top_level_files() {
    let (_temp, base) = temp_base();
    write_file(&base.join("CACHEDIR.TAG"), b"tag");
    write_file(&base.join("debug/.fingerprint/u/x.json"), b"x");

    let inventory = inventory_fingerprint_units(&base).expect("inventory should succeed");
    assert_eq!(inventory.units.len(), 1);
    assert!(base.join("CACHEDIR.TAG").exists());
}

#[test]
fn inventory_continues_when_one_profile_has_no_fingerprint() {
    let (_temp, base) = temp_base();
    fs::create_dir_all(base.join("release")).unwrap();
    let unit_path = base.join("debug/.fingerprint/u");
    write_file(&unit_path.join("x.json"), b"x");

    let inventory = inventory_fingerprint_units(&base).expect("inventory should succeed");
    assert_eq!(inventory.units.len(), 1);
}

#[test]
fn inventory_returns_projected_bytes_for_single_file() {
    let (_temp, base) = temp_base();
    let unit_path = base.join("release/.fingerprint/single");
    write_file(&unit_path.join("only.json"), b"12345");

    let inventory = inventory_fingerprint_units(&base).expect("inventory should succeed");
    assert_eq!(inventory.units[0].projected_bytes, 5);
}

#[cfg(unix)]
#[test]
fn inventory_fails_closed_when_fingerprint_dir_is_unreadable() {
    use std::os::unix::fs::PermissionsExt;

    let (_temp, base) = temp_base();
    let unit_path = base.join("debug/.fingerprint/unit");
    write_file(&unit_path.join("x.json"), b"x");

    // Remove all permissions from the profile root so that traversal into
    // `.fingerprint` fails with EACCES, while the profile root itself is
    // still stat-able (the exact fail-closed case from review feedback).
    let profile_root = base.join("debug");
    let mut perms = fs::metadata(&profile_root).unwrap().permissions();
    perms.set_mode(0o000);
    fs::set_permissions(&profile_root, perms).unwrap();

    let result = inventory_fingerprint_units(&base);

    let mut perms = fs::metadata(&profile_root).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&profile_root, perms).unwrap();

    assert!(
        result.is_err(),
        "expected unreadable .fingerprint to fail closed"
    );
    let err = result.unwrap_err();
    assert!(
        err.contains("failed to read metadata"),
        "unexpected error: {err}"
    );
}

#[cfg(unix)]
#[test]
fn inventory_fails_closed_when_nested_profile_root_is_unreadable() {
    use std::os::unix::fs::PermissionsExt;

    let (_temp, base) = temp_base();
    let unit_path = base.join("x86_64-unknown-linux-gnu/debug/.fingerprint/unit");
    write_file(&unit_path.join("x.json"), b"x");

    let target_root = base.join("x86_64-unknown-linux-gnu");
    let mut perms = fs::metadata(&target_root).unwrap().permissions();
    perms.set_mode(0o000);
    fs::set_permissions(&target_root, perms).unwrap();

    let result = inventory_fingerprint_units(&base);

    let mut perms = fs::metadata(&target_root).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&target_root, perms).unwrap();

    assert!(
        result.is_err(),
        "expected unreadable nested profile root to fail closed"
    );
    let err = result.unwrap_err();
    assert!(
        err.contains("failed to read metadata"),
        "unexpected error: {err}"
    );
}
