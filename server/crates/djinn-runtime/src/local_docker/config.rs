//! Configuration for [`crate::local_docker::LocalDockerRuntime`].
//!
//! Phase 2 PR 6 of `/home/fernando/.claude/plans/phase2-localdocker-scaffolding.md`.
//!
//! The config captures every per-deployment path + resource knob the runtime
//! needs to bind mounts + docker-create a container.  The values are resolved
//! once at startup — a task-run never re-reads this struct after `prepare`.
//!
//! [`LocalDockerConfig::from_env`] mirrors the `docker-compose.yml` contract:
//! `DJINN_HOME` (default `~/.djinn`) hangs the mirror / cache / ipc roots off
//! a single directory.  Individual env overrides (`DJINN_DOCKER_IMAGE`,
//! `DJINN_MIRRORS_ROOT`, `DJINN_CACHE_ROOT`, `DJINN_IPC_ROOT`,
//! `DJINN_RUNTIME_MEMORY_BYTES`, `DJINN_RUNTIME_NANO_CPUS`) let ops pin any
//! field without touching the default layout.

use std::path::{Path, PathBuf};

/// Host-side paths + resource limits that shape one `docker create` call.
///
/// All path fields are absolute host paths — [`crate::local_docker::LocalDockerRuntime`]
/// bind-mounts them into the container at fixed container paths documented
/// in `server/docker/djinn-agent-runtime.Dockerfile`.
#[derive(Clone, Debug)]
pub struct LocalDockerConfig {
    /// Image tag to `docker run` — e.g. `djinn-agent-runtime:local`.
    pub image_tag: String,
    /// Host root holding `<project_id>.git` bare mirrors.  Bound read-only at
    /// `/mirror` inside the container.
    pub mirrors_root: PathBuf,
    /// Host root holding `cargo/`, `pnpm/`, `pip/` subdirectories.  Each is
    /// bound at `/cache/<name>` inside the container.
    pub cache_root: PathBuf,
    /// Host root holding per-task-run Unix domain sockets.  Bound rw at
    /// `/var/run/djinn` inside the container — the worker dials
    /// `$DJINN_IPC_SOCKET` (a file inside that directory) for its reverse-RPC
    /// channel.
    pub ipc_root: PathBuf,
    /// Hard memory cap (bytes) applied via `HostConfig.memory`.  4 GiB default.
    pub memory_limit_bytes: i64,
    /// CPU quota in units of 10⁻⁹ CPUs (`HostConfig.nano_cpus`).  2 full cores
    /// default.
    pub nano_cpus: i64,
}

impl Default for LocalDockerConfig {
    fn default() -> Self {
        Self {
            image_tag: "djinn-agent-runtime:local".into(),
            mirrors_root: PathBuf::from("/var/lib/djinn/mirrors"),
            cache_root: PathBuf::from("/var/lib/djinn/cache"),
            ipc_root: PathBuf::from("/var/run/djinn"),
            memory_limit_bytes: 4 * 1024 * 1024 * 1024,
            nano_cpus: 2_000_000_000,
        }
    }
}

impl LocalDockerConfig {
    /// Fluent builder seeded from [`Default`].
    pub fn builder() -> LocalDockerConfigBuilder {
        LocalDockerConfigBuilder::default()
    }

    /// Resolve the config from environment variables, matching the contract
    /// the `docker-compose.yml` stack sets up.
    ///
    /// Variables read (all optional):
    ///
    /// - `DJINN_DOCKER_IMAGE` — image tag (default `djinn-agent-runtime:local`).
    /// - `DJINN_MIRRORS_ROOT` — mirror root (default `/var/lib/djinn/mirrors`).
    /// - `DJINN_CACHE_ROOT` — cache root (default `/var/lib/djinn/cache`).
    /// - `DJINN_IPC_ROOT` — ipc socket root (default `/var/run/djinn`).
    /// - `DJINN_RUNTIME_MEMORY_BYTES` — memory cap (i64 parse; default 4 GiB).
    /// - `DJINN_RUNTIME_NANO_CPUS` — nano-cpu cap (i64 parse; default 2·10⁹).
    pub fn from_env() -> Self {
        let mut cfg = Self::default();
        if let Ok(v) = std::env::var("DJINN_DOCKER_IMAGE") {
            cfg.image_tag = v;
        }
        if let Ok(v) = std::env::var("DJINN_MIRRORS_ROOT") {
            cfg.mirrors_root = PathBuf::from(v);
        }
        if let Ok(v) = std::env::var("DJINN_CACHE_ROOT") {
            cfg.cache_root = PathBuf::from(v);
        }
        if let Ok(v) = std::env::var("DJINN_IPC_ROOT") {
            cfg.ipc_root = PathBuf::from(v);
        }
        if let Ok(v) = std::env::var("DJINN_RUNTIME_MEMORY_BYTES") {
            if let Ok(n) = v.parse::<i64>() {
                cfg.memory_limit_bytes = n;
            }
        }
        if let Ok(v) = std::env::var("DJINN_RUNTIME_NANO_CPUS") {
            if let Ok(n) = v.parse::<i64>() {
                cfg.nano_cpus = n;
            }
        }
        cfg
    }

    /// Host cache subpath for the given cache name (`cargo`, `pnpm`, `pip`).
    pub fn cache_subpath(&self, name: &str) -> PathBuf {
        self.cache_root.join(name)
    }

    /// Host path of the per-task-run Unix socket the worker will dial.
    pub fn socket_path_for(&self, task_run_id: &str) -> PathBuf {
        self.ipc_root.join(format!("{task_run_id}.sock"))
    }
}

/// Fluent builder for [`LocalDockerConfig`].
#[derive(Clone, Debug, Default)]
pub struct LocalDockerConfigBuilder {
    inner: Option<LocalDockerConfig>,
}

impl LocalDockerConfigBuilder {
    fn ensure(&mut self) -> &mut LocalDockerConfig {
        self.inner.get_or_insert_with(LocalDockerConfig::default)
    }

    pub fn image_tag(mut self, v: impl Into<String>) -> Self {
        self.ensure().image_tag = v.into();
        self
    }

    pub fn mirrors_root(mut self, v: impl AsRef<Path>) -> Self {
        self.ensure().mirrors_root = v.as_ref().to_path_buf();
        self
    }

    pub fn cache_root(mut self, v: impl AsRef<Path>) -> Self {
        self.ensure().cache_root = v.as_ref().to_path_buf();
        self
    }

    pub fn ipc_root(mut self, v: impl AsRef<Path>) -> Self {
        self.ensure().ipc_root = v.as_ref().to_path_buf();
        self
    }

    pub fn memory_limit_bytes(mut self, v: i64) -> Self {
        self.ensure().memory_limit_bytes = v;
        self
    }

    pub fn nano_cpus(mut self, v: i64) -> Self {
        self.ensure().nano_cpus = v;
        self
    }

    pub fn build(self) -> LocalDockerConfig {
        self.inner.unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_has_reasonable_caps() {
        let c = LocalDockerConfig::default();
        assert_eq!(c.memory_limit_bytes, 4 * 1024 * 1024 * 1024);
        assert_eq!(c.nano_cpus, 2_000_000_000);
        assert_eq!(c.image_tag, "djinn-agent-runtime:local");
    }

    #[test]
    fn builder_overrides_fields() {
        let c = LocalDockerConfig::builder()
            .image_tag("custom:tag")
            .mirrors_root("/tmp/m")
            .cache_root("/tmp/c")
            .ipc_root("/tmp/i")
            .memory_limit_bytes(1024)
            .nano_cpus(500_000_000)
            .build();
        assert_eq!(c.image_tag, "custom:tag");
        assert_eq!(c.mirrors_root, PathBuf::from("/tmp/m"));
        assert_eq!(c.cache_root, PathBuf::from("/tmp/c"));
        assert_eq!(c.ipc_root, PathBuf::from("/tmp/i"));
        assert_eq!(c.memory_limit_bytes, 1024);
        assert_eq!(c.nano_cpus, 500_000_000);
    }

    #[test]
    fn socket_path_composes_task_run_id() {
        let c = LocalDockerConfig::builder().ipc_root("/tmp/ipc").build();
        assert_eq!(
            c.socket_path_for("run-42"),
            PathBuf::from("/tmp/ipc/run-42.sock")
        );
    }

    #[test]
    fn cache_subpath_composes_name() {
        let c = LocalDockerConfig::builder().cache_root("/tmp/cache").build();
        assert_eq!(c.cache_subpath("cargo"), PathBuf::from("/tmp/cache/cargo"));
    }
}
