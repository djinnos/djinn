//! jemalloc startup configuration and process-wide memory statistics.
//!
//! The configuration parser is deliberately available on every target so the
//! server's startup contract is consistent. Statistics and live settings are
//! Linux-only because the server installs jemalloc there.

use std::env;
use std::fmt;

// Library tests do not link the binary-local production allocator. Keep this
// test-only allocator here so the Linux statistics smoke test exercises jemalloc.
#[cfg(all(test, target_os = "linux"))]
#[global_allocator]
static TEST_GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

/// A snapshot of the allocator's process-wide byte counters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AllocatorStats {
    pub allocated: usize,
    pub resident: usize,
    pub retained: usize,
}

/// A snapshot of the managed live jemalloc settings.
///
/// Unlike the startup parser, this is read from jemalloc's control interface
/// after the allocator has been linked and initialized.
#[cfg(target_os = "linux")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AllocatorSettings {
    pub background_thread: bool,
    pub dirty_decay_ms: isize,
    pub muzzy_decay_ms: isize,
}

/// Failure to parse the supported `MALLOC_CONF` startup options.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MallocConfError(String);

impl fmt::Display for MallocConfError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for MallocConfError {}

/// Validate the `MALLOC_CONF` value inherited by this process.
///
/// An unset variable is valid. The server deliberately accepts only the
/// allocator controls it manages: `background_thread`, `dirty_decay_ms`, and
/// `muzzy_decay_ms`.
pub fn validate_malloc_conf_from_env() -> Result<(), MallocConfError> {
    match env::var("MALLOC_CONF") {
        Ok(value) => validate_malloc_conf_value(Some(&value)),
        Err(env::VarError::NotPresent) => validate_malloc_conf_value(None),
        Err(env::VarError::NotUnicode(_)) => Err(MallocConfError(
            "MALLOC_CONF must contain valid Unicode configuration text".into(),
        )),
    }
}

fn validate_malloc_conf_value(value: Option<&str>) -> Result<(), MallocConfError> {
    match value {
        Some(value) => validate_malloc_conf(value),
        None => Ok(()),
    }
}

/// Validate a `MALLOC_CONF` string without reading process state.
pub fn validate_malloc_conf(value: &str) -> Result<(), MallocConfError> {
    if value.is_empty() {
        return Err(MallocConfError("configuration must not be empty".into()));
    }

    for option in value.split(',') {
        let (key, value) = option.split_once(':').ok_or_else(|| {
            MallocConfError(format!("option `{option}` must have the form key:value"))
        })?;
        if key.is_empty() || value.is_empty() || value.contains(':') {
            return Err(MallocConfError(format!("malformed option `{option}`")));
        }

        match key {
            "background_thread" => match value {
                "true" | "false" => {}
                _ => {
                    return Err(MallocConfError(format!(
                        "background_thread must be true or false, got `{value}`"
                    )));
                }
            },
            "dirty_decay_ms" | "muzzy_decay_ms" => {
                let decay_ms: i64 = value.parse().map_err(|_| {
                    MallocConfError(format!("{key} must be an integer, got `{value}`"))
                })?;
                if decay_ms < -1 {
                    return Err(MallocConfError(format!(
                        "{key} must be -1 or a non-negative integer, got `{value}`"
                    )));
                }
            }
            _ => {
                return Err(MallocConfError(format!(
                    "unsupported MALLOC_CONF key `{key}`"
                )));
            }
        }
    }

    Ok(())
}

/// Refresh jemalloc's epoch and read its current byte counters.
#[cfg(target_os = "linux")]
pub fn stats() -> Result<AllocatorStats, tikv_jemalloc_ctl::Error> {
    let epoch = tikv_jemalloc_ctl::epoch::mib()?;
    epoch.advance()?;
    Ok(AllocatorStats {
        allocated: tikv_jemalloc_ctl::stats::allocated::read()?,
        resident: tikv_jemalloc_ctl::stats::resident::read()?,
        retained: tikv_jemalloc_ctl::stats::retained::read()?,
    })
}

/// Read the managed live jemalloc settings through `mallctl`.
#[cfg(target_os = "linux")]
pub fn settings() -> Result<AllocatorSettings, tikv_jemalloc_ctl::Error> {
    use tikv_jemalloc_ctl::{Access, AsName};

    Ok(AllocatorSettings {
        background_thread: tikv_jemalloc_ctl::background_thread::read()?,
        dirty_decay_ms: b"arenas.dirty_decay_ms\0".name().read()?,
        muzzy_decay_ms: b"arenas.muzzy_decay_ms\0".name().read()?,
    })
}

#[cfg(test)]
mod tests {
    use super::{validate_malloc_conf, validate_malloc_conf_value};

    #[test]
    fn accepts_absent_malloc_conf() {
        assert!(validate_malloc_conf_value(None).is_ok());
    }

    #[test]
    fn accepts_supported_malloc_conf() {
        assert!(
            validate_malloc_conf("background_thread:true,dirty_decay_ms:10000,muzzy_decay_ms:-1")
                .is_ok()
        );
    }

    #[test]
    fn rejects_malformed_malloc_conf() {
        assert!(validate_malloc_conf("background_thread").is_err());
        assert!(validate_malloc_conf("background_thread:true,").is_err());
    }

    #[test]
    fn rejects_unknown_malloc_conf_key() {
        assert!(validate_malloc_conf("narenas:4").is_err());
    }

    #[test]
    fn rejects_invalid_typed_malloc_conf_values() {
        assert!(validate_malloc_conf("background_thread:yes").is_err());
        assert!(validate_malloc_conf("dirty_decay_ms:-2").is_err());
        assert!(validate_malloc_conf("muzzy_decay_ms:fast").is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn stats_refreshes_epoch_before_reading() {
        let stats = super::stats().expect("jemalloc statistics should be readable");
        assert!(stats.resident >= stats.allocated);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn helm_default_is_consumed_by_jemalloc() {
        const HELM_DEFAULT: &str =
            "background_thread:true,dirty_decay_ms:10000,muzzy_decay_ms:10000";
        const CHILD_ENV: &str = "DJINN_ALLOCATOR_SETTINGS_TEST_CHILD";

        if std::env::var_os(CHILD_ENV).is_some() {
            let settings = super::settings().expect("jemalloc settings should be readable");
            assert_eq!(
                settings,
                super::AllocatorSettings {
                    background_thread: true,
                    dirty_decay_ms: 10_000,
                    muzzy_decay_ms: 10_000,
                }
            );
            return;
        }

        let status = std::process::Command::new(
            std::env::current_exe().expect("test executable path should be available"),
        )
        .arg("--exact")
        .arg("allocator::tests::helm_default_is_consumed_by_jemalloc")
        .env(CHILD_ENV, "1")
        .env("MALLOC_CONF", HELM_DEFAULT)
        .status()
        .expect("controlled jemalloc test process should start");

        assert!(status.success(), "controlled jemalloc test process failed");
    }
}
