//! Census of the cargo warm base — the convergence instrument.
//!
//! # The question this answers
//!
//! The warm cycle runs `cargo sweep --stamp`, compiles, then `cargo sweep
//! --file`, and `--file` deletes every artifact older than the stamp. When the
//! test-compile step is killed at its budget, the test binaries it did *not*
//! get around to touching are older than the stamp and are therefore deleted —
//! so the next cycle starts from less progress than the previous one finished
//! with. Whether the base converges is exactly the question of whether
//! `test_units` rises or falls cycle over cycle.
//!
//! Nothing here can answer that from a single sample; it is a per-cycle census
//! published either side of the compile phase so the trend is readable from
//! metrics and from the Pod log without shelling into the cluster.
//!
//! # What is counted
//!
//! * `fingerprint_units` — directories under `<base>/debug/.fingerprint`. One
//!   per cargo unit that has a recorded fingerprint.
//! * `test_units` — fingerprint units carrying a `test-*.json` fingerprint.
//!   Cargo names a unit's fingerprint file after its unit kind, so `test-lib-*`
//!   / `test-bin-*` is an exact identification of a `--test` unit. This is the
//!   artifact class the `--no-run` warm step exists to produce.
//! * `dep_files` — regular files under `<base>/debug/deps`, i.e. the linked
//!   artifacts themselves.
//!
//! Counting is best-effort and bounded: a missing base, an unreadable
//! directory, or a partially-written tree yields the counts observed so far
//! rather than an error, because a census must never be able to fail a warm.

use std::path::Path;

/// Counts for one warm-base sample.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WarmBaseCensus {
    pub fingerprint_units: u64,
    pub test_units: u64,
    pub dep_files: u64,
}

impl WarmBaseCensus {
    /// Whether the sample found anything at all. A base that has never been
    /// compiled reports all zeros, which is a legitimate observation and not an
    /// error.
    pub fn is_empty(&self) -> bool {
        self.fingerprint_units == 0 && self.test_units == 0 && self.dep_files == 0
    }
}

const FINGERPRINT_DIR: &str = "debug/.fingerprint";
const DEPS_DIR: &str = "debug/deps";
const TEST_FINGERPRINT_PREFIX: &str = "test-";
const FINGERPRINT_SUFFIX: &str = ".json";

/// Take a census of the warm base rooted at `base` (the `CARGO_TARGET_DIR` the
/// warm compiles into). Never fails: unreadable components contribute zero.
pub fn census(base: &Path) -> WarmBaseCensus {
    let mut out = WarmBaseCensus::default();
    count_fingerprint_units(&base.join(FINGERPRINT_DIR), &mut out);
    out.dep_files = count_regular_files(&base.join(DEPS_DIR));
    out
}

fn count_fingerprint_units(fingerprint_root: &Path, out: &mut WarmBaseCensus) {
    let Ok(entries) = std::fs::read_dir(fingerprint_root) else {
        return;
    };
    for entry in entries.flatten() {
        // `file_type` avoids a stat per entry on platforms that carry the type
        // in the directory entry; a symlinked unit dir is not a cargo unit.
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_dir() {
            continue;
        }
        out.fingerprint_units += 1;
        if unit_dir_has_test_fingerprint(&entry.path()) {
            out.test_units += 1;
        }
    }
}

fn unit_dir_has_test_fingerprint(unit_dir: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(unit_dir) else {
        return false;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if name.starts_with(TEST_FINGERPRINT_PREFIX) && name.ends_with(FINGERPRINT_SUFFIX) {
            return true;
        }
    }
    false
}

fn count_regular_files(dir: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut count = 0;
    for entry in entries.flatten() {
        match entry.file_type() {
            Ok(file_type) if file_type.is_file() => count += 1,
            _ => {}
        }
    }
    count
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, contents: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create fixture parent");
        }
        std::fs::write(path, contents).expect("write fixture");
    }

    #[test]
    fn counts_units_test_units_and_dep_files() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let base = tmp.path();

        // A library unit: lib fingerprint only.
        write(
            &base.join("debug/.fingerprint/djinn-core-aaaa/lib-djinn_core.json"),
            "{}",
        );
        // A unit compiled with --test: this is what the --no-run warm step
        // produces and what the convergence question is about.
        write(
            &base.join("debug/.fingerprint/djinn-core-bbbb/test-lib-djinn_core.json"),
            "{}",
        );
        write(
            &base.join("debug/.fingerprint/djinn-server-cccc/test-bin-djinn_server.json"),
            "{}",
        );
        // Linked artifacts.
        write(&base.join("debug/deps/libdjinn_core-aaaa.rlib"), "");
        write(&base.join("debug/deps/djinn_core-bbbb"), "");
        write(&base.join("debug/deps/djinn_core-bbbb.d"), "");

        let census = census(base);
        assert_eq!(census.fingerprint_units, 3);
        assert_eq!(census.test_units, 2);
        assert_eq!(census.dep_files, 3);
        assert!(!census.is_empty());
    }

    #[test]
    fn missing_base_is_an_all_zero_observation_not_an_error() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let census = census(&tmp.path().join("never-compiled"));
        assert_eq!(census, WarmBaseCensus::default());
        assert!(census.is_empty());
    }

    /// A base holding only non-test units must not be reported as holding test
    /// artifacts — otherwise a truncated `--no-run` step would look converged.
    #[test]
    fn units_without_a_test_fingerprint_are_not_counted_as_test_units() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let base = tmp.path();
        write(
            &base.join("debug/.fingerprint/serde-dddd/lib-serde.json"),
            "{}",
        );
        write(
            &base.join("debug/.fingerprint/serde-dddd/build-script-build-script-build.json"),
            "{}",
        );
        let census = census(base);
        assert_eq!(census.fingerprint_units, 1);
        assert_eq!(census.test_units, 0);
    }

    /// Files directly under `.fingerprint` are not units; only directories are.
    #[test]
    fn stray_files_under_the_fingerprint_root_are_not_units() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let base = tmp.path();
        write(&base.join("debug/.fingerprint/stray.json"), "{}");
        assert_eq!(census(base).fingerprint_units, 0);
    }
}
