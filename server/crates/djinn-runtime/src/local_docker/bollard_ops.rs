//! Thin wrappers around the handful of bollard calls [`super::LocalDockerRuntime`]
//! needs.
//!
//! Phase 2 PR 6 of `/home/fernando/.claude/plans/phase2-localdocker-scaffolding.md`.
//!
//! Keeping the bollard surface area in one file (a) isolates the version
//! coupling — if we bump bollard, diffs stay local — and (b) keeps `mod.rs`
//! focused on lifecycle flow rather than docker-client plumbing.
//!
//! Every helper takes plain `&Docker` + primitive args, returns a bollard
//! [`Error`][bollard::errors::Error] directly.  Translation to
//! [`crate::RuntimeError`] happens at the `LocalDockerRuntime` boundary.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use bollard::Docker;
use bollard::container::{
    AttachContainerOptions, AttachContainerResults, Config, CreateContainerOptions,
    KillContainerOptions, RemoveContainerOptions, StartContainerOptions,
};
use bollard::errors::Error as BollardError;
use bollard::models::{ContainerCreateResponse, HostConfig};
use tokio::io::AsyncWriteExt;

use super::config::LocalDockerConfig;

/// Bind spec in docker's `HostConfig.Binds` format: `host:container[:mode]`.
#[derive(Clone, Debug)]
pub(crate) struct BindSpec {
    pub host: PathBuf,
    pub container: &'static str,
    pub mode: &'static str,
}

impl BindSpec {
    fn render(&self) -> String {
        format!("{}:{}:{}", self.host.display(), self.container, self.mode)
    }
}

/// Compute the six bind mounts every task-run container needs — matches the
/// contract documented in `server/docker/djinn-agent-runtime.Dockerfile`.
///
/// * `workspace_path` — host tempdir the ephemeral clone lives under → rw `/workspace`.
/// * `cfg.mirrors_root` → ro `/mirror`.
/// * `cfg.cache_root/{cargo,pnpm,pip}` → rw `/cache/<name>`.
/// * `cfg.ipc_root` → rw `/var/run/djinn`.
pub(crate) fn default_binds(cfg: &LocalDockerConfig, workspace_path: &Path) -> Vec<BindSpec> {
    vec![
        BindSpec {
            host: workspace_path.to_path_buf(),
            container: "/workspace",
            mode: "rw",
        },
        BindSpec {
            host: cfg.mirrors_root.clone(),
            container: "/mirror",
            mode: "ro",
        },
        BindSpec {
            host: cfg.cache_subpath("cargo"),
            container: "/cache/cargo",
            mode: "rw",
        },
        BindSpec {
            host: cfg.cache_subpath("pnpm"),
            container: "/cache/pnpm",
            mode: "rw",
        },
        BindSpec {
            host: cfg.cache_subpath("pip"),
            container: "/cache/pip",
            mode: "rw",
        },
        BindSpec {
            host: cfg.ipc_root.clone(),
            container: "/var/run/djinn",
            mode: "rw",
        },
    ]
}

/// Render the bind specs into docker's `Vec<String>` shape.
pub(crate) fn binds_as_strings(binds: &[BindSpec]) -> Vec<String> {
    binds.iter().map(|b| b.render()).collect()
}

/// Build the env block docker passes into the container.
///
/// The runtime sets `DJINN_IPC_SOCKET` to the container-side socket path
/// (inside `/var/run/djinn`) so the worker can dial it without knowing the
/// host-side root.  Other paths mirror the Dockerfile defaults but are
/// re-set here so bespoke `LocalDockerConfig`s stay authoritative.
pub(crate) fn default_env(container_socket_path: &str) -> Vec<String> {
    vec![
        format!("DJINN_IPC_SOCKET={container_socket_path}"),
        "CARGO_HOME=/cache/cargo".to_string(),
        "CARGO_TARGET_DIR=/workspace/target".to_string(),
        "PNPM_STORE_DIR=/cache/pnpm".to_string(),
        "PIP_CACHE_DIR=/cache/pip".to_string(),
        "RUST_LOG=info,djinn=debug".to_string(),
    ]
}

/// HostConfig defaults mandated by §4 of the scaffolding plan:
/// 4 GiB memory cap, 2-core nano-cpus, `cap_drop: [ALL]`,
/// `security_opt: [no-new-privileges:true]`, bridge network (egress for the
/// provider API).
pub(crate) fn default_host_config(cfg: &LocalDockerConfig, binds: Vec<String>) -> HostConfig {
    HostConfig {
        binds: Some(binds),
        memory: Some(cfg.memory_limit_bytes),
        nano_cpus: Some(cfg.nano_cpus),
        cap_drop: Some(vec!["ALL".to_string()]),
        security_opt: Some(vec!["no-new-privileges:true".to_string()]),
        network_mode: Some("bridge".to_string()),
        auto_remove: Some(false),
        ..Default::default()
    }
}

/// Arguments bundled for [`create_container_for_run`].
pub(crate) struct CreateArgs<'a> {
    pub image: &'a str,
    pub name: &'a str,
    pub cmd: Vec<String>,
    pub env: Vec<String>,
    pub host_config: HostConfig,
    /// Working directory inside the container (typically `/workspace`).
    pub workdir: &'a str,
}

/// Create a container configured for stdin pipe, ready to be started +
/// attached.  Returns the freshly-created container id.
pub(crate) async fn create_container_for_run(
    docker: &Docker,
    args: CreateArgs<'_>,
) -> Result<ContainerCreateResponse, BollardError> {
    let config: Config<String> = Config {
        image: Some(args.image.to_string()),
        cmd: Some(args.cmd),
        env: Some(args.env),
        host_config: Some(args.host_config),
        working_dir: Some(args.workdir.to_string()),
        attach_stdin: Some(true),
        open_stdin: Some(true),
        stdin_once: Some(true),
        attach_stdout: Some(true),
        attach_stderr: Some(true),
        tty: Some(false),
        labels: Some(HashMap::from([(
            "djinn.component".to_string(),
            "task-run".to_string(),
        )])),
        ..Default::default()
    };
    let opts = CreateContainerOptions {
        name: args.name.to_string(),
        platform: None,
    };
    docker.create_container(Some(opts), config).await
}

/// `docker start` the named container.
pub(crate) async fn start_container(docker: &Docker, id: &str) -> Result<(), BollardError> {
    docker
        .start_container(id, None::<StartContainerOptions<String>>)
        .await
}

/// Attach to the container with stdin enabled, returning the duplex handle.
///
/// Caller writes the bincode-serialized spec to `input` then drops it; the
/// container observes EOF on its stdin (docker closes on detach thanks to
/// `stdin_once: true` in the create config).
pub(crate) async fn attach_stdin_stdout(
    docker: &Docker,
    id: &str,
) -> Result<AttachContainerResults, BollardError> {
    let opts: AttachContainerOptions<String> = AttachContainerOptions {
        stdin: Some(true),
        stdout: Some(true),
        stderr: Some(true),
        stream: Some(true),
        logs: Some(false),
        detach_keys: None,
    };
    docker.attach_container(id, Some(opts)).await
}

/// Write the given payload to the container stdin then shut the write half.
pub(crate) async fn pipe_spec_to_stdin<T>(
    attach: &mut AttachContainerResults,
    payload: &T,
) -> Result<(), crate::RuntimeError>
where
    T: serde::Serialize,
{
    // Re-use the wire codec so the worker-side `read_frame` parses this
    // exactly the same way `serve_on_unix_socket` parses its inbound frames.
    crate::wire::write_frame(&mut attach.input, payload)
        .await
        .map_err(|e| crate::RuntimeError::Prepare(format!("write spec frame: {e}")))?;
    attach
        .input
        .shutdown()
        .await
        .map_err(|e| crate::RuntimeError::Prepare(format!("shutdown stdin: {e}")))?;
    Ok(())
}

/// SIGTERM a running container with `grace_seconds` before docker escalates
/// to SIGKILL.  Idempotent in the sense that a "container already stopped"
/// error from bollard is swallowed to `Ok(())`.
pub(crate) async fn kill_container(
    docker: &Docker,
    id: &str,
    signal: &str,
) -> Result<(), BollardError> {
    let opts = KillContainerOptions {
        signal: signal.to_string(),
    };
    match docker.kill_container(id, Some(opts)).await {
        Ok(()) => Ok(()),
        // Already stopped / already removed — treat as success.
        Err(BollardError::DockerResponseServerError {
            status_code: 404 | 409,
            ..
        }) => Ok(()),
        Err(e) => Err(e),
    }
}

/// Force-remove the container (running or not).  404 is swallowed to keep
/// teardown idempotent.
pub(crate) async fn remove_container(docker: &Docker, id: &str) -> Result<(), BollardError> {
    let opts = RemoveContainerOptions {
        force: true,
        v: false,
        link: false,
    };
    match docker.remove_container(id, Some(opts)).await {
        Ok(()) => Ok(()),
        Err(BollardError::DockerResponseServerError {
            status_code: 404, ..
        }) => Ok(()),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_binds_matches_dockerfile_paths() {
        let cfg = LocalDockerConfig::builder()
            .mirrors_root("/m")
            .cache_root("/c")
            .ipc_root("/i")
            .build();
        let binds = default_binds(&cfg, Path::new("/ws"));
        let rendered = binds_as_strings(&binds);
        assert!(rendered.contains(&"/ws:/workspace:rw".to_string()));
        assert!(rendered.contains(&"/m:/mirror:ro".to_string()));
        assert!(rendered.contains(&"/c/cargo:/cache/cargo:rw".to_string()));
        assert!(rendered.contains(&"/c/pnpm:/cache/pnpm:rw".to_string()));
        assert!(rendered.contains(&"/c/pip:/cache/pip:rw".to_string()));
        assert!(rendered.contains(&"/i:/var/run/djinn:rw".to_string()));
    }

    #[test]
    fn default_env_has_ipc_socket_env_var() {
        let env = default_env("/var/run/djinn/run-1.sock");
        assert!(env.iter().any(|s| s == "DJINN_IPC_SOCKET=/var/run/djinn/run-1.sock"));
        assert!(env.iter().any(|s| s.starts_with("CARGO_HOME=")));
    }

    #[test]
    fn default_host_config_applies_hard_caps() {
        let cfg = LocalDockerConfig::builder()
            .memory_limit_bytes(1_000_000)
            .nano_cpus(500_000_000)
            .build();
        let hc = default_host_config(&cfg, vec!["/a:/b:rw".into()]);
        assert_eq!(hc.memory, Some(1_000_000));
        assert_eq!(hc.nano_cpus, Some(500_000_000));
        assert_eq!(hc.cap_drop, Some(vec!["ALL".into()]));
        assert_eq!(
            hc.security_opt,
            Some(vec!["no-new-privileges:true".into()])
        );
        assert_eq!(hc.network_mode.as_deref(), Some("bridge"));
    }
}
