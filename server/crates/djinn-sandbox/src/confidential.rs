// Confidential filesystem paths that repository-controlled code must never read.
//
// jqvg: the agent shell sandbox's Landlock policy is WRITE-confinement only —
// it grants `ReadFile` across the whole of `/`. The task-run Pod mounts the
// org's provider credentials and the projected ServiceAccount token into the
// same container that runs `bash -lc`, so any agent shell command, `build.rs`,
// test, Makefile target, or npm `postinstall` could read them.
//
// Landlock is purely additive within a ruleset layer: the kernel walks from the
// accessed dentry towards the mount root and any matching rule that grants the
// requested access satisfies the layer. A deeper, narrower rule therefore
// cannot subtract from a broader ancestor grant — you cannot carve an exception
// out of `PathBeneath("/", ReadFile)`.
//
// What you CAN do is never grant the ancestor in the first place. This module
// computes, mechanically from the confidential paths themselves, the set of
// paths that must be granted `ReadFile` so that the entire filesystem stays
// readable EXCEPT the confidential subtrees. For every strict ancestor of a
// confidential path, every sibling that is not itself on a confidential path is
// granted individually; the ancestors themselves are not. That is a denylist
// derived from a constant, not a hand-maintained read allowlist: nothing can be
// "missed" except by adding a new secret without adding it here, which the
// drift tests around `CONFIDENTIAL_ROOTS` guard.
//
// Directory listing (`ReadDir`) and `Execute` stay granted on `/` by the
// callers, so `ls /`, path traversal, and every toolchain binary keep working.
// Neither leaks file CONTENT: a filename is not the secret, and `Execute` on a
// non-executable blob is not reachable and would not surface its bytes to the
// caller anyway.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// Directories inside the task-run Pod whose *contents* must be unreadable to
/// any process the agent spawns.
///
/// These mirror the mount paths rendered by `djinn_k8s::job`; a drift test in
/// that crate asserts the two stay in step.
///
/// * `/var/run/djinn` — `djinn_k8s::job::SPEC_MOUNT_DIR`, the per-task-run
///   Secret volume. Carries `credentials.bin` (the org's resolved provider
///   credentials), `spec.bin`, `environment.json` and `service_metadata.json`.
///   The whole directory is excluded rather than just `credentials.bin`, so a
///   future key added to the same Secret is covered by construction.
/// * `/var/run/secrets` — the parent of `djinn_k8s::job::TOKEN_MOUNT_DIR`.
///   Covers both the djinn-audience projected ServiceAccount token
///   (`tokens/djinn`) and, on clusters that still automount it, the
///   kube-apiserver ServiceAccount token under `kubernetes.io/serviceaccount/`.
///   The two have very different blast radii — the djinn-audience token only
///   authenticates the worker to djinn-server for its own task-run, while the
///   automounted one is accepted by the apiserver — so the token half is also
///   addressed at the manifest: `job.rs` sets `automountServiceAccountToken:
///   false`, removing the apiserver-capable token from the Pod entirely.
///
/// A path that does not exist on the running host is skipped: there is nothing
/// to protect, and the cover degenerates to the historical blanket `/` grant.
pub const CONFIDENTIAL_ROOTS: &[&str] = &["/var/run/djinn", "/var/run/secrets"];

/// Resolve the confidential roots that actually exist, as canonical paths.
///
/// Canonicalization is load-bearing. On most modern images `/var/run` is a
/// symlink to `/run`, so `/var/run/djinn` and `/run/djinn` are the same inode.
/// Landlock matches on the resolved dentry rather than on the path string, so
/// the cover must be computed over canonical paths or the exclusion would sit
/// on the wrong branch of the tree and grant the secret back through the other
/// name.
///
/// Runs in the parent process before `fork`: it allocates and touches the
/// filesystem, neither of which is async-signal-safe.
pub fn present_confidential_roots(roots: &[&str]) -> Vec<PathBuf> {
    roots
        .iter()
        .filter_map(|raw| std::fs::canonicalize(raw).ok())
        .collect()
}

/// Compute the paths that must be granted `ReadFile` so the whole filesystem
/// stays readable except beneath `confidential_roots`.
///
/// Returns `["/"]` when there is nothing to exclude, which reproduces the
/// historical blanket grant exactly (dev hosts, the macOS path, any image that
/// does not mount the Pod secrets).
///
/// Fails closed: an ancestor directory that cannot be listed contributes no
/// grants rather than falling back to a blanket `/`, so a hostile or broken
/// filesystem cannot silently reopen the hole.
///
/// Runs in the parent process before `fork` — `read_dir` allocates.
pub fn read_file_cover(confidential_roots: &[PathBuf]) -> Vec<PathBuf> {
    let root = Path::new("/");

    // Drop anything that is not a usable exclusion, then drop entries that are
    // already inside another exclusion. Without that second pass a nested pair
    // such as `/run/secrets` + `/run/secrets/tokens` would make `/run/secrets`
    // an "ancestor", and its other children would be granted straight back.
    let mut candidates: Vec<PathBuf> = confidential_roots
        .iter()
        .filter(|path| path.is_absolute() && path.as_path() != root)
        .cloned()
        .collect();
    candidates.sort();
    candidates.dedup();
    let excluded: BTreeSet<PathBuf> = candidates
        .iter()
        .filter(|path| {
            !candidates
                .iter()
                .any(|other| other != *path && path.starts_with(other))
        })
        .cloned()
        .collect();

    if excluded.is_empty() {
        return vec![root.to_path_buf()];
    }

    // Every strict ancestor of an excluded path loses its blanket grant and is
    // re-covered entry by entry below. `/` is always in this set.
    let mut ancestors: BTreeSet<PathBuf> = BTreeSet::new();
    for path in &excluded {
        let mut current = path.parent();
        while let Some(dir) = current {
            ancestors.insert(dir.to_path_buf());
            current = dir.parent();
        }
    }

    let mut cover: Vec<PathBuf> = Vec::new();
    for dir in &ancestors {
        let Ok(entries) = std::fs::read_dir(dir) else {
            // Fail closed: grant nothing out of a directory we cannot enumerate.
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if ancestors.contains(&path) || excluded.contains(&path) {
                continue;
            }
            cover.push(path);
        }
    }
    cover.sort();
    cover.dedup();
    cover
}

/// Arm a Landlock layer on `cmd` that denies reading file CONTENT beneath the
/// confidential roots, and restricts **nothing else**.
///
/// This is for the command paths that are not under the full shell sandbox —
/// notably project setup hooks, which are operator-configured but invoke the
/// repository's own build tooling, so `build.rs`, an npm `postinstall` or a
/// Makefile target still executes repository-controlled code there. Applying
/// the full [`crate::linux::LandlockSandbox`] would additionally impose
/// write-confinement and could break a hook that legitimately writes outside
/// the worktree; this layer handles only `ReadFile`, so every other access
/// stays exactly as it was. Landlock layers intersect, so it also composes
/// safely with the full sandbox if both are ever applied.
///
/// No-op when Landlock is unavailable, or when no confidential root exists on
/// this host — both cases leave the command's behaviour byte-identical.
#[cfg(target_os = "linux")]
pub fn deny_confidential_reads(cmd: &mut std::process::Command) {
    deny_confidential_reads_beneath(cmd, &present_confidential_roots(CONFIDENTIAL_ROOTS));
}

/// [`deny_confidential_reads`] with an explicit root set, so tests can exercise
/// the real layer against a fixture tree.
#[cfg(target_os = "linux")]
pub(crate) fn deny_confidential_reads_beneath(
    cmd: &mut std::process::Command,
    confidential_roots: &[PathBuf],
) {
    use landlock::{AccessFs, PathBeneath, PathFd, Ruleset, RulesetAttr, RulesetCreatedAttr};
    use std::os::unix::process::CommandExt;

    if !crate::probe_landlock() {
        return;
    }
    // Computed in the parent: `read_dir` allocates and is not
    // async-signal-safe. `pre_exec` only opens the resulting paths.
    let cover = read_file_cover(confidential_roots);
    if cover.len() == 1 && cover[0] == Path::new("/") {
        // Nothing to protect on this host — do not arm a layer at all.
        return;
    }

    // Safety: `pre_exec` runs in the forked child. The closure only performs
    // Landlock syscalls and `open(2)` via `PathFd::new`, both async-signal-safe.
    unsafe {
        cmd.pre_exec(move || {
            let to_io =
                |e: std::io::Error| std::io::Error::new(std::io::ErrorKind::PermissionDenied, e);
            let mut ruleset = Ruleset::default()
                .handle_access(AccessFs::ReadFile)
                .map_err(|e| to_io(std::io::Error::other(e.to_string())))?
                .create()
                .map_err(|e| to_io(std::io::Error::other(e.to_string())))?;
            for path in &cover {
                if let Ok(fd) = PathFd::new(path) {
                    ruleset = ruleset
                        .add_rule(PathBeneath::new(fd, AccessFs::ReadFile))
                        .map_err(|e| to_io(std::io::Error::other(e.to_string())))?;
                }
            }
            ruleset
                .restrict_self()
                .map_err(|e| to_io(std::io::Error::other(e.to_string())))?;
            Ok(())
        });
    }
}

/// Non-Linux hosts have no Landlock; the task-run Pod is Linux-only, so this is
/// a no-op that keeps call sites free of `cfg` noise.
#[cfg(not(target_os = "linux"))]
pub fn deny_confidential_reads(_cmd: &mut std::process::Command) {}

#[cfg(test)]
mod tests {
    use super::*;

    /// `/var/run/djinn` and `/var/run/secrets` are the two mount points
    /// `djinn_k8s::job` renders into the worker container. If either constant
    /// moves, this crate must move with it.
    #[test]
    fn confidential_roots_cover_the_pod_secret_mounts() {
        // Mirrors `djinn_k8s::job::CREDENTIALS_MOUNT_FILE` and
        // `djinn_k8s::job::TOKEN_MOUNT_FILE`.
        for secret in [
            "/var/run/djinn/credentials.bin",
            "/var/run/djinn/spec.bin",
            "/var/run/secrets/tokens/djinn",
            "/var/run/secrets/kubernetes.io/serviceaccount/token",
        ] {
            assert!(
                CONFIDENTIAL_ROOTS
                    .iter()
                    .any(|root| Path::new(secret).starts_with(root)),
                "{secret} must sit beneath a confidential root"
            );
        }
    }

    #[test]
    fn empty_exclusions_reproduce_the_blanket_root_grant() {
        assert_eq!(read_file_cover(&[]), vec![PathBuf::from("/")]);
        // A bare `/` is not a usable exclusion and must not blank the cover.
        assert_eq!(
            read_file_cover(&[PathBuf::from("/")]),
            vec![PathBuf::from("/")]
        );
    }

    /// The cover must contain every sibling of every ancestor of the excluded
    /// path, and none of the ancestors themselves.
    #[test]
    fn cover_grants_siblings_and_withholds_ancestors() {
        let fixture = tempfile::tempdir_in("/var/tmp").expect("fixture root");
        let base = fixture
            .path()
            .canonicalize()
            .expect("canonical fixture root");
        let secret_dir = base.join("run/secret");
        let peer_dir = base.join("run/peer");
        std::fs::create_dir_all(&secret_dir).expect("secret dir");
        std::fs::create_dir_all(&peer_dir).expect("peer dir");
        let sibling_file = base.join("sibling.txt");
        std::fs::write(&sibling_file, "public").expect("sibling file");

        let cover = read_file_cover(std::slice::from_ref(&secret_dir));

        assert!(
            !cover.contains(&secret_dir),
            "excluded path must not appear"
        );
        assert!(
            cover.contains(&peer_dir),
            "sibling directory must be granted"
        );
        assert!(
            cover.contains(&sibling_file),
            "sibling file must be granted"
        );
        assert!(
            !cover.contains(&base.join("run")),
            "ancestor must not be granted as a subtree"
        );
        assert!(!cover.contains(&base), "ancestor must not be granted");
        assert!(
            !cover.contains(&PathBuf::from("/")),
            "the blanket root grant must be gone once anything is excluded"
        );
        // Unrelated top-level entries stay readable.
        assert!(cover.contains(&PathBuf::from("/etc")));
    }

    /// The standalone read-denial layer must block the secret and leave
    /// everything else — including writes outside the worktree, which the full
    /// shell sandbox would deny — completely alone.
    #[cfg(target_os = "linux")]
    #[test]
    fn read_denial_layer_blocks_the_secret_and_nothing_else() {
        if !crate::probe_landlock() {
            return;
        }
        // Outside every writable root the full sandbox grants, so no broader
        // rule can hand `ReadFile` back. See the matching note in `linux.rs`.
        let fixture = tempfile::tempdir_in(std::env::current_dir().expect("test directory"))
            .expect("fixture");
        let base = fixture.path().canonicalize().expect("canonical fixture");
        let secret_dir = base.join("var/run/djinn");
        std::fs::create_dir_all(&secret_dir).expect("secret dir");
        let secret = secret_dir.join("credentials.bin");
        std::fs::write(&secret, "LAYER-CANARY").expect("secret");
        let neighbour = base.join("var/run/neighbour.txt");
        std::fs::write(&neighbour, "NEIGHBOUR").expect("neighbour");
        let roots = vec![secret_dir, base.join("var/run/secrets")];

        let run = |script: &str, arg: &Path| {
            let mut cmd = std::process::Command::new("sh");
            cmd.args(["-c", script, "--"]).arg(arg);
            deny_confidential_reads_beneath(&mut cmd, &roots);
            cmd.output().expect("child should spawn")
        };

        let denied = run("cat \"$1\"", &secret);
        assert!(!denied.status.success(), "the layer must deny the secret");
        assert!(
            !String::from_utf8_lossy(&denied.stdout).contains("LAYER-CANARY"),
            "the canary leaked through the read-denial layer"
        );

        let allowed = run("cat \"$1\"", &neighbour);
        assert!(
            allowed.status.success(),
            "a neighbouring read must still work: {}",
            String::from_utf8_lossy(&allowed.stderr)
        );

        // The layer handles only `ReadFile`, so an ordinary write outside any
        // worktree — which the full sandbox would deny — must still succeed.
        let scratch = base.join("scratch.txt");
        assert!(
            run("printf ok > \"$1\"", &scratch).status.success(),
            "the read-denial layer must not impose write confinement"
        );
        assert!(scratch.exists());
    }

    /// A nested pair must collapse to the broader exclusion, otherwise the
    /// inner directory's siblings get granted back out of the outer one.
    #[test]
    fn nested_exclusions_collapse_to_the_broadest() {
        let fixture = tempfile::tempdir_in("/var/tmp").expect("fixture root");
        let base = fixture
            .path()
            .canonicalize()
            .expect("canonical fixture root");
        let outer = base.join("secrets");
        let inner = outer.join("tokens");
        let leaked = outer.join("other");
        std::fs::create_dir_all(&inner).expect("inner dir");
        std::fs::create_dir_all(&leaked).expect("peer inside outer");

        let cover = read_file_cover(&[inner, outer.clone()]);

        assert!(!cover.contains(&outer));
        assert!(
            !cover.contains(&leaked),
            "a sibling INSIDE the broader exclusion must stay excluded"
        );
    }
}
