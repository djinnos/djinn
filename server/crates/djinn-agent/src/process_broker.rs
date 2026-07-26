//! Broker-backed remote child lifecycle for lease invocations.

use std::io;
use std::process::{Command, ExitStatus};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use djinn_cgroup_launcher::{
    CommandSpec, CpuStat, Invocation, broker::ChildStatus, transport::UnixBrokerClient,
};
use djinn_supervisor::services::{LeaseFencingToken, TaskInvocationLeaseIdentity};

#[cfg(unix)]
use std::os::unix::process::ExitStatusExt;

/// Remote process lifecycle owned by the authenticated launcher broker.
pub(crate) trait ProcessHandle: Send {
    fn drain_stdout(&mut self) -> io::Result<Vec<u8>>;
    fn drain_stderr(&mut self) -> io::Result<Vec<u8>>;
    fn try_wait(&mut self) -> io::Result<Option<ExitStatus>>;
    fn wait(&mut self) -> io::Result<ExitStatus>;
    fn sample_cpu(&mut self) -> io::Result<CpuStat>;
    fn fenced_lift(&mut self, fence: &LeaseFencingToken) -> io::Result<()>;
    fn kill(&mut self) -> io::Result<()>;
    fn wait_empty(&mut self) -> io::Result<()>;
    fn cleanup(&mut self) -> io::Result<()>;
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) trait CgroupLauncherClient: Send + Sync + 'static {
    fn launch(
        &self,
        command: Command,
        identity: &TaskInvocationLeaseIdentity,
    ) -> io::Result<Box<dyn ProcessHandle>>;
}

/// Production-only Unix broker adapter; it has no local `Command::spawn` fallback.
pub(crate) struct UnixBrokerLauncher {
    client: Arc<Mutex<UnixBrokerClient>>,
    invocation_fence: u64,
}

impl UnixBrokerLauncher {
    #[allow(dead_code)] // constructed by workspace broker composition
    pub(crate) fn new(client: UnixBrokerClient, invocation_fence: u64) -> Self {
        Self {
            client: Arc::new(Mutex::new(client)),
            invocation_fence,
        }
    }
}

impl CgroupLauncherClient for UnixBrokerLauncher {
    fn launch(
        &self,
        command: Command,
        identity: &TaskInvocationLeaseIdentity,
    ) -> io::Result<Box<dyn ProcessHandle>> {
        let spec = command_spec(command)?;
        let id = identity.invocation_id.clone();
        let mut client = lock_client(&self.client)?;
        client
            .begin(Invocation {
                id: id.clone(),
                fence: self.invocation_fence,
            })
            .map_err(broker_error)?;
        if let Err(error) = client.create(&id, &id, &spec) {
            let _ = client.cleanup(&id);
            return Err(broker_error(error));
        }
        Ok(Box::new(UnixBrokerProcessHandle {
            client: self.client.clone(),
            id,
            status: None,
            stdout: Vec::new(),
        }))
    }
}

struct UnixBrokerProcessHandle {
    client: Arc<Mutex<UnixBrokerClient>>,
    id: String,
    status: Option<ExitStatus>,
    // Status polling uses the broker's stdout operation, which drains its
    // accumulated stream buffer. Preserve those bytes for the next drain.
    stdout: Vec<u8>,
}

impl UnixBrokerProcessHandle {
    fn record(&mut self, status: ChildStatus) -> io::Result<()> {
        if status != ChildStatus::Running {
            self.status = Some(remote_exit_status(status)?);
        }
        Ok(())
    }

    fn output(&mut self, stderr: bool) -> io::Result<Vec<u8>> {
        let (bytes, _, status) = if stderr {
            lock_client(&self.client)?
                .stderr(&self.id)
                .map_err(broker_error)?
        } else {
            lock_client(&self.client)?
                .stdout(&self.id)
                .map_err(broker_error)?
        };
        self.record(status)?;
        Ok(bytes)
    }
}

impl ProcessHandle for UnixBrokerProcessHandle {
    fn drain_stdout(&mut self) -> io::Result<Vec<u8>> {
        let mut output = std::mem::take(&mut self.stdout);
        output.extend(self.output(false)?);
        Ok(output)
    }

    fn drain_stderr(&mut self) -> io::Result<Vec<u8>> {
        self.output(true)
    }

    fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        if self.status.is_none() {
            let (output, _, status) = lock_client(&self.client)?
                .stdout(&self.id)
                .map_err(broker_error)?;
            self.stdout.extend(output);
            self.record(status)?;
        }
        Ok(self.status)
    }

    fn wait(&mut self) -> io::Result<ExitStatus> {
        loop {
            if let Some(status) = self.try_wait()? {
                return Ok(status);
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    fn sample_cpu(&mut self) -> io::Result<CpuStat> {
        lock_client(&self.client)?
            .sample(&self.id)
            .map_err(broker_error)
    }

    fn fenced_lift(&mut self, fence: &LeaseFencingToken) -> io::Result<()> {
        lock_client(&self.client)?
            .lift(&self.id, fence.0)
            .map_err(broker_error)
    }

    fn kill(&mut self) -> io::Result<()> {
        lock_client(&self.client)?
            .kill(&self.id)
            .map_err(broker_error)
    }

    fn wait_empty(&mut self) -> io::Result<()> {
        lock_client(&self.client)?
            .wait_empty(&self.id)
            .map_err(broker_error)
    }

    fn cleanup(&mut self) -> io::Result<()> {
        lock_client(&self.client)?
            .cleanup(&self.id)
            .map_err(broker_error)
    }
}

fn lock_client(
    client: &Arc<Mutex<UnixBrokerClient>>,
) -> io::Result<std::sync::MutexGuard<'_, UnixBrokerClient>> {
    client
        .lock()
        .map_err(|_| io::Error::other("launcher broker client mutex poisoned"))
}

fn broker_error(error: djinn_cgroup_launcher::Error) -> io::Error {
    io::Error::other(error)
}

#[cfg(unix)]
fn remote_exit_status(status: ChildStatus) -> io::Result<ExitStatus> {
    match status {
        ChildStatus::Running => Err(io::Error::other("running child is not terminal")),
        ChildStatus::Exited(code) => Ok(ExitStatus::from_raw(i32::from(code) << 8)),
        ChildStatus::Signaled(signal) => Ok(ExitStatus::from_raw(i32::from(signal))),
    }
}

#[cfg(not(unix))]
fn remote_exit_status(_: ChildStatus) -> io::Result<ExitStatus> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "unix broker status",
    ))
}

fn command_spec(command: Command) -> io::Result<CommandSpec> {
    let text = |value: &std::ffi::OsStr, what| {
        value.to_str().map(str::to_owned).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, format!("{what} must be UTF-8"))
        })
    };
    let program = text(command.get_program(), "program")?;
    let argv = command
        .get_args()
        .map(|argument| text(argument, "argument"))
        .collect::<io::Result<Vec<_>>>()?;
    let cwd = command
        .get_current_dir()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "command cwd is required"))?;
    let cwd = text(cwd.as_os_str(), "cwd")?;
    let spec = CommandSpec {
        program,
        argv,
        cwd,
        environment: child_environment(&command)?,
    };
    spec.validate().map_err(broker_error)?;
    Ok(spec)
}

/// The environment a brokered child is given.
///
/// # Why the worker's own environment has to be forwarded (task 7deu, defect 2)
///
/// The launcher execs the child with **exactly** the environment in the
/// [`CommandSpec`] — never the broker's. That is the right isolation property,
/// but `Command::get_envs()` returns only the variables a caller explicitly set
/// on the command, not the ones the process inherited. Under the in-process
/// path the child inherits the pod's environment; under the broker path it got
/// nothing. So a brokered build ran with no `CARGO_TARGET_DIR` (a cold build, in
/// the wrong directory, missing the warm cache), no `CARGO_BUILD_RUSTFLAGS` (no
/// mold linker), no `RUSTUP_HOME`, and no `CARGO_BUILD_JOBS` — the last of which
/// silently reverted the load-103 fix.
///
/// Compilation is 60-90% of task wall clock, so that is not a rough edge; it is
/// the feature making the thing it governs dramatically slower.
///
/// The fix keeps the allow-list closed and forwards through it:
/// `djinn_cgroup_launcher::is_allowed_environment_key` is the single predicate,
/// applied here as a convenience. The actual control is inside the privileged
/// broker, in `CommandSpec::validate`, and it is the strictly stronger
/// `is_allowed_environment_entry`: one forwardable name (`GIT_CONFIG_SYSTEM`)
/// points at a *configuration file*, so it is judged by its value and not its
/// key. Explicit per-command values win over inherited ones; the launcher then
/// overrides the parallelism pins — and the git trust anchor — with values it
/// derives itself, because only it knows the quota the leaf will really run at
/// and only it owns a git config file that is safe to point a child at.
fn child_environment(command: &Command) -> io::Result<Vec<(String, String)>> {
    use std::collections::BTreeMap;

    let mut environment: BTreeMap<String, String> = std::env::vars()
        .filter(|(key, _)| djinn_cgroup_launcher::is_allowed_environment_key(key))
        .collect();

    for (key, value) in command.get_envs() {
        let key = key.to_str().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "environment key must be UTF-8")
        })?;
        match value {
            // `Command::env_remove` is represented as a `None` value.
            None => {
                environment.remove(key);
            }
            Some(value) => {
                let value = value.to_str().ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "environment value must be UTF-8",
                    )
                })?;
                environment.insert(key.to_owned(), value.to_owned());
            }
        }
    }

    Ok(environment.into_iter().collect())
}

#[cfg(test)]
impl ProcessHandle for std::process::Child {
    fn drain_stdout(&mut self) -> io::Result<Vec<u8>> {
        Ok(Vec::new())
    }

    fn drain_stderr(&mut self) -> io::Result<Vec<u8>> {
        Ok(Vec::new())
    }

    fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        std::process::Child::try_wait(self)
    }

    fn wait(&mut self) -> io::Result<ExitStatus> {
        std::process::Child::wait(self)
    }

    fn sample_cpu(&mut self) -> io::Result<CpuStat> {
        Ok(CpuStat {
            usage_usec: 10,
            ..CpuStat::default()
        })
    }

    fn fenced_lift(&mut self, _: &LeaseFencingToken) -> io::Result<()> {
        Ok(())
    }

    fn kill(&mut self) -> io::Result<()> {
        std::process::Child::kill(self)
    }

    fn wait_empty(&mut self) -> io::Result<()> {
        Ok(())
    }

    fn cleanup(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod broker_environment_tests {
    use super::*;

    /// The pod's build environment must survive the broker hop, or a brokered
    /// build runs cold, in the wrong target directory, without mold — the exact
    /// regression that made routing through the launcher a net loss.
    #[test]
    fn the_inherited_build_environment_reaches_the_child() {
        // SAFETY: single-threaded test, and both keys are test-local.
        unsafe {
            std::env::set_var("CARGO_TARGET_DIR", "/cache/cargo-target/proj/mold-jobs-4");
            std::env::set_var("DJINN_BROKER_ENV_TEST_SECRET", "kept");
            std::env::set_var("AWS_SECRET_ACCESS_KEY", "must-not-leak");
        }

        let mut command = Command::new("/bin/sh");
        command.current_dir("/workspace");
        let spec = command_spec(command).expect("spec");
        let value = |key: &str| {
            spec.environment
                .iter()
                .find(|(name, _)| name == key)
                .map(|(_, value)| value.as_str())
        };

        assert_eq!(
            value("CARGO_TARGET_DIR"),
            Some("/cache/cargo-target/proj/mold-jobs-4"),
            "the warm cache directory must reach a brokered build"
        );
        // The allow-list is still closed: an unlisted key is not forwarded.
        assert_eq!(value("AWS_SECRET_ACCESS_KEY"), None);

        unsafe {
            std::env::remove_var("CARGO_TARGET_DIR");
            std::env::remove_var("DJINN_BROKER_ENV_TEST_SECRET");
            std::env::remove_var("AWS_SECRET_ACCESS_KEY");
        }
    }

    /// An explicitly-set value wins over the inherited one, and `env_remove`
    /// really removes rather than being ignored.
    #[test]
    fn explicit_command_environment_overrides_and_removes() {
        // SAFETY: single-threaded test, test-local key.
        unsafe { std::env::set_var("RUST_LOG", "inherited") };

        let mut command = Command::new("/bin/sh");
        command.current_dir("/workspace");
        command.env("RUST_LOG", "explicit");
        command.env("CARGO_INCREMENTAL", "0");
        command.env_remove("TERM");
        let spec = command_spec(command).expect("spec");
        let value = |key: &str| {
            spec.environment
                .iter()
                .find(|(name, _)| name == key)
                .map(|(_, value)| value.as_str())
        };

        assert_eq!(value("RUST_LOG"), Some("explicit"));
        assert_eq!(value("CARGO_INCREMENTAL"), Some("0"));
        assert_eq!(value("TERM"), None);

        unsafe { std::env::remove_var("RUST_LOG") };
    }
}
