//! Regression coverage for seeding a warm base owned by a DIFFERENT uid.
//!
//! The production failure this file exists for: the warm base is written by the
//! warm Job as uid 10001 and read by the task-run worker as uid 1000. With
//! `fs.protected_hardlinks=1` the kernel refuses `linkat()` with EPERM for any
//! source the caller does not own unless it is a group-writable regular file, so
//! a handful of `0644`/`0755`/`0444` entries and symlinks in a 38k-file base
//! made every hardlink attempt on those entries fail — and one failure aborted
//! the whole seed, discarding a 36G base on every task run.
//!
//! Two layers of coverage, because a test running as the base OWNER proves
//! nothing — that is exactly why the bug shipped:
//!
//! 1. [`LinkBackend::RefuseSourcesForeignOwnersCannotLink`] reproduces the
//!    kernel's `safe_hardlink_source()` decision deterministically on any uid,
//!    including unprivileged CI runners. These tests always run.
//! 2. [`real_foreign_uid_base_seeds_the_whole_base`] builds a base genuinely
//!    owned by another uid and runs the real `linkat` from a process that has
//!    dropped to the worker's uid/gid. That needs root to `chown`, so it runs
//!    only when the test process is root (a container, not a GitHub runner) and
//!    reports a skip otherwise.

use super::tests::{metric_test_guard, walk_files};
use super::*;

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;

/// uid that owns the warm base in production (the warm Job's user).
const BASE_UID: u32 = 10001;
/// gid shared by the warm base and the task-run worker.
const SHARED_GID: u32 = 1000;
/// uid the task-run worker runs as in production.
const WORKER_UID: u32 = 1000;

/// One base file: relative path, mode, contents.
struct BaseFile {
    relative: &'static str,
    mode: u32,
    contents: &'static [u8],
}

/// A warm base shaped like production.
///
/// The bulk is `0664` regular files and `0775` executables written by cargo and
/// rustc under `umask 002`, which the kernel WILL let a foreign owner hardlink.
/// The anomalies are the real-world minority that it will NOT: build-script
/// `OUT_DIR` payloads and registry sources keep their SOURCE mode when a build
/// script copies them, and the cargo registry ships `0644`/`0640`/`0600` files.
const BASE_FILES: &[BaseFile] = &[
    // Linkable majority.
    BaseFile {
        relative: "debug/deps/libserde-abc.rlib",
        mode: 0o664,
        contents: b"serde rlib",
    },
    BaseFile {
        relative: "debug/deps/libtokio-def.rlib",
        mode: 0o664,
        contents: b"tokio rlib",
    },
    BaseFile {
        relative: "debug/deps/djinn_server-1234",
        mode: 0o775,
        contents: b"test harness binary",
    },
    BaseFile {
        relative: "debug/build/ring-abc/build-script-build",
        mode: 0o775,
        contents: b"build script binary",
    },
    // Copied metadata (never hardlinked regardless of mode).
    BaseFile {
        relative: "debug/.fingerprint/serde-abc/lib-serde.json",
        mode: 0o664,
        contents: b"fingerprint json",
    },
    BaseFile {
        relative: "debug/deps/serde.d",
        mode: 0o664,
        contents: b"dep info",
    },
    // Anomalies the kernel refuses to hardlink for a foreign owner.
    BaseFile {
        relative: "debug/build/ring-abc/out/aes_nohw.o",
        mode: 0o644,
        contents: b"object copied from a registry source",
    },
    BaseFile {
        relative: "debug/build/zstd-sys-def/out/libzstd.a",
        mode: 0o444,
        contents: b"read-only vendored archive",
    },
    BaseFile {
        relative: "debug/build/protobuf-ghi/out/protoc",
        mode: 0o755,
        contents: b"vendored tool not group writable",
    },
];

/// Files the seed must place in the run dir, with the contents it must preserve.
fn expected_run_contents() -> BTreeMap<&'static str, &'static [u8]> {
    BASE_FILES
        .iter()
        .map(|file| (file.relative, file.contents))
        .collect()
}

/// Build a warm base with production mode bits. Directories are `2775` with the
/// setgid bit so children inherit the shared gid, exactly as the PVC contract
/// requires.
fn build_production_shaped_base(base: &Path) {
    fs::create_dir_all(base).expect("create base root");
    for file in BASE_FILES {
        let path = base.join(file.relative);
        fs::create_dir_all(path.parent().expect("parent")).expect("create parent dir");
        fs::write(&path, file.contents).expect("write base file");
        fs::set_permissions(&path, fs::Permissions::from_mode(file.mode))
            .expect("set base file mode");
    }
    // Cargo recreates its locks, and incremental state must never be shared.
    fs::write(base.join("debug/.cargo-lock"), b"").expect("write cargo lock");
    fs::create_dir_all(base.join("debug/incremental/foo")).expect("create incremental");
    fs::write(base.join("debug/incremental/foo/s-cache.bin"), b"state").expect("write incremental");

    for dir in walk_dirs(base) {
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o2775)).expect("set base dir mode");
    }
}

fn walk_dirs(root: &Path) -> Vec<PathBuf> {
    let mut dirs = vec![root.to_path_buf()];
    let mut index = 0;
    while index < dirs.len() {
        let Ok(read) = fs::read_dir(&dirs[index]) else {
            index += 1;
            continue;
        };
        for entry in read.flatten() {
            if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                dirs.push(entry.path());
            }
        }
        index += 1;
    }
    dirs
}

fn emulating_options(parallelism: usize) -> CargoTargetSeedOptions {
    CargoTargetSeedOptions::new(parallelism)
        .with_link_backend(LinkBackend::RefuseSourcesForeignOwnersCannotLink)
}

fn assert_every_expected_file_seeded(run: &Path) {
    for (relative, contents) in expected_run_contents() {
        let seeded = run.join(relative);
        assert!(
            seeded.exists(),
            "warm-base artifact {relative} is missing from the seeded run dir"
        );
        assert_eq!(
            fs::read(&seeded).expect("read seeded artifact"),
            contents,
            "seeded artifact {relative} has the wrong contents"
        );
    }
}

/// THE regression test. A base owned by a foreign uid with production mode bits
/// must seed COMPLETELY, not fall back to a cold build.
#[test]
fn foreign_owned_base_with_production_modes_seeds_completely() {
    let _guard = metric_test_guard();
    let tmp = tempfile::tempdir().expect("tempdir");
    let base = tmp.path().join("mold-jobs-4");
    let run = tmp.path().join("run-target");
    build_production_shaped_base(&base);

    let result = seed_cargo_target_dir_with_options(&base, &run, &emulating_options(4))
        .expect("seed must not fail dispatch");

    assert!(
        !result.cold_started(),
        "a base whose artifacts are individually seedable must never cold-start; \
         first entry error: {:?}",
        result.first_entry_error
    );
    assert_eq!(
        result.unseeded_file_count, 0,
        "every artifact is either linkable or copyable, so none may be dropped"
    );
    assert_eq!(
        result.base_seedable_file_count,
        BASE_FILES.len() as u64,
        "every non-skipped base file must be counted as seedable"
    );
    // The four group-writable artifacts hardlink; the three anomalies the kernel
    // refuses are byte-copied instead of discarded.
    assert_eq!(result.linked_file_count, 4);
    assert_eq!(result.degraded_link_file_count, 3);
    // `.fingerprint` and `.d` metadata are copied by classification, not by
    // degradation.
    assert_eq!(result.copied_file_count, 2);
    assert!(!result.link_fallback_budget_exhausted);
    assert!(result.degraded());
    assert_every_expected_file_seeded(&run);

    // Nothing that must stay private may be seeded.
    assert!(!run.join("debug/.cargo-lock").exists());
    assert!(!run.join("debug/incremental").exists());
}

/// A `0444` base artifact copied into the PRIVATE run dir must not stay
/// read-only: `fs::copy` reproduces the source mode, and Cargo rewrites that
/// path when it rebuilds the unit — a `0444` file rejects even its owner.
#[test]
fn degraded_copies_are_writable_by_the_run_that_owns_them() {
    let _guard = metric_test_guard();
    let tmp = tempfile::tempdir().expect("tempdir");
    let base = tmp.path().join("mold-jobs-4");
    let run = tmp.path().join("run-target");
    build_production_shaped_base(&base);

    seed_cargo_target_dir_with_options(&base, &run, &emulating_options(2)).expect("seed");

    let read_only_source = base.join("debug/build/zstd-sys-def/out/libzstd.a");
    assert_eq!(
        fs::metadata(&read_only_source)
            .expect("base metadata")
            .permissions()
            .mode()
            & 0o200,
        0,
        "the fixture's vendored archive must be read-only in the base"
    );

    let seeded = run.join("debug/build/zstd-sys-def/out/libzstd.a");
    let mode = fs::metadata(&seeded)
        .expect("seeded metadata")
        .permissions()
        .mode();
    assert_ne!(
        mode & 0o200,
        0,
        "seeded artifact 0{mode:o} must be rewritable by the run that owns it"
    );
    // Proof rather than inference: the run can actually rewrite it.
    fs::write(&seeded, b"rebuilt by cargo").expect("rewrite seeded artifact");
}

/// The pre-fix behaviour, stated as a property: a single entry the kernel
/// refuses must never cost the rest of the base.
#[test]
fn one_unseedable_entry_never_discards_the_base() {
    let _guard = metric_test_guard();
    let tmp = tempfile::tempdir().expect("tempdir");
    let base = tmp.path().join("mold-jobs-4");
    let run = tmp.path().join("run-target");
    build_production_shaped_base(&base);

    // A zero-byte budget forbids substituting a copy, so the refused entries are
    // genuinely unseedable — the harshest version of the production failure.
    let options = emulating_options(4).with_link_fallback_copy_budget_bytes(0);
    let result = seed_cargo_target_dir_with_options(&base, &run, &options).expect("seed");

    assert!(
        !result.cold_started(),
        "unseedable entries must degrade the seed, not abandon the base"
    );
    assert_eq!(result.linked_file_count, 4);
    assert_eq!(result.copied_file_count, 2);
    assert_eq!(result.degraded_link_file_count, 0);
    assert_eq!(result.unseeded_file_count, 3);
    assert!(result.link_fallback_budget_exhausted);
    assert!(result.degraded());

    // The linkable majority still landed. Cargo treats the three absent outputs
    // as dirty units and rebuilds only those.
    assert_eq!(
        fs::read(run.join("debug/deps/libserde-abc.rlib")).expect("linked artifact"),
        b"serde rlib"
    );
    assert!(!run.join("debug/build/protobuf-ghi/out/protoc").exists());
}

/// The diagnostic that would have ended this investigation in one log line.
#[test]
fn entry_error_names_the_operation_the_path_and_why_the_kernel_refused() {
    let _guard = metric_test_guard();
    let tmp = tempfile::tempdir().expect("tempdir");
    let base = tmp.path().join("mold-jobs-4");
    let run = tmp.path().join("run-target");
    build_production_shaped_base(&base);

    let options = emulating_options(1).with_link_fallback_copy_budget_bytes(0);
    let result = seed_cargo_target_dir_with_options(&base, &run, &options).expect("seed");

    let error = result
        .first_entry_error
        .expect("a refused hardlink must be reported");
    assert!(
        error.starts_with("hardlink "),
        "the failing OPERATION must lead the message: {error}"
    );
    assert!(
        ["out/aes_nohw.o", "out/libzstd.a", "out/protoc"]
            .iter()
            .any(|path| error.contains(path)),
        "the failing PATH must be named: {error}"
    );
    assert!(
        error.contains("os error 1"),
        "the errno must survive: {error}"
    );
    assert!(
        error.contains("source mode=0") && error.contains("process euid="),
        "the ownership facts that decide the kernel's answer must be present: {error}"
    );
    assert!(
        error.contains("fs.protected_hardlinks=1"),
        "an EPERM on hardlink must name the kernel rule that produced it: {error}"
    );
}

/// A base that yields nothing at all is the only case worth abandoning, and it
/// must still say which entry failed and why.
#[test]
fn base_that_yields_nothing_reports_a_cold_fallback_naming_the_entry() {
    let _guard = metric_test_guard();
    let tmp = tempfile::tempdir().expect("tempdir");
    let base = tmp.path().join("mold-jobs-4");
    let run = tmp.path().join("run-target");
    fs::create_dir_all(base.join("debug/deps")).expect("create base");
    let artifact = base.join("debug/deps/libonly-0644.rlib");
    fs::write(&artifact, b"unlinkable").expect("write artifact");
    fs::set_permissions(&artifact, fs::Permissions::from_mode(0o644)).expect("mode");

    let options = emulating_options(1).with_link_fallback_copy_budget_bytes(0);
    let result = seed_cargo_target_dir_with_options(&base, &run, &options).expect("seed");

    assert!(result.cold_started());
    let Some(CargoTargetSeedFallback::CloneFailed(reason)) = result.fallback_reason else {
        panic!("expected a clone failure, got {:?}", result.fallback_reason);
    };
    assert!(reason.contains("libonly-0644.rlib"), "reason: {reason}");
    assert!(reason.starts_with("hardlink "), "reason: {reason}");
    assert!(run.is_dir(), "the cold run dir must still be prepared");
}

/// The copy substituted for a refused hardlink is bounded, because an unbounded
/// one would duplicate a multi-tens-of-GiB base into per-run private disk.
#[test]
fn link_fallback_copy_is_bounded_by_a_byte_budget() {
    let _guard = metric_test_guard();
    let tmp = tempfile::tempdir().expect("tempdir");
    let base = tmp.path().join("mold-jobs-4");
    let run = tmp.path().join("run-target");
    fs::create_dir_all(base.join("debug/deps")).expect("create base");
    for index in 0..4 {
        let artifact = base.join(format!("debug/deps/libunlinkable-{index}.rlib"));
        fs::write(&artifact, vec![b'x'; 1024]).expect("write artifact");
        fs::set_permissions(&artifact, fs::Permissions::from_mode(0o644)).expect("mode");
    }

    // Room for exactly two of the four 1 KiB artifacts. Parallelism 1 keeps the
    // reservation order deterministic.
    let options = emulating_options(1).with_link_fallback_copy_budget_bytes(2048);
    let result = seed_cargo_target_dir_with_options(&base, &run, &options).expect("seed");

    assert!(!result.cold_started());
    assert_eq!(result.degraded_link_file_count, 2);
    assert_eq!(result.unseeded_file_count, 2);
    assert!(result.link_fallback_budget_exhausted);
    assert_eq!(result.copied_bytes, 2048);
}

/// A symlink must never reach `linkat`: it would pin the SYMLINK inode, which is
/// not `S_ISREG`, and the kernel refuses that for a foreign owner. It must also
/// never be descended into, or a self-referential link would cycle the scan.
#[test]
fn symlinks_are_materialised_privately_and_never_hardlinked() {
    let _guard = metric_test_guard();
    let tmp = tempfile::tempdir().expect("tempdir");
    let base = tmp.path().join("mold-jobs-4");
    let run = tmp.path().join("run-target");
    fs::create_dir_all(base.join("debug/build/foo-abc/out")).expect("create base");
    let target = base.join("debug/build/foo-abc/out/libnative.so.1.2.3");
    fs::write(&target, b"native library").expect("write link target");
    fs::set_permissions(&target, fs::Permissions::from_mode(0o664)).expect("mode");
    std::os::unix::fs::symlink(&target, base.join("debug/build/foo-abc/out/libnative.so"))
        .expect("symlink to a regular file");
    std::os::unix::fs::symlink(
        base.join("debug/build/foo-abc/out/missing"),
        base.join("debug/build/foo-abc/out/dangling.so"),
    )
    .expect("dangling symlink");
    // A symlinked directory that points at its own ancestor: descending would
    // never terminate.
    std::os::unix::fs::symlink(
        base.join("debug"),
        base.join("debug/build/foo-abc/out/loop"),
    )
    .expect("symlinked directory");

    let entries = scan_entries(&base).expect("scan must survive symlinks");
    let symlink_entry = entries
        .iter()
        .find(|entry| entry.relative_path.ends_with("libnative.so"))
        .expect("symlink entry");
    assert_eq!(
        symlink_entry.action,
        CloneAction::Copy,
        "a symlink must never be classified for hardlink"
    );

    let result = seed_cargo_target_dir_with_options(&base, &run, &emulating_options(2))
        .expect("seed must survive symlinks");

    assert!(!result.cold_started(), "{:?}", result.first_entry_error);
    assert_eq!(result.unseeded_file_count, 0);
    let seeded_link = run.join("debug/build/foo-abc/out/libnative.so");
    assert_eq!(
        fs::read(&seeded_link).expect("seeded symlink payload"),
        b"native library"
    );
    assert!(
        fs::symlink_metadata(&seeded_link)
            .expect("seeded metadata")
            .file_type()
            .is_file(),
        "the run dir must own a private regular file, not a link into the base"
    );
    assert!(!run.join("debug/build/foo-abc/out/dangling.so").exists());
    assert!(!run.join("debug/build/foo-abc/out/loop").exists());
}

/// Everything classified `Copy` is copied precisely so the run can rewrite it,
/// so a read-only base entry must not arrive unwritable.
#[test]
fn copied_entries_are_writable_even_when_the_base_entry_is_read_only() {
    let _guard = metric_test_guard();
    let tmp = tempfile::tempdir().expect("tempdir");
    let base = tmp.path().join("mold-jobs-4");
    let run = tmp.path().join("run-target");
    fs::create_dir_all(base.join("debug/build/vendored-abc/out")).expect("create base");
    let payload = base.join("debug/build/vendored-abc/out/libvendored.so.1");
    fs::write(&payload, b"vendored payload").expect("write payload");
    std::os::unix::fs::symlink(
        &payload,
        base.join("debug/build/vendored-abc/out/libvendored.so"),
    )
    .expect("symlink");
    let fingerprint = base.join("debug/.fingerprint/vendored-abc/lib-vendored.json");
    fs::create_dir_all(fingerprint.parent().expect("parent")).expect("create fingerprint dir");
    fs::write(&fingerprint, b"{}").expect("write fingerprint");
    fs::set_permissions(&payload, fs::Permissions::from_mode(0o444)).expect("read-only payload");
    fs::set_permissions(&fingerprint, fs::Permissions::from_mode(0o444))
        .expect("read-only fingerprint");

    seed_cargo_target_dir_with_options(&base, &run, &emulating_options(2)).expect("seed");

    for relative in [
        "debug/build/vendored-abc/out/libvendored.so",
        "debug/.fingerprint/vendored-abc/lib-vendored.json",
    ] {
        let seeded = run.join(relative);
        fs::write(&seeded, b"rewritten by cargo")
            .unwrap_or_else(|err| panic!("seeded {relative} must be rewritable: {err}"));
    }
}

/// The emulated backend is only trustworthy if it agrees with the kernel. This
/// asserts the rule it encodes against the modes measured on a real 6.x kernel
/// with a base owned by `10001:1000` and a process running `1000:1000`.
#[test]
fn emulated_refusal_matches_the_measured_kernel_rule() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let cases: &[(u32, bool)] = &[
        (0o664, true),
        (0o775, true),
        (0o644, false),
        (0o755, false),
        (0o444, false),
        (0o600, false),
    ];
    for (mode, linkable) in cases {
        let path = tmp.path().join(format!("mode-{mode:o}"));
        fs::write(&path, b"payload").expect("write");
        fs::set_permissions(&path, fs::Permissions::from_mode(*mode)).expect("mode");
        assert_eq!(
            super::tests::source_is_linkable_by_foreign_owner(&path),
            *linkable,
            "mode 0{mode:o} link expectation"
        );
    }
    let link = tmp.path().join("symlink");
    std::os::unix::fs::symlink(tmp.path().join("mode-664"), &link).expect("symlink");
    assert!(
        !super::tests::source_is_linkable_by_foreign_owner(&link),
        "a symlink is not a regular file and can never be hardlinked by a foreign owner"
    );
}

// ── real different-uid coverage (root only) ─────────────────────────────────

/// Run `body` in a forked child that has dropped to `uid`/`gid`, returning its
/// exit status.
fn run_dropped_to(uid: u32, gid: u32, body: impl FnOnce() -> i32) -> i32 {
    // SAFETY: `fork` from a test process that has not yet started a rayon pool
    // or any background thread; the child only calls async-signal-safe setup
    // before running `body` and then `_exit`s without unwinding.
    let pid = unsafe { libc::fork() };
    assert!(pid >= 0, "fork failed");
    if pid == 0 {
        let groups = [gid];
        // SAFETY: single-threaded child; all three calls are plain credential
        // changes on the calling process.
        let dropped = unsafe {
            libc::setgroups(1, groups.as_ptr()) == 0
                && libc::setgid(gid) == 0
                && libc::setuid(uid) == 0
        };
        let code = if dropped { body() } else { 90 };
        // SAFETY: terminate the forked child without running parent atexit
        // handlers or unwinding through the test harness.
        unsafe { libc::_exit(code) };
    }
    let mut status = 0;
    // SAFETY: reaping the child just forked above.
    unsafe { libc::waitpid(pid, &raw mut status, 0) };
    (status >> 8) & 0xff
}

fn chown_tree(root: &Path, uid: u32, gid: u32) {
    let mut paths = walk_dirs(root);
    paths.extend(walk_files(root));
    for path in paths {
        let raw = std::ffi::CString::new(path.as_os_str().as_encoded_bytes())
            .expect("path without interior NUL");
        // SAFETY: `raw` is a NUL-terminated path owned for the duration of the
        // call; `lchown` never follows the final symlink.
        let rc = unsafe { libc::lchown(raw.as_ptr(), uid, gid) };
        assert_eq!(rc, 0, "lchown {}", path.display());
    }
}

/// The genuine article: a base owned by uid 10001, seeded by a process running
/// as uid 1000, through the real `linkat` and the real kernel.
///
/// Requires root to `chown`, so it self-skips on an unprivileged CI runner. The
/// emulated tests above are the always-on coverage; this one exists so the
/// emulation can be validated against the kernel wherever root is available
/// (locally: `docker run --rm -v "$PWD":/w -w /w rust:… cargo test …`).
#[test]
#[allow(clippy::print_stderr)] // test diagnostic: a silent skip would hide missing coverage
fn real_foreign_uid_base_seeds_the_whole_base() {
    // SAFETY: reading this process's effective uid.
    if unsafe { libc::geteuid() } != 0 {
        eprintln!(
            "SKIP real_foreign_uid_base_seeds_the_whole_base: needs root to chown a base to \
             uid {BASE_UID}; the emulated LinkBackend tests cover the same rule unprivileged"
        );
        return;
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    let base = tmp.path().join("mold-jobs-4");
    let run = tmp.path().join("run-target");
    build_production_shaped_base(&base);
    fs::create_dir_all(&run).expect("create run dir");
    // chown clears setuid/setgid bits, so ownership must be applied BEFORE the
    // 2775 directory modes the PVC contract requires.
    chown_tree(&base, BASE_UID, SHARED_GID);
    for dir in walk_dirs(&base) {
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o2775)).expect("restore setgid dir");
    }
    for file in BASE_FILES {
        fs::set_permissions(
            base.join(file.relative),
            fs::Permissions::from_mode(file.mode),
        )
        .expect("restore file mode");
    }
    chown_tree(&run, WORKER_UID, SHARED_GID);
    // The dropped-privilege child reports back through a directory it owns; the
    // tempdir root itself stays root-owned and merely traversable.
    let handoff = tmp.path().join("handoff");
    fs::create_dir_all(&handoff).expect("create handoff dir");
    chown_tree(&handoff, WORKER_UID, SHARED_GID);
    fs::set_permissions(tmp.path(), fs::Permissions::from_mode(0o755)).expect("traversable tmp");

    let summary = handoff.join("summary.txt");
    let seed_base = base.clone();
    let seed_run = run.clone();
    let seed_summary = summary.clone();
    let status = run_dropped_to(WORKER_UID, SHARED_GID, move || {
        // Real `linkat`, real kernel, base owned by another uid.
        let options = CargoTargetSeedOptions::new(4);
        match seed_cargo_target_dir_with_options(&seed_base, &seed_run, &options) {
            Ok(result) => {
                let line = format!(
                    "cold={} linked={} degraded={} copied={} unseeded={} error={}",
                    result.cold_started(),
                    result.linked_file_count,
                    result.degraded_link_file_count,
                    result.copied_file_count,
                    result.unseeded_file_count,
                    result.first_entry_error.unwrap_or_default(),
                );
                if fs::write(&seed_summary, line).is_err() {
                    return 81;
                }
                0
            }
            Err(_) => 82,
        }
    });
    assert_eq!(status, 0, "seed child exited with {status}");

    let summary = fs::read_to_string(&summary).expect("seed summary");
    assert!(
        summary.contains("cold=false"),
        "a real foreign-owned base must not cold-start: {summary}"
    );
    assert!(
        summary.contains("unseeded=0"),
        "every artifact is linkable or copyable: {summary}"
    );
    assert!(
        summary.contains("linked=4"),
        "the group-writable majority must hardlink: {summary}"
    );
    assert!(
        summary.contains("degraded=3"),
        "the kernel must genuinely refuse the three non-group-writable entries, \
         proving the emulated backend models the real rule: {summary}"
    );
    assert_every_expected_file_seeded(&run);
}
