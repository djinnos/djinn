//! Startup readiness contract, on a real kernel, with no fakes and no gate.
//!
//! Task grkq/7deu: the launcher's hard startup preconditions — a delegated root
//! it can establish, a capability set it can shed, and a spawn seam that really
//! places a child in the leaf — must fail LOUDLY and BEFORE the broker binds its
//! control socket, rather than surfacing later as an opaque per-command error.
//!
//! The containment suite next door drives a `FakeCgroup`, so it can prove what
//! the launcher does with a readiness value but never whether the launcher can
//! *derive* a truthful one from an actual directory. That gap is what let a
//! non-cgroup "delegated root" ship. These tests close it using the real
//! `NativeCgroupFs`/`Bootstrap`/`NativeCgroupSpawn` against real filesystem
//! objects, which needs no privileges and therefore runs in the ordinary test
//! lane.

use std::path::PathBuf;

use djinn_cgroup_launcher::{
    CGROUP2_SUPER_MAGIC, CommandSpec, Error, Invocation, NativeCgroupFs, NativeCgroupSpawn,
    SpawnIntoCgroup,
};

/// Per-test scratch directory under the crate's test tmpdir.
struct Scratch(PathBuf);

impl Scratch {
    fn new(label: &str) -> Self {
        let base = std::env::var_os("CARGO_TARGET_TMPDIR")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir)
            .join(format!("grkq-readiness-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).expect("create scratch dir");
        Self(base)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A plain directory — which is exactly what an `emptyDir` volume gives a
/// container — is refused by NAME, not as a bare `ENOENT` from groping for a
/// control file that was never going to be there.
#[test]
fn a_delegated_root_that_is_not_cgroup2_is_refused_by_name() {
    let scratch = Scratch::new("not-cgroupfs");

    let error = NativeCgroupFs::open(&scratch.0, 0)
        .err()
        .expect("a plain directory is not a delegated cgroup v2 root");

    match error {
        Error::DelegatedRootIsNotCgroupFs {
            path,
            actual,
            expected,
        } => {
            assert_eq!(expected, CGROUP2_SUPER_MAGIC);
            assert_ne!(actual, CGROUP2_SUPER_MAGIC);
            assert_eq!(path, scratch.0.display().to_string());
            // The message has to name the path an operator would go look at.
            let rendered = Error::DelegatedRootIsNotCgroupFs {
                path,
                actual,
                expected,
            }
            .to_string();
            assert!(
                rendered.contains("cgroup2"),
                "the failure must say what was expected: {rendered}"
            );
        }
        other => panic!("expected DelegatedRootIsNotCgroupFs, got: {other}"),
    }
}

/// The filesystem-type check runs BEFORE any control-file read, so a root that
/// is not cgroup2 can never be misreported as, say, a missing `cpu` delegation.
#[test]
fn the_filesystem_type_check_precedes_the_controller_check() {
    let scratch = Scratch::new("precedence");
    // Plant a plausible-looking `cgroup.subtree_control` in a NON-cgroup
    // directory. If the type check ran second, this would be read and the
    // launcher would report a controller/ownership problem instead of the
    // truth: nothing mounted a cgroup2 hierarchy here.
    std::fs::write(scratch.0.join("cgroup.subtree_control"), "cpu\n").expect("plant control file");

    assert!(
        matches!(
            NativeCgroupFs::open(&scratch.0, 0),
            Err(Error::DelegatedRootIsNotCgroupFs { .. })
        ),
        "a forged control file must not be able to disguise a non-cgroup2 root"
    );
}

/// The spawn seam verifies cgroup membership instead of assuming it, and a
/// child it cannot place never reaches `execve`.
///
/// This is the always-on, unprivileged half of the no-unthrottled-interval
/// proof: the privileged lane measures `cpu.stat` on a real leaf, and this
/// asserts that a failed placement is refused by name rather than producing a
/// running child whose CPU nothing governs. The shipped `clone3` seam could not
/// fail this way — it passed a flag that truncated to zero, so every child was
/// an ordinary fork that silently stayed in the launcher's own cgroup.
#[test]
fn a_child_that_cannot_be_placed_is_refused_by_name_and_never_execs() {
    let scratch = Scratch::new("placement");
    let raw = std::ffi::CString::new(scratch.0.to_string_lossy().as_bytes()).expect("path");
    let not_a_cgroup = unsafe {
        libc::open(
            raw.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    };
    assert!(not_a_cgroup >= 0, "open the scratch directory");

    let command = CommandSpec {
        // Would exit 0 instantly if it ever ran.
        program: "/bin/true".to_owned(),
        argv: vec![],
        cwd: "/workspace".to_owned(),
        environment: vec![],
    };
    let invocation = Invocation {
        id: "readiness".to_owned(),
        fence: 1,
    };
    let result = NativeCgroupSpawn.spawn_into_cgroup(not_a_cgroup, &invocation, &command);
    unsafe { libc::close(not_a_cgroup) };

    match result {
        Err(Error::CgroupPlacementFailed { errno, .. }) => assert_eq!(errno, libc::ENOENT),
        Err(other) => panic!("expected a named placement failure, got: {other}"),
        Ok(_) => panic!(
            "a child was started into a directory that is not a cgroup; its CPU would be \
             governed by nothing at all"
        ),
    }
}
