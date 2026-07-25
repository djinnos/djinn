//! Per-project build resource overrides + Kubernetes Quantity parsing.
//!
//! `EnvironmentConfig.build_resources` lets a project raise or lower the CPU
//! and memory a task-run or warm Pod is provisioned with, on top of the
//! deployment-wide defaults. Task-run and warm blocks resolve *independently*:
//! neither inherits the other's overrides, and any field left unset inherits
//! its deployment default (see `djinn-k8s` for the resolution + render step).
//!
//! This module owns the shape (`BuildResources` / `BuildResourceOverrides`),
//! the write-time validation (parse, positivity, and per-block
//! request ≤ limit), and an exact-rational [`Quantity`] parser reused by the
//! `djinn-k8s` resolver for the resolution-time bound checks. Values are the
//! same strings the Kubernetes Quantity parser accepts (e.g. `"4"`, `"300m"`,
//! `"8Gi"`); malformed, zero, or negative values are rejected, never clamped.

use std::cmp::Ordering;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::environment::{EnvResult, EnvironmentConfigError};

/// Optional per-project CPU/memory overrides for the task-run and warm Pods.
///
/// The whole object is optional on `EnvironmentConfig`; each inner block is
/// optional; each field within a block is optional. Anything unset inherits
/// the deployment default at resolution time. The two blocks are resolved
/// separately — a `task` override never affects `warm` and vice-versa.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BuildResources {
    /// Overrides applied to the task-run Pod.
    #[serde(default)]
    pub task: Option<BuildResourceOverrides>,
    /// Overrides applied to the warm-job Pod.
    #[serde(default)]
    pub warm: Option<BuildResourceOverrides>,
}

impl BuildResources {
    /// Validate both blocks. Called from [`crate::environment::EnvironmentConfig::validate`]
    /// before any config write.
    pub(crate) fn validate(&self) -> EnvResult<()> {
        if let Some(task) = &self.task {
            task.validate("build_resources.task")?;
        }
        if let Some(warm) = &self.warm {
            warm.validate("build_resources.warm")?;
        }
        Ok(())
    }
}

/// CPU + memory request/limit overrides for one Pod kind. Every field is an
/// optional Kubernetes Quantity string; `None` inherits the deployment default.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BuildResourceOverrides {
    /// CPU request override (e.g. `"4"`, `"500m"`).
    #[serde(default)]
    pub cpu_request: Option<String>,
    /// CPU limit override.
    #[serde(default)]
    pub cpu_limit: Option<String>,
    /// Memory request override (e.g. `"8Gi"`, `"512Mi"`).
    #[serde(default)]
    pub memory_request: Option<String>,
    /// Memory limit override.
    #[serde(default)]
    pub memory_limit: Option<String>,
}

impl BuildResourceOverrides {
    /// Write-time validation: every present field must parse as a positive
    /// Quantity, and where both a request and its limit are present *in this
    /// block* the request must not exceed the limit. Cross-default checks
    /// (request ≤ limit after combining with the deployment default, and admin
    /// min/max bounds) run at resolution time in `djinn-k8s`, which has the
    /// deployment context this module lacks.
    fn validate(&self, field: &str) -> EnvResult<()> {
        let cpu_request = validate_quantity(field, "cpu_request", &self.cpu_request)?;
        let cpu_limit = validate_quantity(field, "cpu_limit", &self.cpu_limit)?;
        let memory_request = validate_quantity(field, "memory_request", &self.memory_request)?;
        let memory_limit = validate_quantity(field, "memory_limit", &self.memory_limit)?;

        if let (Some(req), Some(lim)) = (cpu_request, cpu_limit)
            && req > lim
        {
            return Err(EnvironmentConfigError::RequestExceedsLimit {
                field: format!("{field}.cpu"),
                request: self.cpu_request.clone().unwrap_or_default(),
                limit: self.cpu_limit.clone().unwrap_or_default(),
            });
        }
        if let (Some(req), Some(lim)) = (memory_request, memory_limit)
            && req > lim
        {
            return Err(EnvironmentConfigError::RequestExceedsLimit {
                field: format!("{field}.memory"),
                request: self.memory_request.clone().unwrap_or_default(),
                limit: self.memory_limit.clone().unwrap_or_default(),
            });
        }
        Ok(())
    }
}

/// Parse an optional quantity field, rejecting malformed / zero / negative
/// values. Returns `None` for an unset field (inherits the default).
fn validate_quantity(
    block: &str,
    field: &str,
    value: &Option<String>,
) -> EnvResult<Option<Quantity>> {
    match value {
        None => Ok(None),
        Some(raw) => match Quantity::parse(raw) {
            Some(q) if q.is_positive() => Ok(Some(q)),
            _ => Err(EnvironmentConfigError::InvalidQuantity {
                field: format!("{block}.{field}"),
                value: raw.clone(),
            }),
        },
    }
}

/// A parsed Kubernetes resource Quantity, kept as an exact rational
/// (`value = num / den`, with `den > 0`) so requests, limits, and admin bounds
/// compare without floating-point error across the full CPU (millicores) and
/// memory (bytes) range.
///
/// Accepts the common Kubernetes suffixes: decimal SI (`n`, `u`, `m`, `k`,
/// `M`, `G`, `T`, `P`) and binary SI (`Ki`, `Mi`, `Gi`, `Ti`, `Pi`, `Ei`),
/// plus a bare or decimal number. Scientific notation and the ambiguous `E`
/// (Exa) suffix are intentionally not accepted — no djinn resource quantity
/// uses them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Quantity {
    num: i128,
    den: i128,
}

impl Quantity {
    /// Parse a Kubernetes Quantity string. Returns `None` if the string is not
    /// a syntactically valid quantity. A syntactically valid but non-positive
    /// value (`"0"`, `"-1"`) parses; callers gate on [`Self::is_positive`].
    pub fn parse(input: &str) -> Option<Self> {
        let s = input.trim();
        if s.is_empty() {
            return None;
        }
        // The numeric prefix is the leading run of digits, sign, and decimal
        // point; everything after it is the suffix.
        let split = s
            .find(|c: char| !(c.is_ascii_digit() || c == '.' || c == '+' || c == '-'))
            .unwrap_or(s.len());
        let (num_part, suffix) = s.split_at(split);
        let (mant_num, mant_den) = parse_decimal(num_part)?;
        let (suf_num, suf_den) = suffix_multiplier(suffix)?;
        let num = mant_num.checked_mul(suf_num)?;
        let den = mant_den.checked_mul(suf_den)?;
        if den == 0 {
            return None;
        }
        Some(Self { num, den }.reduced())
    }

    /// Whether the quantity is strictly greater than zero.
    pub fn is_positive(&self) -> bool {
        self.num > 0
    }

    fn reduced(self) -> Self {
        let g = gcd(self.num.abs(), self.den.abs()).max(1);
        Self {
            num: self.num / g,
            den: self.den / g,
        }
    }
}

impl PartialOrd for Quantity {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Quantity {
    fn cmp(&self, other: &Self) -> Ordering {
        // den and other.den are both > 0, so the cross-multiplied comparison
        // preserves ordering. Values stay well within i128.
        (self.num * other.den).cmp(&(other.num * self.den))
    }
}

/// Multiplier for a Kubernetes quantity suffix as an exact `(numerator,
/// denominator)` rational. `None` for an unrecognized suffix.
fn suffix_multiplier(suffix: &str) -> Option<(i128, i128)> {
    Some(match suffix {
        "" => (1, 1),
        "n" => (1, 1_000_000_000),
        "u" => (1, 1_000_000),
        "m" => (1, 1_000),
        "k" => (1_000, 1),
        "M" => (1_000_000, 1),
        "G" => (1_000_000_000, 1),
        "T" => (1_000_000_000_000, 1),
        "P" => (1_000_000_000_000_000, 1),
        "Ki" => (1i128 << 10, 1),
        "Mi" => (1i128 << 20, 1),
        "Gi" => (1i128 << 30, 1),
        "Ti" => (1i128 << 40, 1),
        "Pi" => (1i128 << 50, 1),
        "Ei" => (1i128 << 60, 1),
        _ => return None,
    })
}

/// Parse the numeric prefix (optional sign, digits, optional single decimal
/// point) into an exact `(numerator, denominator)` rational.
fn parse_decimal(s: &str) -> Option<(i128, i128)> {
    let (neg, rest) = match s.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, s.strip_prefix('+').unwrap_or(s)),
    };
    let mut parts = rest.split('.');
    let int_part = parts.next().unwrap_or("");
    let frac_part = parts.next().unwrap_or("");
    if parts.next().is_some() {
        return None; // more than one decimal point
    }
    if int_part.is_empty() && frac_part.is_empty() {
        return None;
    }
    if !int_part.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if !frac_part.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let combined = format!("{int_part}{frac_part}");
    let mut num: i128 = combined.parse().ok()?;
    let den: i128 = 10i128.checked_pow(u32::try_from(frac_part.len()).ok()?)?;
    if neg {
        num = -num;
    }
    Some((num, den))
}

fn gcd(mut a: i128, mut b: i128) -> i128 {
    while b != 0 {
        let t = b;
        b = a % b;
        a = t;
    }
    a
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(s: &str) -> Quantity {
        Quantity::parse(s).unwrap_or_else(|| panic!("{s} should parse"))
    }

    #[test]
    fn parses_plain_millis_and_binary_si() {
        assert!(q("4") == q("4000m"));
        assert!(q("1") == q("1000m"));
        assert!(q("1.5") > q("1"));
        assert!(q("300m") < q("1"));
        assert!(q("8Gi") > q("4Gi"));
        assert!(q("1Gi") == q("1024Mi"));
        assert!(q("1Mi") == q("1024Ki"));
        assert!(q("2Gi") > q("2G")); // binary > decimal
    }

    #[test]
    fn rejects_malformed() {
        for bad in [
            "", "  ", "abc", "4x", "1.2.3", "Gi", "m", "4 Gi", "1e3", "4E",
        ] {
            assert!(Quantity::parse(bad).is_none(), "{bad} must not parse");
        }
    }

    #[test]
    fn parses_but_flags_non_positive() {
        assert!(!q("0").is_positive());
        assert!(!q("0Gi").is_positive());
        assert!(!q("-1").is_positive());
        assert!(q("1m").is_positive());
    }

    #[test]
    fn validate_accepts_unset_and_valid() {
        let ov = BuildResourceOverrides {
            cpu_request: Some("2".into()),
            cpu_limit: Some("4".into()),
            memory_request: None,
            memory_limit: Some("8Gi".into()),
        };
        ov.validate("build_resources.task")
            .expect("valid overrides");
        BuildResourceOverrides::default()
            .validate("build_resources.warm")
            .expect("empty overrides valid");
    }

    #[test]
    fn validate_rejects_malformed_and_request_above_limit() {
        let malformed = BuildResourceOverrides {
            cpu_request: Some("nonsense".into()),
            ..Default::default()
        };
        assert!(matches!(
            malformed.validate("build_resources.task"),
            Err(EnvironmentConfigError::InvalidQuantity { .. })
        ));

        let zero = BuildResourceOverrides {
            memory_limit: Some("0".into()),
            ..Default::default()
        };
        assert!(matches!(
            zero.validate("build_resources.task"),
            Err(EnvironmentConfigError::InvalidQuantity { .. })
        ));

        let inverted = BuildResourceOverrides {
            cpu_request: Some("4".into()),
            cpu_limit: Some("2".into()),
            ..Default::default()
        };
        assert!(matches!(
            inverted.validate("build_resources.task"),
            Err(EnvironmentConfigError::RequestExceedsLimit { .. })
        ));
    }
}
