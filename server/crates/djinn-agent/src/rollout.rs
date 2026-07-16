//! Crate-internal parsing for operator-controlled feature rollouts.
//!
//! This deliberately has no assignment logic: a cohort value is a deployment
//! label, not a per-session experiment arm. Callers select their default policy
//! so old default-off gates and default-enabled gates can share the vocabulary.

/// The policy to apply when neither rollout nor legacy configuration is set.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DefaultPolicy {
    /// Used by default-enabled gates such as session knowledge context.
    #[allow(dead_code)]
    Enabled,
    Off,
}

/// A parsed rollout configuration.
///
/// `Cohort` retains the complete deployment value (for example,
/// `cohort:canary-blue`) so trace writers can record it verbatim.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum RolloutMode {
    Enabled,
    Cohort(String),
    Off,
    KillSwitch,
    LegacyEnabled,
    LegacyDisabled,
}

impl RolloutMode {
    /// The stable deployment label for telemetry and trace metadata.
    pub(crate) fn label(&self) -> &str {
        match self {
            Self::Enabled => "enabled",
            Self::Cohort(label) => label,
            Self::Off => "off",
            Self::KillSwitch => "kill_switch",
            Self::LegacyEnabled => "legacy_enabled",
            Self::LegacyDisabled => "legacy_disabled",
        }
    }

    pub(crate) fn enabled(&self) -> bool {
        matches!(self, Self::Enabled | Self::Cohort(_) | Self::LegacyEnabled)
    }
}

/// Parse rollout and optional compatibility environment values.
///
/// An explicit rollout value always takes precedence over the legacy value,
/// including unknown values. Unknown values and legacy values other than `1`
/// are represented as `LegacyDisabled` so persistence callers can distinguish
/// them from a deliberate `off` value.
pub(crate) fn parse(
    rollout: Option<&str>,
    legacy: Option<&str>,
    default_policy: DefaultPolicy,
) -> RolloutMode {
    if let Some(value) = rollout.map(str::trim).filter(|value| !value.is_empty()) {
        let normalized = value.to_ascii_lowercase().replace('-', "_");
        return match normalized.as_str() {
            "enabled" | "enable" | "on" | "true" | "1" => RolloutMode::Enabled,
            "cohort" | "staging" | "rollout" | "controlled" => {
                RolloutMode::Cohort("cohort".to_owned())
            }
            "off" => RolloutMode::Off,
            "disabled" | "disable" | "kill_switch" | "killswitch" | "false" | "0" => {
                RolloutMode::KillSwitch
            }
            _ if normalized.starts_with("cohort:") && !value[7..].trim().is_empty() => {
                RolloutMode::Cohort(value.to_owned())
            }
            _ => RolloutMode::LegacyDisabled,
        };
    }

    match legacy.map(str::trim) {
        Some("1") => RolloutMode::LegacyEnabled,
        Some(_) => RolloutMode::LegacyDisabled,
        None => match default_policy {
            DefaultPolicy::Enabled => RolloutMode::Enabled,
            DefaultPolicy::Off => RolloutMode::Off,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::{DefaultPolicy, RolloutMode, parse};

    #[test]
    fn parses_enabled_aliases_and_off() {
        for value in ["enabled", "enable", "on", "true", "1"] {
            assert_eq!(
                parse(Some(value), None, DefaultPolicy::Off),
                RolloutMode::Enabled
            );
        }
        assert_eq!(
            parse(Some("off"), None, DefaultPolicy::Enabled),
            RolloutMode::Off
        );
    }

    #[test]
    fn retains_verbatim_named_cohort_and_supports_existing_aliases() {
        let cohort = parse(Some("cohort:canary-Blue"), None, DefaultPolicy::Off);
        assert_eq!(cohort, RolloutMode::Cohort("cohort:canary-Blue".to_owned()));
        assert_eq!(cohort.label(), "cohort:canary-Blue");
        for value in ["cohort", "staging", "rollout", "controlled"] {
            assert_eq!(
                parse(Some(value), None, DefaultPolicy::Off),
                RolloutMode::Cohort("cohort".to_owned())
            );
        }
    }

    #[test]
    fn parses_kill_switch_aliases() {
        for value in [
            "disabled",
            "disable",
            "kill_switch",
            "kill-switch",
            "killswitch",
            "false",
            "0",
        ] {
            assert_eq!(
                parse(Some(value), None, DefaultPolicy::Enabled),
                RolloutMode::KillSwitch
            );
        }
    }

    #[test]
    fn classifies_unknown_and_legacy_disabled_values() {
        assert_eq!(
            parse(Some("unknown"), Some("1"), DefaultPolicy::Off),
            RolloutMode::LegacyDisabled
        );
        assert_eq!(
            parse(None, Some("0"), DefaultPolicy::Off),
            RolloutMode::LegacyDisabled
        );
        assert_eq!(
            parse(None, Some("1"), DefaultPolicy::Off),
            RolloutMode::LegacyEnabled
        );
    }

    #[test]
    fn applies_caller_specific_defaults() {
        assert_eq!(parse(None, None, DefaultPolicy::Off), RolloutMode::Off);
        assert_eq!(
            parse(None, None, DefaultPolicy::Enabled),
            RolloutMode::Enabled
        );
    }

    #[test]
    fn explicit_rollout_precedes_legacy_opt_in() {
        assert_eq!(
            parse(Some("kill-switch"), Some("1"), DefaultPolicy::Off),
            RolloutMode::KillSwitch
        );
    }
}
