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
        }))
    }
}

struct UnixBrokerProcessHandle {
    client: Arc<Mutex<UnixBrokerClient>>,
    id: String,
    status: Option<ExitStatus>,
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
        self.output(false)
    }

    fn drain_stderr(&mut self) -> io::Result<Vec<u8>> {
        self.output(true)
    }

    fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        if self.status.is_none() {
            let status = lock_client(&self.client)?
                .stdout(&self.id)
                .map_err(broker_error)?
                .2;
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
    let environment = command
        .get_envs()
        .filter_map(|(key, value)| {
            value.map(|value| {
                Ok((
                    text(key, "environment key")?,
                    text(value, "environment value")?,
                ))
            })
        })
        .collect::<io::Result<Vec<_>>>()?;
    let spec = CommandSpec {
        program,
        argv,
        cwd,
        environment,
    };
    spec.validate().map_err(broker_error)?;
    Ok(spec)
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
