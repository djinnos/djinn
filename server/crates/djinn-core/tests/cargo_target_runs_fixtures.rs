//! Repository-owned, Linux-only fixture contract for cargo target-run trimming.
//!
//! Fixture JSON is input, not a snapshot: this harness only reads committed
//! expectations and never writes fixture data. Filesystem objects which Git
//! cannot preserve (hardlinks, sparse files, symlinks, timestamps, and removal
//! faults) are materialized from the declared scenario at test time.

#![cfg(unix)]

use djinn_core::cargo_target_runs::{
    CargoTargetRunsCaps, CargoTargetRunsInventoryError, Filesystem, inventory_cargo_target_runs,
    resolve_cargo_target_runs_caps, trim_cargo_target_runs, trim_cargo_target_runs_with_fs,
};
use serde::Deserialize;
use std::collections::HashSet;
use std::fs;
use std::io;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::path::Path;

const FIXTURES: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/cargo_target_runs"
);

#[derive(Debug, Deserialize)]
struct Fixture {
    scenario: String,
    #[serde(default)]
    max_dirs: Option<usize>,
    #[serde(default)]
    max_bytes: Option<u64>,
    #[serde(default)]
    active: Vec<String>,
    #[serde(default)]
    invalid: bool,
    #[serde(default)]
    removal_failure: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Expected {
    #[serde(default)]
    caps: Option<Caps>,
    #[serde(default)]
    invalid_max_dirs: Option<bool>,
    #[serde(default)]
    invalid_max_bytes: Option<bool>,
    #[serde(default)]
    deleted: Option<usize>,
    #[serde(default)]
    errors: Option<usize>,
    #[serde(default)]
    protected: Option<usize>,
    #[serde(default)]
    final_directory_count: Option<usize>,
    #[serde(default)]
    final_allocated_bytes: Option<u64>,
    #[serde(default)]
    outcome: Option<String>,
    #[serde(default)]
    survivors: Vec<String>,
    #[serde(default)]
    absent: Vec<String>,
    #[serde(default)]
    inventory: Option<InventoryExpected>,
    #[serde(default)]
    root_error: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Caps {
    max_dirs: usize,
    max_bytes: u64,
}

#[derive(Debug, Deserialize)]
struct InventoryExpected {
    allocated_bytes: u64,
    directories: usize,
    candidates: Vec<String>,
    protected: usize,
    errors: usize,
    top_level_non_directories: usize,
    sparse_logical_bytes: Option<u64>,
}

#[test]
fn cargo_target_runs_fixture_contract() {
    let root = Path::new(FIXTURES);
    let mut cases = fs::read_dir(root)
        .expect("committed fixture directory must exist")
        .map(|entry| entry.expect("fixture directory entry").path())
        .collect::<Vec<_>>();
    cases.sort();
    assert!(!cases.is_empty(), "fixture matrix must not be empty");

    for case in cases {
        let fixture: Fixture = read_json(&case.join("scenario.json"));
        let expected: Expected = read_json(&case.join("expected.json"));
        run_fixture(&case, &fixture, &expected);
    }
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> T {
    serde_json::from_str(
        &fs::read_to_string(path).unwrap_or_else(|error| {
            panic!("required committed fixture {}: {error}", path.display())
        }),
    )
    .unwrap_or_else(|error| panic!("valid fixture JSON {}: {error}", path.display()))
}

fn run_fixture(case: &Path, fixture: &Fixture, expected: &Expected) {
    if fixture.scenario == "caps" {
        let raw = if fixture.invalid { Some("1K") } else { None };
        let (caps, diagnostics) = resolve_cargo_target_runs_caps(raw, raw);
        let expected_caps = expected.caps.as_ref().expect("caps expectation");
        assert_eq!(caps.max_dirs, expected_caps.max_dirs, "{}", case.display());
        assert_eq!(
            caps.max_bytes,
            expected_caps.max_bytes,
            "{}",
            case.display()
        );
        assert_eq!(
            diagnostics.invalid_max_dirs,
            expected.invalid_max_dirs.unwrap_or(false)
        );
        assert_eq!(
            diagnostics.invalid_max_bytes,
            expected.invalid_max_bytes.unwrap_or(false)
        );
        return;
    }

    let temporary = tempfile::tempdir().expect("fixture temp root");
    let root = temporary.path();
    materialize(root, &fixture.scenario);
    if fixture.scenario == "root_failure" {
        let missing = root.join("missing-root");
        assert!(matches!(
            inventory_cargo_target_runs(&missing),
            Err(CargoTargetRunsInventoryError::RootRead(_))
        ));
        assert_eq!(expected.root_error.as_deref(), Some("root_read"));
        return;
    }

    if let Some(inventory_expected) = &expected.inventory {
        let inventory =
            inventory_cargo_target_runs(root).expect("Linux inventory capability is required");
        assert_eq!(
            inventory.top_level_directory_count,
            inventory_expected.directories
        );
        assert_eq!(
            inventory
                .candidates
                .iter()
                .map(|item| String::from_utf8_lossy(&item.name).into_owned())
                .collect::<Vec<_>>(),
            inventory_expected.candidates
        );
        assert_eq!(inventory.protected.len(), inventory_expected.protected);
        assert_eq!(inventory.errors.len(), inventory_expected.errors);
        assert_eq!(
            inventory.top_level_non_directory_count,
            inventory_expected.top_level_non_directories
        );
        if let Some(logical) = inventory_expected.sparse_logical_bytes {
            assert_eq!(
                fs::metadata(root.join("run/sparse")).unwrap().len(),
                logical
            );
            assert!(
                fs::symlink_metadata(root.join("run/sparse"))
                    .unwrap()
                    .blocks()
                    * 512
                    < logical
            );
        }
        // Independently lstat every entry and root-wide deduplicate inodes. This
        // verifies the fixture's exact allocated-byte postcondition without
        // reusing the core inventory implementation.
        assert_eq!(
            inventory.total_allocated_bytes,
            inventory_expected.allocated_bytes,
            "committed initial allocated bytes {}",
            case.display()
        );
        if inventory_expected.errors == 0 {
            assert_eq!(
                inventory.total_allocated_bytes,
                allocated_bytes_independently(root)
            );
        }
    }

    let caps = CargoTargetRunsCaps {
        max_dirs: fixture.max_dirs.expect("trim fixture max_dirs"),
        max_bytes: fixture.max_bytes.expect("trim fixture max_bytes"),
    };
    let active = fixture.active.iter().cloned().collect::<HashSet<_>>();
    let result = match fixture.removal_failure.as_deref() {
        Some(fail) => trim_cargo_target_runs_with_fs(root, &active, caps, &FailNamed(fail)),
        None => trim_cargo_target_runs(root, &active, caps),
    }
    .expect("Linux trim capability is required");
    if fixture.scenario == "unmeasurable" {
        let mut permissions = fs::metadata(root.join("unmeasurable"))
            .unwrap()
            .permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(root.join("unmeasurable"), permissions).unwrap();
    }
    assert_result(case, root, &result, expected);
}

fn assert_result(
    case: &Path,
    root: &Path,
    result: &djinn_core::cargo_target_runs::CargoTargetRunsTrimResult,
    expected: &Expected,
) {
    assert_eq!(
        result.deleted,
        expected.deleted.expect("deleted expectation"),
        "{}",
        case.display()
    );
    assert_eq!(
        result.errors,
        expected.errors.expect("errors expectation"),
        "{}",
        case.display()
    );
    assert_eq!(
        result.protected,
        expected.protected.expect("protected expectation"),
        "{}",
        case.display()
    );
    assert_eq!(
        result.final_top_level_directory_count,
        expected
            .final_directory_count
            .expect("directory expectation")
    );
    assert_eq!(
        result.outcome.as_str(),
        expected.outcome.as_deref().expect("outcome expectation")
    );
    assert_eq!(
        result.final_allocated_bytes,
        expected
            .final_allocated_bytes
            .expect("final allocated-byte expectation"),
        "committed exact final bytes {}",
        case.display()
    );
    if result.errors == 0 {
        assert_eq!(
            result.final_allocated_bytes,
            allocated_bytes_independently(root)
        );
    }
    for name in &expected.survivors {
        assert!(
            fs::symlink_metadata(root.join(name)).is_ok(),
            "expected survivor {name}"
        );
    }
    for name in &expected.absent {
        assert!(!root.join(name).exists(), "expected removal {name}");
    }
}

fn materialize(root: &Path, scenario: &str) {
    match scenario {
        "disabled" => {
            run(root, "a");
            run(root, "b");
        }
        // The root-failure assertion inventories a deliberately missing child
        // root, so this fixture needs no on-disk entries.
        "root_failure" => {}
        "allocated" => {
            run(root, "run");
            fs::File::create(root.join("run/sparse"))
                .unwrap()
                .set_len(8 * 1024 * 1024)
                .unwrap();
            fs::write(root.join("run/payload"), vec![7_u8; 4096]).unwrap();
            fs::hard_link(root.join("run/payload"), root.join("run/payload-link")).unwrap();
            symlink(root.join("run/payload"), root.join("run-link")).unwrap();
            fs::write(root.join("top-file"), b"top").unwrap();
        }
        "ties" => {
            for name in ["alpha", "bravo", "charlie"] {
                run(root, name);
                fs::File::open(root.join(name))
                    .unwrap()
                    .set_times(
                        fs::FileTimes::new()
                            .set_accessed(std::time::SystemTime::UNIX_EPOCH)
                            .set_modified(std::time::SystemTime::UNIX_EPOCH),
                    )
                    .unwrap();
            }
        }
        "oversized" => run(root, "only"),
        "protected" => {
            run(root, "active");
            fs::create_dir(root.join(" ")).unwrap();
            fs::write(root.join(" ").join("artifact"), vec![1_u8; 4096]).unwrap();
            symlink(root.join("active"), root.join("run-link")).unwrap();
        }
        "non_directory" => fs::write(root.join("top-file"), vec![1_u8; 4096]).unwrap(),
        "hardlink" => {
            run(root, "a");
            run(root, "b");
            fs::write(root.join("a/shared"), vec![1_u8; 4096]).unwrap();
            fs::hard_link(root.join("a/shared"), root.join("b/shared")).unwrap();
        }
        "removal_continue" => {
            run(root, "aaa-stuck");
            run(root, "bbb-free");
        }
        "unmeasurable" => {
            // Materialize an actual recursive read failure. This must fail closed
            // rather than treating the run as a removable candidate.
            run(root, "unmeasurable");
            run(root, "free");
            let mut permissions = fs::metadata(root.join("unmeasurable"))
                .unwrap()
                .permissions();
            permissions.set_mode(0o000);
            fs::set_permissions(root.join("unmeasurable"), permissions).unwrap();
        }
        other => panic!("unknown fixture scenario {other}"),
    }
}

fn run(root: &Path, name: &str) {
    fs::create_dir(root.join(name)).unwrap();
    fs::write(root.join(name).join("payload"), vec![1_u8; 4096]).unwrap();
}

fn allocated_bytes_independently(root: &Path) -> u64 {
    fn walk(path: &Path, seen: &mut HashSet<(u64, u64)>) -> u64 {
        let metadata = fs::symlink_metadata(path).unwrap();
        let own =
            u64::from(seen.insert((metadata.dev(), metadata.ino()))) * metadata.blocks() * 512;
        if metadata.is_dir() && !metadata.file_type().is_symlink() {
            own + fs::read_dir(path)
                .unwrap()
                .map(|entry| walk(&entry.unwrap().path(), seen))
                .sum::<u64>()
        } else {
            own
        }
    }
    walk(root, &mut HashSet::new())
}

struct FailNamed<'a>(&'a str);
impl Filesystem for FailNamed<'_> {
    fn symlink_metadata(&self, path: &Path) -> io::Result<fs::Metadata> {
        fs::symlink_metadata(path)
    }
    fn remove_dir_all(&self, path: &Path) -> io::Result<()> {
        if path.file_name().and_then(|name| name.to_str()) == Some(self.0) {
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "fixture removal failure",
            ))
        } else {
            fs::remove_dir_all(path)
        }
    }
}
