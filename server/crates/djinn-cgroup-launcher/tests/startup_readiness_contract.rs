//! Startup readiness contract, on a real kernel, with no fakes and no gate.
//!
//! Task grkq: the launcher's two hard startup preconditions — a delegated root
//! that really is a cgroup v2 tree, and a reachable `clone3(CLONE_INTO_CGROUP)`
//! — must fail LOUDLY and BEFORE the broker binds its control socket, rather
//! than surfacing later as an opaque per-command spawn error.
//!
//! The containment suite next door drives a `FakeCgroup`, so it can prove what
//! the launcher does with a readiness value but never whether the launcher can
//! *derive* a truthful one from an actual directory. That gap is what let a
//! non-cgroup "delegated root" ship. These tests close it using the real
//! `NativeCgroupFs`/`NativeClone3` against real filesystem objects, which needs
//! no privileges and therefore runs in the ordinary test lane.

use std::path::PathBuf;

use djinn_cgroup_launcher::{CGROUP2_SUPER_MAGIC, Error, NativeCgroupFs, NativeClone3};

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

/// The child-spawn seam is probed at startup. On an ordinary test host `clone3`
/// is reachable, so the preflight passes — and, critically, it does so WITHOUT
/// forking: a preflight that leaked a process would be worse than no preflight.
#[test]
fn the_clone3_preflight_passes_on_a_reachable_kernel_without_forking() {
    let before = std::process::id();
    NativeClone3::preflight().expect(
        "clone3(CLONE_INTO_CGROUP) must be reachable on the test host; if this fails the \
         sandbox running the tests denies clone3, which is exactly the condition the \
         preflight exists to report",
    );
    assert_eq!(
        std::process::id(),
        before,
        "the preflight must never fork; a returning child would corrupt the test process"
    );
}

/// A denied seam is reported as its own named error carrying the errno, so the
/// operator sees "clone3 is blocked" rather than a generic launch failure. This
/// is the shape a `seccompProfile: RuntimeDefault` sandbox produces (that
/// profile answers `clone3` with `ENOSYS`).
#[test]
fn a_denied_clone3_is_a_named_readiness_failure_carrying_the_errno() {
    let denied = Error::Clone3Unavailable {
        errno: libc::ENOSYS,
    };
    let message = denied.to_string();
    assert!(
        message.contains("clone3") && message.contains(&libc::ENOSYS.to_string()),
        "a denied child-spawn seam must name itself and its errno: {message}"
    );
    assert!(
        message.contains("child-spawn"),
        "the message must say why it is fatal: {message}"
    );
}
