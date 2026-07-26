//! Writable render surface for the worker's durable invocation journal.
//!
//! # Why this module exists (launcher blocker 4, v0.7.8 rollback)
//!
//! In `required` mode the worker composes its shell path from an authenticated
//! broker connection, and that composition opens a pod-local write-ahead
//! journal before the supervisor ever runs:
//!
//! ```text
//! djinn-agent::context::ShellLaunchContext::broker_backed
//!   -> InvocationJournal::new(DJINN_INVOCATION_JOURNAL_DIR)
//!        -> std::fs::create_dir_all(directory)
//! ```
//!
//! The binary's compiled-in default for that directory is
//! `/var/run/djinn/invocation-journal`. `/var/run/djinn` in a task-run Pod is
//! the **`spec` Secret volume, mounted `readOnly: true`**, so `create_dir_all`
//! returns `EROFS` and the worker aborts before it creates a session. Measured
//! on the production node inside a real task-run worker container:
//!
//! ```text
//! $ mkdir -p /var/run/djinn/invocation-journal
//! mkdir: cannot create directory '/var/run/djinn/invocation-journal':
//!        Read-only file system
//! ```
//!
//! That is the whole of launcher blocker 4: the cgroup-launcher sidecar reached
//! `Ready` with zero restarts, the worker↔broker handshake completed (AUTH +
//! READY were both accepted on the node in a probe carrying the exact rendered
//! security context), and the worker then died one call later on a directory the
//! manifest never made writable. Every Job exhausted its retries with zero
//! sessions, and nothing lease-related was ever reached — which is exactly why
//! no lease error appeared anywhere.
//!
//! This is the same defect class as the renderer/binary contract gap that has
//! now produced several of these: **the binary requires something the manifest
//! renderer never supplies, and the tests pass because the test harness supplies
//! it** (`InvocationJournal` tests all pass a `tempfile::tempdir()`).
//!
//! # The fix
//!
//! Render a dedicated writable volume at [`INVOCATION_JOURNAL_DIR`] and pass
//! the path explicitly as `DJINN_INVOCATION_JOURNAL_DIR` rather than leaving the
//! worker to fall back to a compiled-in default the render does not know about.
//! Nesting a writable `emptyDir` under the read-only `spec` mount is sound and
//! is measured, not assumed — the launcher IPC volume already nests the same way
//! at `/var/run/djinn/launcher`, and a node probe confirmed a nested `emptyDir`
//! is writable while its read-only parent is not:
//!
//! ```text
//! touch /var/run/djinn/launcher/probe-write            -> OK
//! mkdir -p /var/run/djinn/invocation-journal           -> EROFS
//! mkdir -p /var/run/djinn/invocation-journal-mounted/x -> OK   (emptyDir)
//! ```
//!
//! The journal is rendered ONLY when the launcher is armed. In the explicit
//! `disabled` profile `shell_launch` is `None`, `broker_backed` is never called,
//! and no journal is opened — so the disabled manifest keeps its exact shape.

use k8s_openapi::api::core::v1::{EmptyDirVolumeSource, EnvVar, Volume, VolumeMount};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;

/// Volume name for the worker-private invocation journal.
pub const VOLUME_INVOCATION_JOURNAL: &str = "invocation-journal";

/// Directory the worker opens its durable invocation journal in.
///
/// MUST equal the `djinn-agent` default in
/// `ShellLaunchContext::broker_backed`, because that default is what a worker
/// falls back to when `DJINN_INVOCATION_JOURNAL_DIR` is absent. Keeping the two
/// equal means an older worker image paired with a newer render still lands on
/// the mount rather than on the read-only Secret volume.
pub const INVOCATION_JOURNAL_DIR: &str = "/var/run/djinn/invocation-journal";

/// Env var the worker reads the journal directory from.
pub const INVOCATION_JOURNAL_DIR_ENV: &str = "DJINN_INVOCATION_JOURNAL_DIR";

/// Upper bound on the journal volume.
///
/// The journal holds one small text record per *unresolved* invocation and
/// removes it on resolution, so the steady-state content is a handful of
/// records at most. 4Mi is generous for that and still bounds a runaway.
const INVOCATION_JOURNAL_SIZE_LIMIT: &str = "4Mi";

/// The worker-private journal volume.
///
/// Memory-backed: the records carry task/task-run/pod identity plus a lease
/// fencing token, they are only meaningful for the lifetime of THIS Pod (the
/// recorded pod UID is what recovery matches on), and an `emptyDir` — memory or
/// disk — dies with the Pod either way. Memory-backed keeps them off the node
/// filesystem and makes the `sync_all` the journal performs a no-op rather than
/// a disk barrier on a path that is written on every shell command.
pub fn invocation_journal_volume() -> Volume {
    Volume {
        name: VOLUME_INVOCATION_JOURNAL.to_string(),
        empty_dir: Some(EmptyDirVolumeSource {
            medium: Some("Memory".to_string()),
            size_limit: Some(Quantity(INVOCATION_JOURNAL_SIZE_LIMIT.to_string())),
        }),
        ..Volume::default()
    }
}

/// Worker-side mount of [`invocation_journal_volume`]. Writable, and mounted
/// into the worker ONLY — the journal is the worker's private record of which
/// invocations it still owes a resolution for, and neither the launcher nor a
/// backing-service sidecar has any business reading or writing it.
pub fn worker_invocation_journal_mount() -> VolumeMount {
    VolumeMount {
        name: VOLUME_INVOCATION_JOURNAL.to_string(),
        mount_path: INVOCATION_JOURNAL_DIR.to_string(),
        ..VolumeMount::default()
    }
}

/// The explicit journal-directory env the worker receives with the sidecar.
pub fn worker_invocation_journal_env() -> EnvVar {
    EnvVar {
        name: INVOCATION_JOURNAL_DIR_ENV.to_string(),
        value: Some(INVOCATION_JOURNAL_DIR.to_string()),
        ..EnvVar::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The env the worker reads and the mount the kubelet creates must name the
    /// SAME directory. If they drift, the worker opens a path no volume backs
    /// and falls straight back onto the read-only Secret mount — the exact
    /// blocker this module exists to close.
    #[test]
    fn journal_env_mount_and_default_all_name_one_directory() {
        assert_eq!(
            worker_invocation_journal_env().value.as_deref(),
            Some(INVOCATION_JOURNAL_DIR)
        );
        assert_eq!(
            worker_invocation_journal_mount().mount_path,
            INVOCATION_JOURNAL_DIR
        );
        assert_eq!(
            worker_invocation_journal_mount().name,
            invocation_journal_volume().name
        );
    }

    /// The journal mount must be WRITABLE. A `readOnly: true` mount here fails
    /// exactly the way the unmounted path did, and would be an easy thing to
    /// copy from the neighbouring read-only spec/token mounts.
    #[test]
    fn journal_mount_is_writable() {
        assert_ne!(
            worker_invocation_journal_mount().read_only,
            Some(true),
            "the worker creates and fsyncs this directory; a read-only mount is EROFS"
        );
    }

    /// The directory must sit under a real volume, not inside the read-only
    /// `spec` Secret mount root itself. `/var/run/djinn` IS that Secret mount,
    /// so the journal path has to be a strict descendant that a nested volume
    /// can cover.
    #[test]
    fn journal_directory_is_a_strict_descendant_of_the_spec_mount_root() {
        assert!(INVOCATION_JOURNAL_DIR.starts_with("/var/run/djinn/"));
        assert_ne!(INVOCATION_JOURNAL_DIR, "/var/run/djinn");
    }

    #[test]
    fn journal_volume_is_a_bounded_memory_empty_dir() {
        let volume = invocation_journal_volume();
        let empty_dir = volume.empty_dir.expect("journal is an emptyDir");
        assert_eq!(empty_dir.medium.as_deref(), Some("Memory"));
        assert_eq!(
            empty_dir.size_limit.map(|quantity| quantity.0).as_deref(),
            Some(INVOCATION_JOURNAL_SIZE_LIMIT)
        );
    }
}
