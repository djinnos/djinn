//! Preparation of the cgroup root delegated by the kubelet RuntimeClass.

use std::path::{Path, PathBuf};

use crate::Error;

/// Identity capabilities retained by the launcher to prepare brokered children.
pub const RETAINED_CAPABILITY_NAMES: &[&str] = &["CHOWN", "SETGID", "SETUID", "SETPCAP"];
/// Holding leaf required by cgroup v2's no-internal-process rule.
pub const INIT_LEAF: &str = "init";
const DELEGATED_CONTROLLER: &str = "cpu";

/// Performs the cgroup operations that remain necessary after kubelet delegation.
pub struct Bootstrap {
    root: PathBuf,
}

impl Bootstrap {
    pub fn new(root: impl AsRef<Path>) -> Self {
        Self {
            root: root.as_ref().to_path_buf(),
        }
    }

    /// Vacate the delegated root and delegate exactly the CPU controller.
    pub fn run(&self) -> Result<(), Error> {
        self.vacate_root()?;
        self.delegate_cpu()
    }

    fn vacate_root(&self) -> Result<(), Error> {
        let init = self.root.join(INIT_LEAF);
        if !init.is_dir() {
            std::fs::create_dir(&init).map_err(|error| Error::CgroupDelegationFailed {
                detail: "create the init holding leaf",
                errno: error.raw_os_error().unwrap_or(0),
            })?;
        }
        let procs = std::fs::read_to_string(self.root.join("cgroup.procs")).map_err(|error| {
            Error::CgroupDelegationFailed {
                detail: "read the delegated root's cgroup.procs",
                errno: error.raw_os_error().unwrap_or(0),
            }
        })?;
        for pid in procs.split_ascii_whitespace() {
            std::fs::write(init.join("cgroup.procs"), pid).map_err(|error| {
                Error::CgroupDelegationFailed {
                    detail: "move the launcher's own process into the init leaf",
                    errno: error.raw_os_error().unwrap_or(0),
                }
            })?;
        }
        Ok(())
    }

    fn delegate_cpu(&self) -> Result<(), Error> {
        let control = self.root.join("cgroup.subtree_control");
        let enabled = std::fs::read_to_string(&control).map_err(|error| {
            Error::CgroupDelegationFailed {
                detail: "read cgroup.subtree_control",
                errno: error.raw_os_error().unwrap_or(0),
            }
        })?;
        let mut directives: Vec<String> = enabled
            .split_ascii_whitespace()
            .filter(|controller| *controller != DELEGATED_CONTROLLER)
            .map(|controller| format!("-{controller}"))
            .collect();
        if !enabled
            .split_ascii_whitespace()
            .any(|controller| controller == DELEGATED_CONTROLLER)
        {
            directives.push("+cpu".to_string());
        }
        if directives.is_empty() {
            return Ok(());
        }
        std::fs::write(&control, directives.join(" ")).map_err(|error| {
            Error::CgroupDelegationFailed {
                detail: "enable the cpu controller in cgroup.subtree_control",
                errno: error.raw_os_error().unwrap_or(0),
            }
        })
    }
}
