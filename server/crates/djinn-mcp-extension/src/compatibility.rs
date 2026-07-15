//! Pure, prospective MCP compatibility normalization and lifecycle validation.
//!
//! The production registry is intentionally empty.  Registries are supplied by
//! Djinn code (or tests) only; no request, project, or extension text is ever
//! accepted as a source of traps or remedies.

use std::collections::{BTreeSet, HashSet};
use std::fmt;
use std::str::FromStr;

use djinn_core::tool_call::{
    CompatibilityCode, CompatibilityMetadata, DJINN_TOOL_CALL_METADATA_SCHEMA_VERSION,
    InvalidCompatReason, SurfaceKind, ToolCallErrorCode, ToolCallFailure, TrustedRemedy,
    TrustedRemedyCode,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Server release version. Parsing is deliberately strict `major.minor.patch`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ServerReleaseVersion {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

impl fmt::Display for ServerReleaseVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

impl FromStr for ServerReleaseVersion {
    type Err = String;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let mut pieces = value.split('.');
        let parse = |part: Option<&str>| -> Result<u32, String> {
            let part = part.ok_or_else(|| "version must be major.minor.patch".to_string())?;
            if part.is_empty() || (part.len() > 1 && part.starts_with('0')) {
                return Err("version components must be canonical decimal integers".to_string());
            }
            part.parse()
                .map_err(|_| "version components must be u32".to_string())
        };
        let version = Self {
            major: parse(pieces.next())?,
            minor: parse(pieces.next())?,
            patch: parse(pieces.next())?,
        };
        if pieces.next().is_some() {
            return Err("version must be major.minor.patch".to_string());
        }
        Ok(version)
    }
}

impl Serialize for ServerReleaseVersion {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}
impl<'de> Deserialize<'de> for ServerReleaseVersion {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

/// Strict calendar date (`YYYY-MM-DD`) used only with checked-in release data.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct CivilDate {
    year: i32,
    month: u8,
    day: u8,
}
impl CivilDate {
    pub fn days_after(&self, days: i64) -> Self {
        let ordinal = days_from_civil(self.year, self.month, self.day) + days;
        let (year, month, day) = civil_from_days(ordinal);
        Self { year, month, day }
    }
}
impl fmt::Display for CivilDate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:04}-{:02}-{:02}", self.year, self.month, self.day)
    }
}
impl FromStr for CivilDate {
    type Err = String;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.len() != 10 || &value[4..5] != "-" || &value[7..8] != "-" {
            return Err("date must be YYYY-MM-DD".into());
        }
        let year = value[..4].parse().map_err(|_| "invalid date year")?;
        let month = value[5..7].parse().map_err(|_| "invalid date month")?;
        let day = value[8..10].parse().map_err(|_| "invalid date day")?;
        if !(1..=12).contains(&month) || day == 0 || day > month_days(year, month) {
            return Err("invalid calendar date".into());
        }
        Ok(Self { year, month, day })
    }
}
impl Serialize for CivilDate {
    fn serialize<S>(&self, s: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        s.serialize_str(&self.to_string())
    }
}
impl<'de> Deserialize<'de> for CivilDate {
    fn deserialize<D>(d: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        String::deserialize(d)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}
fn leap(y: i32) -> bool {
    y % 4 == 0 && (y % 100 != 0 || y % 400 == 0)
}
fn month_days(y: i32, m: u8) -> u8 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap(y) => 29,
        2 => 28,
        _ => 0,
    }
}
// Howard Hinnant's civil calendar conversion, epoch 1970-01-01.
fn days_from_civil(y: i32, m: u8, d: u8) -> i64 {
    let y = y - i32::from(m <= 2);
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = i32::from(m) + if m > 2 { -3 } else { 9 };
    let doy = (153 * mp + 2) / 5 + i32::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    i64::from(era * 146097 + doe - 719468)
}
fn civil_from_days(z: i64) -> (i32, u8, u8) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i32 + era as i32 * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    (
        y + if mp < 10 { 0 } else { 1 },
        (mp + if mp < 10 { 3 } else { -9 }) as u8,
        (doy - (153 * mp + 2) / 5 + 1) as u8,
    )
}

/// A server-owned calendar entry; this is never derived from an extension crate version.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerRelease {
    pub version: ServerReleaseVersion,
    pub released_on: CivilDate,
    pub kind: ReleaseKind,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReleaseKind {
    Minor,
    Patch,
}

/// Injected server identity and checked-in release calendar.
#[derive(Clone, Debug)]
pub struct ReleaseCalendar {
    pub current: ServerReleaseVersion,
    pub releases: Vec<ServerRelease>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReleaseNoteOwner {
    McpApi,
    Server,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReleaseNoteRef {
    pub owner: ReleaseNoteOwner,
    pub reference: &'static str,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AtomicDeletionBundle {
    pub change_id: &'static str,
    pub trap_id: &'static str,
    pub fixture_case_ids: &'static [&'static str],
    pub release_note_reference: &'static str,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrapLifecycle {
    pub id: &'static str,
    pub introduced_in: ServerReleaseVersion,
    pub remove_after: ServerReleaseVersion,
    pub release_note: ReleaseNoteRef,
    pub deletion: AtomicDeletionBundle,
}

/// All invariants required before a tool alias may forward.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolForwardingSafety {
    Exact,
    Reject,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParameterMappingSafety {
    SameJsonValueNoConversion,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RemovedParameterBehavior {
    SafeIgnore,
    Reject,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RenamedToolTrap {
    pub old_name: &'static str,
    pub replacement_tool: &'static str,
    pub semantic_safety: ToolForwardingSafety,
    pub lifecycle: TrapLifecycle,
    pub remedy: TrustedRemedyCode,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemovedToolTrap {
    pub old_name: &'static str,
    pub replacement_tool: Option<&'static str>,
    pub lifecycle: TrapLifecycle,
    pub remedy: TrustedRemedyCode,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RenamedParameterTrap {
    pub tool: &'static str,
    pub old_name: &'static str,
    pub replacement_parameter: &'static str,
    pub semantic_safety: ParameterMappingSafety,
    pub lifecycle: TrapLifecycle,
    pub remedy: TrustedRemedyCode,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemovedParameterTrap {
    pub tool: &'static str,
    pub old_name: &'static str,
    pub behavior: RemovedParameterBehavior,
    pub lifecycle: TrapLifecycle,
    pub remedy: TrustedRemedyCode,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CompatibilityTrap {
    RenamedTool(RenamedToolTrap),
    RemovedTool(RemovedToolTrap),
    RenamedParameter(RenamedParameterTrap),
    RemovedParameter(RemovedParameterTrap),
}

/// Production has no retroactive aliases or obsolete parameter behavior.
pub const PRODUCTION_REGISTRY: &[CompatibilityTrap] = &[];

/// Current advertised inventory injected into validation; obsolete surfaces are never added to it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CurrentToolSurface {
    pub name: String,
    pub parameters: BTreeSet<String>,
}

/// A call normalized without any dispatch or project access.
#[derive(Clone, Debug, PartialEq)]
pub struct PreparedToolCall {
    pub name: String,
    pub arguments: Option<Map<String, Value>>,
    pub compatibility_warnings: Vec<CompatibilityMetadata>,
}
#[derive(Clone, Debug, PartialEq)]
pub enum NormalizationResult {
    Prepared(PreparedToolCall),
    Failure(ToolCallFailure),
}

/// Whether a trap is still retained for the injected server release.
///
/// This deliberately compares server identities only; MCP client negotiation
/// never participates in compatibility retention.
pub fn trap_applies(lifecycle: &TrapLifecycle, current: &ServerReleaseVersion) -> bool {
    current < &lifecycle.remove_after
}

/// Apply active, Djinn-authored traps in deterministic order before schema validation.
pub fn normalize_call(
    registry: &[CompatibilityTrap],
    current: &ServerReleaseVersion,
    name: &str,
    arguments: Option<Map<String, Value>>,
) -> NormalizationResult {
    let mut name = name.to_owned();
    let mut arguments = arguments;
    let mut warnings = Vec::new();
    for trap in registry {
        match trap {
            CompatibilityTrap::RenamedTool(t)
                if trap_applies(&t.lifecycle, current) && name == t.old_name =>
            {
                if t.semantic_safety == ToolForwardingSafety::Reject {
                    return failure(
                        metadata(
                            &t.lifecycle,
                            CompatibilityCode::InvalidCompatCall,
                            SurfaceKind::Tool,
                            t.old_name,
                            t.old_name,
                            Some(t.replacement_tool),
                            None,
                            t.remedy,
                            Some(InvalidCompatReason::UnsafeForwarding),
                        ),
                        ToolCallErrorCode::InvalidCompatCall,
                        "renamed tool cannot be safely forwarded",
                    );
                }
                warnings.push(metadata(
                    &t.lifecycle,
                    CompatibilityCode::DeprecatedSurface,
                    SurfaceKind::Tool,
                    t.old_name,
                    t.replacement_tool,
                    Some(t.replacement_tool),
                    None,
                    t.remedy,
                    None,
                ));
                name = t.replacement_tool.to_owned();
            }
            CompatibilityTrap::RemovedTool(t)
                if trap_applies(&t.lifecycle, current) && name == t.old_name =>
            {
                return failure(
                    metadata(
                        &t.lifecycle,
                        CompatibilityCode::RemovedSurface,
                        SurfaceKind::Tool,
                        t.old_name,
                        t.replacement_tool.unwrap_or(t.old_name),
                        t.replacement_tool,
                        None,
                        t.remedy,
                        None,
                    ),
                    ToolCallErrorCode::RemovedSurface,
                    "tool surface has been removed",
                );
            }
            _ => {}
        }
    }
    let mut parameter_traps: Vec<&CompatibilityTrap> = registry.iter().filter(|t| matches!(t, CompatibilityTrap::RenamedParameter(p) if trap_applies(&p.lifecycle, current) && p.tool == name) || matches!(t, CompatibilityTrap::RemovedParameter(p) if trap_applies(&p.lifecycle, current) && p.tool == name)).collect();
    parameter_traps.sort_by_key(|t| match t {
        CompatibilityTrap::RenamedParameter(p) => p.old_name,
        CompatibilityTrap::RemovedParameter(p) => p.old_name,
        _ => unreachable!(),
    });
    for trap in parameter_traps {
        let Some(args) = arguments.as_mut() else {
            continue;
        };
        match trap {
            CompatibilityTrap::RenamedParameter(t) if args.contains_key(t.old_name) => {
                if args.contains_key(t.replacement_parameter) {
                    return failure(
                        metadata(
                            &t.lifecycle,
                            CompatibilityCode::InvalidCompatCall,
                            SurfaceKind::Parameter,
                            t.old_name,
                            t.tool,
                            None,
                            Some(t.replacement_parameter),
                            t.remedy,
                            Some(InvalidCompatReason::AmbiguousParameter),
                        ),
                        ToolCallErrorCode::InvalidCompatCall,
                        "both obsolete and replacement parameters were supplied",
                    );
                }
                let value = args.remove(t.old_name).expect("checked key exists");
                args.insert(t.replacement_parameter.to_owned(), value);
                warnings.push(metadata(
                    &t.lifecycle,
                    CompatibilityCode::DeprecatedSurface,
                    SurfaceKind::Parameter,
                    t.old_name,
                    t.tool,
                    None,
                    Some(t.replacement_parameter),
                    t.remedy,
                    None,
                ));
            }
            CompatibilityTrap::RemovedParameter(t) if args.contains_key(t.old_name) => {
                match t.behavior {
                    RemovedParameterBehavior::SafeIgnore => {
                        args.remove(t.old_name);
                        warnings.push(metadata(
                            &t.lifecycle,
                            CompatibilityCode::DeprecatedSurface,
                            SurfaceKind::Parameter,
                            t.old_name,
                            t.tool,
                            None,
                            None,
                            t.remedy,
                            None,
                        ));
                    }
                    RemovedParameterBehavior::Reject => {
                        return failure(
                            metadata(
                                &t.lifecycle,
                                CompatibilityCode::InvalidCompatCall,
                                SurfaceKind::Parameter,
                                t.old_name,
                                t.tool,
                                None,
                                None,
                                t.remedy,
                                Some(InvalidCompatReason::UnsafeOmission),
                            ),
                            ToolCallErrorCode::InvalidCompatCall,
                            "removed parameter cannot be safely ignored",
                        );
                    }
                }
            }
            _ => {}
        }
    }
    NormalizationResult::Prepared(PreparedToolCall {
        name,
        arguments,
        compatibility_warnings: warnings,
    })
}
#[allow(clippy::too_many_arguments)]
fn metadata(
    l: &TrapLifecycle,
    code: CompatibilityCode,
    surface_kind: SurfaceKind,
    old_name: &str,
    tool: &str,
    replacement_tool: Option<&str>,
    replacement_parameter: Option<&str>,
    remedy: TrustedRemedyCode,
    reason: Option<InvalidCompatReason>,
) -> CompatibilityMetadata {
    CompatibilityMetadata {
        schema_version: DJINN_TOOL_CALL_METADATA_SCHEMA_VERSION,
        code,
        surface_kind,
        old_name: old_name.to_owned(),
        tool: tool.to_owned(),
        replacement_tool: replacement_tool.map(str::to_owned),
        replacement_parameter: replacement_parameter.map(str::to_owned),
        introduced_in: l.introduced_in.to_string(),
        remove_after: l.remove_after.to_string(),
        remedy: TrustedRemedy::new(remedy),
        reason,
    }
}
fn failure(
    data: CompatibilityMetadata,
    code: ToolCallErrorCode,
    message: &str,
) -> NormalizationResult {
    NormalizationResult::Failure(ToolCallFailure::Structured {
        code,
        message: message.to_owned(),
        data,
    })
}

/// Validate static registry invariants and release retirement policy.
pub fn validate_registry(
    registry: &[CompatibilityTrap],
    current_surface: &[CurrentToolSurface],
    calendar: &ReleaseCalendar,
) -> Result<(), String> {
    let versions: HashSet<_> = calendar
        .releases
        .iter()
        .map(|r| r.version.clone())
        .collect();
    if !versions.contains(&calendar.current) {
        return Err("current server release is absent from release calendar".into());
    }
    let names: BTreeSet<_> = current_surface.iter().map(|s| s.name.as_str()).collect();
    let mut ids = HashSet::new();
    let mut keys = HashSet::new();
    let mut cases = HashSet::new();
    for trap in registry {
        let (l, old, tool, replacement_tool, replacement_parameter) = match trap {
            CompatibilityTrap::RenamedTool(t) => (
                &t.lifecycle,
                t.old_name,
                t.old_name,
                Some(t.replacement_tool),
                None,
            ),
            CompatibilityTrap::RemovedTool(t) => (
                &t.lifecycle,
                t.old_name,
                t.old_name,
                t.replacement_tool,
                None,
            ),
            CompatibilityTrap::RenamedParameter(t) => (
                &t.lifecycle,
                t.old_name,
                t.tool,
                None,
                Some(t.replacement_parameter),
            ),
            CompatibilityTrap::RemovedParameter(t) => {
                (&t.lifecycle, t.old_name, t.tool, None, None)
            }
        };
        if !ids.insert(l.id) || !keys.insert((tool, old)) {
            return Err("duplicate trap id or surface key".into());
        }
        if l.id.is_empty()
            || l.release_note.reference.is_empty()
            || l.deletion.change_id.is_empty()
            || l.deletion.trap_id != l.id
            || l.deletion.release_note_reference != l.release_note.reference
            || l.deletion.fixture_case_ids.is_empty()
        {
            return Err("invalid release-note or deletion bundle".into());
        }
        for case in l.deletion.fixture_case_ids {
            if !cases.insert(*case) {
                return Err("duplicate fixture case id".into());
            }
        }
        if !versions.contains(&l.introduced_in) || !versions.contains(&l.remove_after) {
            return Err("lifecycle versions must be server releases".into());
        }
        if names.contains(old) {
            return Err("obsolete surface is advertised".into());
        }
        if let Some(replacement) = replacement_tool
            && !names.contains(replacement)
        {
            return Err("replacement tool is not current".into());
        }
        if let Some(parameter) = replacement_parameter {
            let surface = current_surface
                .iter()
                .find(|s| s.name == tool)
                .ok_or("parameter tool is not current")?;
            if !surface.parameters.contains(parameter) {
                return Err("replacement parameter is not current".into());
            }
        }
        validate_retirement(l, &calendar.releases)?;
    }
    Ok(())
}
fn validate_retirement(l: &TrapLifecycle, releases: &[ServerRelease]) -> Result<(), String> {
    let introduced = releases
        .iter()
        .find(|r| r.version == l.introduced_in)
        .ok_or("unknown introduction release")?;
    let threshold_minor = releases
        .iter()
        .find(|r| {
            r.kind == ReleaseKind::Minor
                && r.version.major == l.introduced_in.major
                && r.version.minor >= l.introduced_in.minor + 2
        })
        .ok_or("calendar lacks two-minor threshold")?;
    let date = introduced.released_on.days_after(90);
    let expected = releases
        .iter()
        .filter(|r| {
            r.kind == ReleaseKind::Minor
                && r.version > threshold_minor.version
                && r.released_on >= date
        })
        .min_by_key(|r| &r.version)
        .ok_or("calendar lacks first removable minor")?;
    if l.remove_after != expected.version {
        return Err("remove_after must be first minor after two releases and 90 days".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn v(v: &str) -> ServerReleaseVersion {
        v.parse().unwrap()
    }
    fn lifecycle() -> TrapLifecycle {
        TrapLifecycle {
            id: "x",
            introduced_in: v("1.0.0"),
            remove_after: v("1.3.0"),
            release_note: ReleaseNoteRef {
                owner: ReleaseNoteOwner::McpApi,
                reference: "note",
            },
            deletion: AtomicDeletionBundle {
                change_id: "delete",
                trap_id: "x",
                fixture_case_ids: &["case"],
                release_note_reference: "note",
            },
        }
    }
    #[test]
    fn normalizes_parameter_without_conversion_and_rejects_ambiguity() {
        let trap = CompatibilityTrap::RenamedParameter(RenamedParameterTrap {
            tool: "new",
            old_name: "old",
            replacement_parameter: "new_arg",
            semantic_safety: ParameterMappingSafety::SameJsonValueNoConversion,
            lifecycle: lifecycle(),
            remedy: TrustedRemedyCode::UseReplacementParameter,
        });
        let mut args = Map::new();
        args.insert("old".into(), Value::from(7));
        let NormalizationResult::Prepared(call) =
            normalize_call(&[trap.clone()], &v("1.1.0"), "new", Some(args))
        else {
            panic!()
        };
        assert_eq!(call.arguments.unwrap()["new_arg"], 7);
        let mut both = Map::new();
        both.insert("old".into(), Value::from(7));
        both.insert("new_arg".into(), Value::from(7));
        assert!(matches!(
            normalize_call(&[trap], &v("1.1.0"), "new", Some(both)),
            NormalizationResult::Failure(_)
        ));
    }
}
