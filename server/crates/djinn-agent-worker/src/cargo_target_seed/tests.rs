//! Classification and clone-semantics coverage for the Cargo target seed.
//!
//! Foreign-owner / permission coverage lives in `foreign_owner_tests`.

use super::*;

#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use std::sync::{Barrier, MutexGuard};

const CARGO_TARGET_SEED_TOTAL: &str = "djinn_cargo_target_seed_total";

/// Emulate the kernel's `safe_hardlink_source()` decision for a process that is
/// in the source file's group but is NOT its owner, under
/// `fs.protected_hardlinks=1`.
///
/// Verified against a real 6.x kernel with a base owned by `10001:1000` and a
/// process running `1000:1000`: `0664`/`0775` link, while `0644`, `0755`,
/// `0444`, `0600` and symlinks all fail with EPERM.
#[cfg(unix)]
pub(super) fn source_is_linkable_by_foreign_owner(source: &Path) -> bool {
    let Ok(meta) = fs::symlink_metadata(source) else {
        return false;
    };
    let mode = meta.mode();
    // Special (non-regular) files are never pinnable; setuid and
    // setgid+group-executable files are refused outright; and the source must be
    // both readable and writable by the caller, which for a foreign owner in the
    // file's group means the GROUP bits.
    meta.file_type().is_file()
        && mode & libc::S_ISUID == 0
        && mode & (libc::S_ISGID | libc::S_IXGRP) != (libc::S_ISGID | libc::S_IXGRP)
        && mode & libc::S_IRGRP != 0
        && mode & libc::S_IWGRP != 0
}

#[cfg(not(unix))]
pub(super) fn source_is_linkable_by_foreign_owner(_source: &Path) -> bool {
    true
}

#[test]
fn classifies_heavy_artifacts_for_hardlink() {
    assert_eq!(
        classify_cargo_target_path(Path::new("debug/deps/libserde-abc.rlib")),
        CloneAction::Hardlink
    );
    // The compiled build script itself IS immutable — cargo replaces it by
    // rename, never in place — so it stays hardlinked. It is also the heavy part
    // of `build/`, which is why the carve-out below is scoped to `out/` and the
    // stamps rather than to all of `build/`.
    assert_eq!(
        classify_cargo_target_path(Path::new("release/build/ring-abc/build-script-build")),
        CloneAction::Hardlink
    );
}

#[test]
fn classifies_build_script_output_for_copy() {
    // A hardlinked OUT_DIR payload shares its inode with the shared warm base.
    // When a re-running build script rewrites it, `fs::copy` opens the
    // destination `O_CREAT|O_TRUNC` and only then `fchmod`s it, so the
    // truncation lands THROUGH the link into the base (9 zero-byte `nlink=2`
    // files were found there in production, all under `debug/build/*/out/`)
    // before the `fchmod` fails EPERM and `libssh2-sys` panics on its
    // `.unwrap()`. Copying gives the run a private inode, so the truncation can
    // no longer reach the base.
    for path in [
        "release/build/foo-abc/out/generated.rs",
        "debug/build/libssh2-sys-abc/out/libssh2.h",
        "debug/build/openssl-sys-def/out/openssl/include/opensslconf.h",
        "x86_64-unknown-linux-gnu/debug/build/foo-abc/out/generated.rs",
    ] {
        assert_eq!(
            classify_cargo_target_path(Path::new(path)),
            CloneAction::Copy,
            "{path} is build-script output and must be copied, not linked"
        );
    }
}

#[test]
fn classifies_build_script_stamps_for_copy() {
    // Cargo rewrites these in place after re-running a build script; a shared
    // inode writes through into the warm base every other pod seeds from.
    for name in ["output", "stderr", "root-output", "invoked.timestamp"] {
        assert_eq!(
            classify_cargo_target_path(Path::new(&format!("debug/build/foo-abc/{name}"))),
            CloneAction::Copy,
            "debug/build/foo-abc/{name} must be copied"
        );
    }
}

#[test]
fn build_script_matchers_do_not_oversweep() {
    // `out`/`output` only carry meaning at their cargo-defined position. A
    // dependency artifact that merely contains the word must keep its normal
    // classification, or the carve-out silently converts the heavy majority of
    // the base from hardlinks into byte copies.
    for path in [
        "debug/deps/out.rlib",
        "debug/deps/libout-abc.rlib",
        "debug/out/stray.bin",
        "debug/build/foo-abc/out.rs",
    ] {
        assert_eq!(
            classify_cargo_target_path(Path::new(path)),
            CloneAction::Hardlink,
            "{path} must not be swept into the build-script carve-out"
        );
    }
}

#[test]
fn classifies_fingerprint_and_dep_info_for_copy() {
    assert_eq!(
        classify_cargo_target_path(Path::new("debug/.fingerprint/foo-abc/lib-foo.json")),
        CloneAction::Copy
    );
    assert_eq!(
        classify_cargo_target_path(Path::new("debug/deps/foo.d")),
        CloneAction::Copy
    );
    assert_eq!(
        classify_cargo_target_path(Path::new("debug/build/foo/output.d")),
        CloneAction::Copy
    );
}

#[test]
fn classifies_build_directory_lock_for_skip() {
    // Cargo's per-profile lock files must never be hardlinked: flock attaches
    // to the inode, so a shared lock serializes `cargo` across every run and
    // the warm base via the shared PVC. All three variants emitted by the
    // pinned toolchain (`.cargo-lock`, `.cargo-build-lock`,
    // `.cargo-artifact-lock`) must be skipped in both `debug/` and `release/`.
    for name in [".cargo-lock", ".cargo-build-lock", ".cargo-artifact-lock"] {
        assert_eq!(
            classify_cargo_target_path(Path::new(&format!("debug/{name}"))),
            CloneAction::Skip,
            "debug/{name} must be skipped"
        );
        assert_eq!(
            classify_cargo_target_path(Path::new(&format!("release/{name}"))),
            CloneAction::Skip,
            "release/{name} must be skipped"
        );
        // Root-level and target-triple-nested variants are skipped by name.
        assert_eq!(
            classify_cargo_target_path(Path::new(name)),
            CloneAction::Skip,
            "root-level {name} must be skipped"
        );
        assert_eq!(
            classify_cargo_target_path(Path::new(&format!(
                "x86_64-unknown-linux-gnu/debug/{name}"
            ))),
            CloneAction::Skip,
            "target-triple-nested {name} must be skipped"
        );
    }
}

#[test]
fn cargo_lock_matcher_excludes_non_lock_cargo_metadata() {
    // The matcher is `.cargo` prefix + `lock` substring, so `.cargo` files
    // that are NOT locks (e.g. a hypothetical in-place-rewritten metadata
    // file) must fall through to their normal classification rather than
    // being wrongly skipped.
    assert_ne!(
        classify_cargo_target_path(Path::new("debug/.cargo-metadata.json")),
        CloneAction::Skip
    );
    assert_ne!(
        classify_cargo_target_path(Path::new(".rustc_info.json")),
        CloneAction::Skip
    );
}

#[test]
fn classifies_rustc_info_cache_for_copy() {
    // `.rustc_info.json` is rewritten in place by cargo, so a hardlink would
    // write through to the shared warm base. Copy gives each run a private
    // inode while inheriting the cached value.
    assert_eq!(
        classify_cargo_target_path(Path::new(".rustc_info.json")),
        CloneAction::Copy
    );
}

#[test]
fn classifies_incremental_for_skip_even_under_fingerprint() {
    assert_eq!(
        classify_cargo_target_path(Path::new("debug/incremental/foo/s-cache.bin")),
        CloneAction::Skip
    );
    assert_eq!(
        classify_cargo_target_path(Path::new("debug/.fingerprint/incremental/foo.d")),
        CloneAction::Skip
    );
}

#[test]
fn missing_base_returns_cold_start_and_prepares_run_dir() {
    let _guard = metric_test_guard();
    let tmp = tempfile::tempdir().expect("tempdir");
    let base = tmp.path().join("missing-base");
    let run = tmp.path().join("run-target");

    let result = seed_cargo_target_dir_with_options(&base, &run, &CargoTargetSeedOptions::new(2))
        .expect("missing base should not fail dispatch");

    assert_eq!(
        result.fallback_reason,
        Some(CargoTargetSeedFallback::BaseMissing)
    );
    assert!(result.cold_started());
    assert!(run.is_dir());
    assert_eq!(result.linked_file_count, 0);
    assert_eq!(result.copied_file_count, 0);
    assert_eq!(result.skipped_file_count, 0);
}

#[test]
fn task_variant_one_does_not_seed_sibling_variant_four() {
    let _guard = metric_test_guard();
    let tmp = tempfile::tempdir().expect("tempdir");
    let warm_root = tmp.path().join("cargo-target");
    let project_id = "project";
    let task_base = warm_base_dir_for_jobs_at_root(&warm_root, project_id, 1);
    let sibling_base = warm_base_dir_for_jobs_at_root(&warm_root, project_id, 4);
    let run = tmp.path().join("run-target");
    let sibling_artifact = Path::new("debug/deps/libsibling.rlib");
    write_base_file(&sibling_base, sibling_artifact, b"variant four only");

    let result =
        seed_cargo_target_dir_with_options(&task_base, &run, &CargoTargetSeedOptions::new(1))
            .expect("missing exact variant should cold-start");

    assert_eq!(
        result.fallback_reason,
        Some(CargoTargetSeedFallback::BaseMissing)
    );
    assert!(result.cold_started());
    assert!(
        !run.join(sibling_artifact).exists(),
        "a task for mold-jobs-1 must not seed mold-jobs-4 artifacts"
    );
}

#[test]
fn task_variant_one_reuses_only_its_matching_warm_base() {
    let _guard = metric_test_guard();
    let tmp = tempfile::tempdir().expect("tempdir");
    let warm_root = tmp.path().join("cargo-target");
    let project_id = "project";
    let task_base = warm_base_dir_for_jobs_at_root(&warm_root, project_id, 1);
    let sibling_base = warm_base_dir_for_jobs_at_root(&warm_root, project_id, 4);
    let additional_base = warm_base_dir_for_jobs_at_root(&warm_root, project_id, 8);
    let run = tmp.path().join("run-target");
    let matching_artifact = Path::new("debug/deps/libmatching.rlib");
    let sibling_artifact = Path::new("debug/deps/libsibling.rlib");

    assert_ne!(task_base, sibling_base);
    assert_ne!(task_base, additional_base);
    assert_ne!(sibling_base, additional_base);
    write_base_file(&sibling_base, sibling_artifact, b"variant four");
    write_base_file(&task_base, matching_artifact, b"variant one");

    let result =
        seed_cargo_target_dir_with_options(&task_base, &run, &CargoTargetSeedOptions::new(1))
            .expect("matching variant should seed");

    assert_eq!(result.fallback_reason, None);
    assert!(!result.cold_started());
    assert_eq!(
        fs::read(run.join(matching_artifact)).expect("read matching seeded artifact"),
        b"variant one"
    );
    assert!(
        !run.join(sibling_artifact).exists(),
        "a matching seed must not include sibling variant artifacts"
    );
}

#[test]
fn non_directory_base_returns_cold_start() {
    let _guard = metric_test_guard();
    let tmp = tempfile::tempdir().expect("tempdir");
    let base = tmp.path().join("base-file");
    let run = tmp.path().join("run-target");
    fs::write(&base, b"not a directory").expect("write base file");

    let result = seed_cargo_target_dir_with_options(&base, &run, &CargoTargetSeedOptions::new(2))
        .expect("non-directory base should not fail dispatch");

    assert_eq!(
        result.fallback_reason,
        Some(CargoTargetSeedFallback::BaseNotDirectory)
    );
    assert!(run.is_dir());
}

#[test]
fn seeds_target_tree_with_required_clone_semantics() {
    let _guard = metric_test_guard();
    let tmp = tempfile::tempdir().expect("tempdir");
    let base = tmp.path().join("warm-base");
    let run = tmp.path().join("run-target");

    let heavy = Path::new("debug/deps/libfoo.rlib");
    let fingerprint = Path::new("debug/.fingerprint/foo-abc/invoked.timestamp");
    let dep_info = Path::new("debug/deps/foo.d");
    let incremental = Path::new("debug/incremental/foo-abc/s-cache.bin");
    // Every cargo lock variant, in both profile dirs, must be skipped.
    let lock_files = [
        Path::new("debug/.cargo-lock"),
        Path::new("debug/.cargo-build-lock"),
        Path::new("debug/.cargo-artifact-lock"),
        Path::new("release/.cargo-lock"),
        Path::new("release/.cargo-build-lock"),
        Path::new("release/.cargo-artifact-lock"),
    ];
    let rustc_info = Path::new(".rustc_info.json");

    write_base_file(&base, heavy, b"large immutable artifact");
    write_base_file(&base, fingerprint, b"fingerprint metadata");
    write_base_file(&base, dep_info, b"dep-info metadata");
    write_base_file(&base, incremental, b"incremental state");
    for lock in lock_files {
        write_base_file(&base, lock, b"");
    }
    write_base_file(&base, rustc_info, b"rustc info cache");

    let result = seed_cargo_target_dir_with_options(&base, &run, &CargoTargetSeedOptions::new(4))
        .expect("seed target dir");

    assert_eq!(result.fallback_reason, None);
    assert_eq!(result.linked_file_count, 1);
    // fingerprint + dep-info + .rustc_info.json are byte-copied.
    assert_eq!(result.copied_file_count, 3);
    assert_eq!(result.degraded_link_file_count, 0);
    assert_eq!(result.unseeded_file_count, 0);
    assert_eq!(result.base_seedable_file_count, 4);
    assert!(
        result.skipped_file_count > lock_files.len() as u64,
        "incremental state and every cargo lock variant should be skipped"
    );

    assert_eq!(
        fs::read(run.join(heavy)).expect("read linked artifact"),
        b"large immutable artifact"
    );
    assert_eq!(
        fs::read(run.join(fingerprint)).expect("read copied fingerprint"),
        b"fingerprint metadata"
    );
    assert_eq!(
        fs::read(run.join(dep_info)).expect("read copied dep-info"),
        b"dep-info metadata"
    );
    assert!(
        !run.join(incremental).exists(),
        "incremental state must not be seeded into the private run dir"
    );
    assert!(
        !run.join("debug/incremental").exists(),
        "incremental directories must be skipped before descent"
    );
    for lock in lock_files {
        assert!(
            !run.join(lock).exists(),
            "cargo lock file {} must not be seeded into the private run dir",
            lock.display()
        );
    }
    assert_eq!(
        fs::read(run.join(rustc_info)).expect("read copied rustc info"),
        b"rustc info cache"
    );

    #[cfg(unix)]
    {
        assert_same_inode(&base.join(heavy), &run.join(heavy));
        assert_different_inode(&base.join(fingerprint), &run.join(fingerprint));
        assert_different_inode(&base.join(dep_info), &run.join(dep_info));
        // No seeded file may share an inode with ANY base file whose name
        // matches the cargo lock pattern: flock is inode-scoped, so a shared
        // inode would make this run's `cargo` serialize against the base and
        // every sibling. Each lock is skipped entirely, so it must simply be
        // absent from the run dir.
        let base_lock_inodes: std::collections::HashSet<(u64, u64)> = lock_files
            .iter()
            .map(|lock| {
                let meta = fs::metadata(base.join(lock)).expect("base lock metadata");
                (meta.dev(), meta.ino())
            })
            .collect();
        for entry in walk_files(&run) {
            let meta = fs::metadata(&entry).expect("run file metadata");
            assert!(
                !base_lock_inodes.contains(&(meta.dev(), meta.ino())),
                "seeded file {} shares an inode with a base cargo lock file",
                entry.display()
            );
        }
        // `.rustc_info.json` is copied, so the run owns a private inode and a
        // cargo rewrite cannot corrupt the shared warm base.
        assert_different_inode(&base.join(rustc_info), &run.join(rustc_info));
    }
}

#[test]
fn build_script_output_is_privately_owned_and_rewritable_without_touching_the_base() {
    // The verified production failure: a hardlinked OUT_DIR payload let a
    // re-running build script truncate the SHARED warm base through the link.
    // Nine zero-byte files were found in the base, every one under
    // `debug/build/*/out/` and every one `nlink=2` — cargo's fingerprints still
    // called those units fresh, so nothing regenerated them and the base stopped
    // re-converging.
    //
    // The inode assertions and the final base-contents assertion below are the
    // load-bearing ones: they fail deterministically under the old `Hardlink`
    // classification on any uid, whereas the EPERM half of the same syscall
    // sequence only reproduces when the base is genuinely foreign-owned (see
    // `foreign_owner_tests`).
    let _guard = metric_test_guard();
    let tmp = tempfile::tempdir().expect("tempdir");
    let base = tmp.path().join("warm-base");
    let run = tmp.path().join("run-target");

    let out_header = Path::new("debug/build/libssh2-sys-abc/out/libssh2.h");
    let out_nested = Path::new("debug/build/openssl-sys-def/out/openssl/include/opensslconf.h");
    let stamp = Path::new("debug/build/libssh2-sys-abc/output");
    let build_script = Path::new("debug/build/libssh2-sys-abc/build-script-build");

    write_base_file(&base, out_header, b"original vendored header");
    write_base_file(&base, out_nested, b"original vendored conf");
    write_base_file(&base, stamp, b"cargo:rustc-link-lib=ssh2");
    write_base_file(&base, build_script, b"build script binary");

    seed_cargo_target_dir_with_options(&base, &run, &CargoTargetSeedOptions::new(4))
        .expect("seed target dir");

    // Every rewritable build-script path must own a private inode...
    for path in [out_header, out_nested, stamp] {
        assert_different_inode(&base.join(path), &run.join(path));
    }
    // ...while the immutable build script itself stays hardlinked, so the
    // carve-out did not quietly convert the heavy part of `build/` into copies.
    let base_script = fs::metadata(base.join(build_script)).expect("base build script");
    let run_script = fs::metadata(run.join(build_script)).expect("run build script");
    assert_eq!(
        (base_script.dev(), base_script.ino()),
        (run_script.dev(), run_script.ino()),
        "the compiled build script is immutable and must still be hardlinked"
    );

    // Reproduce what a re-running `-sys` build script actually does: `fs::copy`
    // a vendored source over the seeded OUT_DIR path.
    let vendored = tmp.path().join("vendored-libssh2.h");
    fs::write(&vendored, b"regenerated vendored header").expect("write vendored source");
    fs::copy(&vendored, run.join(out_header)).expect(
        "a re-running build script must be able to fs::copy over its own seeded OUT_DIR payload",
    );

    assert_eq!(
        fs::read(run.join(out_header)).expect("read rewritten run payload"),
        b"regenerated vendored header"
    );
    // The shared warm base that every other task-run pod seeds from is untouched.
    assert_eq!(
        fs::read(base.join(out_header)).expect("read base payload"),
        b"original vendored header",
        "rewriting the run dir must not write through into the shared warm base"
    );
}

#[test]
fn copied_entries_present_the_same_age_as_hardlinked_ones() {
    // A hardlink carries the base's mtime for free because it IS the same inode.
    // `fs::copy` does not copy timestamps, so without an explicit restore every
    // `Copy`-classified entry would land with a SEED-time mtime while its
    // `Hardlink`-classified neighbours kept their WARM-time mtime. Cargo's
    // `StaleDependency` check is a raw mtime comparison, so that split makes
    // freshly-seeded metadata look newer than the artifacts it describes and
    // triggers broad, ordering-dependent spurious rebuilds — trading one failure
    // for another. Every seeded path must present the base's age, whichever arm
    // produced it.
    let _guard = metric_test_guard();
    let tmp = tempfile::tempdir().expect("tempdir");
    let base = tmp.path().join("warm-base");
    let run = tmp.path().join("run-target");

    let copied = Path::new("debug/build/libssh2-sys-abc/out/libssh2.h");
    let stamp = Path::new("debug/build/libssh2-sys-abc/output");
    let fingerprint = Path::new("debug/.fingerprint/foo-abc/lib-foo.json");
    let dep_info = Path::new("debug/deps/foo.d");
    let rustc_info = Path::new(".rustc_info.json");
    let linked = Path::new("debug/deps/libserde-abc.rlib");

    for path in [copied, stamp, fingerprint, dep_info, rustc_info, linked] {
        write_base_file(&base, path, b"warm artifact");
    }

    // Age the whole base well past any clock granularity the seed could
    // accidentally reproduce, so an unpreserved mtime cannot coincidentally pass.
    let warm_time = std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(1_600_000_000);
    for path in [copied, stamp, fingerprint, dep_info, rustc_info, linked] {
        let file = fs::OpenOptions::new()
            .write(true)
            .open(base.join(path))
            .expect("open base file to age it");
        file.set_times(fs::FileTimes::new().set_modified(warm_time))
            .expect("age base file");
    }

    seed_cargo_target_dir_with_options(&base, &run, &CargoTargetSeedOptions::new(4))
        .expect("seed target dir");

    for path in [copied, stamp, fingerprint, dep_info, rustc_info] {
        assert_eq!(
            classify_cargo_target_path(path),
            CloneAction::Copy,
            "{} must be a copy for this test to prove anything",
            path.display()
        );
        // NON-VACUITY: a copy really did produce a private inode, so the mtime
        // below is preserved rather than merely inherited from a shared one.
        assert_different_inode(&base.join(path), &run.join(path));
        assert_eq!(
            fs::metadata(run.join(path))
                .expect("seeded metadata")
                .modified()
                .expect("seeded mtime"),
            warm_time,
            "copied entry {} must present the base's mtime, not the seed's",
            path.display()
        );
    }

    // The hardlinked majority it has to stay consistent with.
    assert_eq!(
        fs::metadata(run.join(linked))
            .expect("linked metadata")
            .modified()
            .expect("linked mtime"),
        warm_time
    );
}

#[test]
fn incremental_prune_preserves_seedable_path_actions_and_contents() {
    let _guard = metric_test_guard();
    let tmp = tempfile::tempdir().expect("tempdir");
    let cache_root = tmp.path().join("cache");
    let base = cache_root.join("project");
    let before_run = tmp.path().join("before-run");
    let after_run = tmp.path().join("after-run");
    populate_prune_parity_fixture(&base);

    let before = seedable_snapshot(&base);
    assert_incremental_is_skipped(&base);
    let before_result =
        seed_cargo_target_dir_with_options(&base, &before_run, &CargoTargetSeedOptions::new(1))
            .expect("seed before prune");

    let prune = crate::cargo_incremental_prune::prune_fixture_incremental(&base, &cache_root)
        .expect("prune warm incremental fixture");
    assert_eq!(prune.outcome.as_str(), "pruned");
    assert!(!base.join("debug/incremental").exists());

    let after = seedable_snapshot(&base);
    let after_result =
        seed_cargo_target_dir_with_options(&base, &after_run, &CargoTargetSeedOptions::new(1))
            .expect("seed after prune");

    assert_eq!(
        before, after,
        "pruning may only remove skipped incremental state"
    );
    assert_eq!(
        seed_result_without_skips(&before_result),
        seed_result_without_skips(&after_result),
        "all Hardlink/Copy output counts and bytes must survive pruning"
    );
    assert_seeded_snapshot(&base, &before_run, &before);
    assert_seeded_snapshot(&base, &after_run, &after);
    assert_prune_kept_non_incremental_fixture(&base);
}

#[test]
fn concurrent_prune_and_seed_scan_never_selects_incremental_or_changes_candidates() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let cache_root = tmp.path().join("cache");
    let base = cache_root.join("project");
    populate_prune_parity_fixture(&base);
    let expected = seedable_snapshot(&base);
    let barrier = Arc::new(Barrier::new(2));
    let scan_base = base.clone();
    let scan_barrier = Arc::clone(&barrier);

    let scan = std::thread::spawn(move || {
        let entries = scan_entries(&scan_base).expect("concurrent seed scan");
        assert!(
            entries
                .iter()
                .filter(|entry| has_component(&entry.relative_path, "incremental"))
                .all(|entry| entry.action == CloneAction::Skip),
            "incremental entries observed by a seed scan must always be skipped"
        );
        scan_barrier.wait();
        seedable_snapshot_from_entries(&scan_base, entries)
    });

    barrier.wait();
    crate::cargo_incremental_prune::prune_fixture_incremental(&base, &cache_root)
        .expect("concurrent prune");
    let concurrent = scan.join().expect("seed scan thread");
    let after = seedable_snapshot(&base);

    assert_eq!(expected, concurrent);
    assert_eq!(expected, after);
    assert_prune_kept_non_incremental_fixture(&base);
}

#[test]
fn emits_fallback_metric_with_bounded_reason_label() {
    let _guard = metric_test_guard();
    let tmp = tempfile::tempdir().expect("tempdir");
    let base = tmp.path().join("missing-base");
    let run = tmp.path().join("run-target");
    let reason = djinn_telemetry::cargo_target_seed::FALLBACK_REASON_BASE_MISSING;
    let before = fallback_metric_value(reason);

    let result = seed_cargo_target_dir_with_options(&base, &run, &CargoTargetSeedOptions::new(2))
        .expect("missing base should not fail dispatch");

    assert_eq!(
        result.fallback_reason,
        Some(CargoTargetSeedFallback::BaseMissing)
    );
    let after = fallback_metric_value(reason);
    assert_eq!(
        after,
        before + 1.0,
        "fallback metric should increment exactly once for BaseMissing"
    );
}

#[test]
fn successful_seed_does_not_emit_fallback_metric() {
    let _guard = metric_test_guard();
    let tmp = tempfile::tempdir().expect("tempdir");
    let base = tmp.path().join("warm-base");
    let run = tmp.path().join("run-target");
    write_base_file(
        &base,
        Path::new("debug/deps/libsuccess.rlib"),
        b"seeded artifact",
    );
    let before = total_fallback_metric_value();

    let result = seed_cargo_target_dir_with_options(&base, &run, &CargoTargetSeedOptions::new(2))
        .expect("seed target dir");

    assert_eq!(result.fallback_reason, None);
    assert_eq!(total_fallback_metric_value(), before);
}

#[test]
fn teardown_run_dir_removes_private_dir_and_ignores_missing() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let run = tmp.path().join("run-target");
    fs::create_dir_all(run.join("debug/deps")).expect("create run tree");
    fs::write(run.join("debug/deps/libfoo.rlib"), b"artifact").expect("write run file");

    let removed = teardown_run_dir(&run).expect("remove run dir");
    assert_eq!(removed.outcome(), "removed");
    assert_eq!(removed.removed_count(), 1);
    assert!(!run.exists());

    let missing = teardown_run_dir(&run).expect("missing run dir should be non-fatal");
    assert_eq!(missing.outcome(), "already_absent");
    assert_eq!(missing.removed_count(), 0);
}

/// Recursively collect every regular file under `root` (test-only helper for
/// the inode-level lock-sharing assertion).
pub(super) fn walk_files(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(read) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in read.flatten() {
            let path = entry.path();
            let meta = fs::symlink_metadata(&path).expect("entry metadata");
            if meta.is_dir() {
                stack.push(path);
            } else if meta.is_file() {
                files.push(path);
            }
        }
    }
    files
}

pub(super) fn write_base_file(base: &Path, relative: &Path, contents: &[u8]) {
    let path = base.join(relative);
    fs::create_dir_all(path.parent().expect("relative path parent")).expect("create parent");
    fs::write(path, contents).expect("write base file");
}

fn populate_prune_parity_fixture(base: &Path) {
    write_base_file(
        base,
        Path::new("debug/deps/libalpha.rlib"),
        b"alpha artifact",
    );
    fs::hard_link(
        base.join("debug/deps/libalpha.rlib"),
        base.join("debug/deps/libalpha-alias.rlib"),
    )
    .expect("hardlink fixture artifact");
    write_base_file(base, Path::new("debug/deps/alpha.d"), b"alpha dep-info");
    write_base_file(
        base,
        Path::new("debug/.fingerprint/alpha-abc/invoked.timestamp"),
        b"fingerprint metadata",
    );
    write_base_file(base, Path::new(".rustc_info.json"), b"rustc metadata");
    write_base_file(
        base,
        Path::new("release/build/alpha/out/generated.rs"),
        b"unrelated hardlink candidate",
    );
    write_base_file(base, Path::new("unrelated/keep.txt"), b"unrelated subtree");
    write_base_file(
        base,
        Path::new("debug/incremental/alpha/session.bin"),
        b"disposable incremental state",
    );
    write_base_file(
        base,
        Path::new("debug/incremental/alpha/work-products.bin"),
        b"more disposable state",
    );
}

fn seedable_snapshot(base: &Path) -> Vec<(PathBuf, CloneAction, Vec<u8>)> {
    seedable_snapshot_from_entries(base, scan_entries(base).expect("scan seed fixture"))
}

fn seedable_snapshot_from_entries(
    base: &Path,
    entries: Vec<SeedEntry>,
) -> Vec<(PathBuf, CloneAction, Vec<u8>)> {
    let mut snapshot: Vec<_> = entries
        .into_iter()
        .filter(|entry| !entry.is_dir && entry.action != CloneAction::Skip)
        .map(|entry| {
            let contents = fs::read(base.join(&entry.relative_path)).expect("read seed candidate");
            (entry.relative_path, entry.action, contents)
        })
        .collect();
    snapshot.sort_by(|left, right| left.0.cmp(&right.0));
    snapshot
}

fn assert_incremental_is_skipped(base: &Path) {
    let entries = scan_entries(base).expect("scan incremental fixture");
    assert!(
        entries
            .iter()
            .filter(|entry| has_component(&entry.relative_path, "incremental"))
            .all(|entry| entry.action == CloneAction::Skip),
        "CARGO_INCREMENTAL=1 warm state must remain represented as skipped seed input"
    );
}

fn seed_result_without_skips(result: &CargoTargetSeedResult) -> (u64, u64, u64, u64) {
    (
        result.linked_file_count,
        result.copied_file_count,
        result.linked_bytes,
        result.copied_bytes,
    )
}

fn assert_seeded_snapshot(base: &Path, run: &Path, snapshot: &[(PathBuf, CloneAction, Vec<u8>)]) {
    for (relative, action, contents) in snapshot {
        assert_eq!(
            fs::read(run.join(relative)).expect("read seeded candidate"),
            *contents
        );
        #[cfg(unix)]
        match action {
            CloneAction::Hardlink => assert_same_inode(&base.join(relative), &run.join(relative)),
            CloneAction::Copy => assert_different_inode(&base.join(relative), &run.join(relative)),
            CloneAction::Skip => panic!("snapshot excludes skipped entries"),
        }
    }
}

fn assert_prune_kept_non_incremental_fixture(base: &Path) {
    assert_eq!(
        fs::read(base.join("debug/deps/libalpha.rlib")).expect("deps survives"),
        b"alpha artifact"
    );
    assert_eq!(
        fs::read(base.join("debug/.fingerprint/alpha-abc/invoked.timestamp"))
            .expect("fingerprint survives"),
        b"fingerprint metadata"
    );
    assert_eq!(
        fs::read(base.join("unrelated/keep.txt")).expect("unrelated subtree survives"),
        b"unrelated subtree"
    );
}

pub(super) fn metric_test_guard() -> MutexGuard<'static, ()> {
    crate::tests::seed_telemetry_guard()
}

fn total_fallback_metric_value() -> f64 {
    [
        djinn_telemetry::cargo_target_seed::FALLBACK_REASON_BASE_MISSING,
        djinn_telemetry::cargo_target_seed::FALLBACK_REASON_BASE_NOT_DIRECTORY,
        djinn_telemetry::cargo_target_seed::FALLBACK_REASON_BASE_UNUSABLE,
        djinn_telemetry::cargo_target_seed::FALLBACK_REASON_SCAN_FAILED,
        djinn_telemetry::cargo_target_seed::FALLBACK_REASON_CLONE_FAILED,
        djinn_telemetry::cargo_target_seed::FALLBACK_REASON_UNKNOWN,
    ]
    .into_iter()
    .map(fallback_metric_value)
    .sum()
}

pub(super) fn fallback_metric_value(reason: &str) -> f64 {
    djinn_telemetry::render()
        .expect("render telemetry")
        .lines()
        .find_map(|line| {
            let (sample, value) = line.rsplit_once(' ')?;
            if sample.starts_with(CARGO_TARGET_SEED_TOTAL)
                && sample.contains("outcome=\"fallback\"")
                && sample.contains(&format!("fallback_reason=\"{reason}\""))
            {
                value.parse::<f64>().ok()
            } else {
                None
            }
        })
        .unwrap_or(0.0)
}

#[cfg(unix)]
pub(super) fn assert_same_inode(left: &Path, right: &Path) {
    let left = fs::metadata(left).expect("left metadata");
    let right = fs::metadata(right).expect("right metadata");
    assert_eq!((left.dev(), left.ino()), (right.dev(), right.ino()));
}

#[cfg(unix)]
pub(super) fn assert_different_inode(left: &Path, right: &Path) {
    let left = fs::metadata(left).expect("left metadata");
    let right = fs::metadata(right).expect("right metadata");
    assert_ne!((left.dev(), left.ino()), (right.dev(), right.ino()));
}
