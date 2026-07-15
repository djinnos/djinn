//! Checked-in, synthetic compatibility contract. Production never reads this fixture.

use djinn_core::tool_call::{ToolCallFailure, TrustedRemedyCode};
use djinn_mcp_extension::compatibility::{
    AtomicDeletionBundle, CompatibilityTrap, CurrentToolSurface, ParameterMappingSafety,
    ReleaseCalendar, ReleaseKind, ReleaseNoteOwner, ReleaseNoteRef, RemovedParameterBehavior,
    RemovedParameterTrap, RemovedToolTrap, RenamedParameterTrap, RenamedToolTrap, ServerRelease,
    ServerReleaseVersion, ToolForwardingSafety, TrapLifecycle, normalize_call, trap_applies,
    validate_registry,
};
use serde::Deserialize;
use serde_json::{Map, Value, json};

const FIXTURE: &str = include_str!("fixtures/compatibility_traps.json");

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Fixture {
    current_release: ServerReleaseVersion,
    calendar: Vec<ServerRelease>,
    fixture_case_ids: Vec<String>,
    current_surfaces: Vec<FixtureSurface>,
    traps: Vec<FixtureTrap>,
    synthetic_expected_metadata: SyntheticExpectedMetadata,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct SyntheticExpectedMetadata {
    unsafe_removed_parameter: Value,
    unsafe_renamed_tool: Value,
    removed_tool_without_replacement: Value,
    agent_local_warning_envelope: Value,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct FixtureSurface {
    name: String,
    parameters: Vec<String>,
}

#[derive(Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum FixtureTrap {
    RenamedTool {
        id: String,
        old_name: String,
        replacement_tool: String,
        introduced_in: ServerReleaseVersion,
        remove_after: ServerReleaseVersion,
        release_note: FixtureNote,
        deletion: FixtureDeletion,
        remedy: TrustedRemedyCode,
        expected_metadata: Value,
    },
    RemovedTool {
        id: String,
        old_name: String,
        replacement_tool: Option<String>,
        introduced_in: ServerReleaseVersion,
        remove_after: ServerReleaseVersion,
        release_note: FixtureNote,
        deletion: FixtureDeletion,
        remedy: TrustedRemedyCode,
        expected_metadata: Value,
    },
    RenamedParameter {
        id: String,
        tool: String,
        old_name: String,
        replacement_parameter: String,
        introduced_in: ServerReleaseVersion,
        remove_after: ServerReleaseVersion,
        release_note: FixtureNote,
        deletion: FixtureDeletion,
        remedy: TrustedRemedyCode,
        expected_metadata: Value,
    },
    RemovedParameter {
        id: String,
        tool: String,
        old_name: String,
        behavior: RemovedParameterBehaviorFixture,
        introduced_in: ServerReleaseVersion,
        remove_after: ServerReleaseVersion,
        release_note: FixtureNote,
        deletion: FixtureDeletion,
        remedy: TrustedRemedyCode,
        expected_metadata: Value,
    },
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct FixtureNote {
    owner: ReleaseNoteOwner,
    reference: String,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct FixtureDeletion {
    change_id: String,
    trap_id: String,
    fixture_case_ids: Vec<String>,
    release_note_reference: String,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum RemovedParameterBehaviorFixture {
    SafeIgnore,
    Reject,
}

fn leak(value: String) -> &'static str {
    Box::leak(value.into_boxed_str())
}

fn lifecycle(
    id: String,
    introduced_in: ServerReleaseVersion,
    remove_after: ServerReleaseVersion,
    note: FixtureNote,
    deletion: FixtureDeletion,
) -> TrapLifecycle {
    let case_ids = deletion
        .fixture_case_ids
        .into_iter()
        .map(leak)
        .collect::<Vec<_>>();
    TrapLifecycle {
        id: leak(id),
        introduced_in,
        remove_after,
        release_note: ReleaseNoteRef {
            owner: note.owner,
            reference: leak(note.reference),
        },
        deletion: AtomicDeletionBundle {
            change_id: leak(deletion.change_id),
            trap_id: leak(deletion.trap_id),
            fixture_case_ids: Box::leak(case_ids.into_boxed_slice()),
            release_note_reference: leak(deletion.release_note_reference),
        },
    }
}

fn registry(
    fixture: Fixture,
) -> (
    Vec<CompatibilityTrap>,
    Vec<CurrentToolSurface>,
    ReleaseCalendar,
) {
    let surfaces = fixture
        .current_surfaces
        .into_iter()
        .map(|surface| CurrentToolSurface {
            name: surface.name,
            parameters: surface.parameters.into_iter().collect(),
        })
        .collect();
    let traps = fixture
        .traps
        .into_iter()
        .map(|trap| match trap {
            FixtureTrap::RenamedTool {
                id,
                old_name,
                replacement_tool,
                introduced_in,
                remove_after,
                release_note,
                deletion,
                remedy,
                ..
            } => CompatibilityTrap::RenamedTool(RenamedToolTrap {
                old_name: leak(old_name),
                replacement_tool: leak(replacement_tool),
                semantic_safety: ToolForwardingSafety::Exact,
                lifecycle: lifecycle(id, introduced_in, remove_after, release_note, deletion),
                remedy,
            }),
            FixtureTrap::RemovedTool {
                id,
                old_name,
                replacement_tool,
                introduced_in,
                remove_after,
                release_note,
                deletion,
                remedy,
                ..
            } => CompatibilityTrap::RemovedTool(RemovedToolTrap {
                old_name: leak(old_name),
                replacement_tool: replacement_tool.map(leak),
                lifecycle: lifecycle(id, introduced_in, remove_after, release_note, deletion),
                remedy,
            }),
            FixtureTrap::RenamedParameter {
                id,
                tool,
                old_name,
                replacement_parameter,
                introduced_in,
                remove_after,
                release_note,
                deletion,
                remedy,
                ..
            } => CompatibilityTrap::RenamedParameter(RenamedParameterTrap {
                tool: leak(tool),
                old_name: leak(old_name),
                replacement_parameter: leak(replacement_parameter),
                semantic_safety: ParameterMappingSafety::SameJsonValueNoConversion,
                lifecycle: lifecycle(id, introduced_in, remove_after, release_note, deletion),
                remedy,
            }),
            FixtureTrap::RemovedParameter {
                id,
                tool,
                old_name,
                behavior,
                introduced_in,
                remove_after,
                release_note,
                deletion,
                remedy,
                ..
            } => CompatibilityTrap::RemovedParameter(RemovedParameterTrap {
                tool: leak(tool),
                old_name: leak(old_name),
                behavior: match behavior {
                    RemovedParameterBehaviorFixture::SafeIgnore => {
                        RemovedParameterBehavior::SafeIgnore
                    }
                    RemovedParameterBehaviorFixture::Reject => RemovedParameterBehavior::Reject,
                },
                lifecycle: lifecycle(id, introduced_in, remove_after, release_note, deletion),
                remedy,
            }),
        })
        .collect();
    (
        traps,
        surfaces,
        ReleaseCalendar {
            current: fixture.current_release,
            releases: fixture.calendar,
        },
    )
}

fn args(value: Value) -> Option<Map<String, Value>> {
    value.as_object().cloned()
}

fn metadata(failure: djinn_core::tool_call::ToolCallFailure) -> Value {
    match failure {
        ToolCallFailure::Structured { data, .. } => {
            serde_json::to_value(data).expect("metadata JSON")
        }
        other => panic!("expected structured compatibility failure, got {other:?}"),
    }
}

fn validate_fixture(fixture: &Fixture) -> Result<(), String> {
    let declared = fixture
        .fixture_case_ids
        .iter()
        .collect::<std::collections::BTreeSet<_>>();
    if declared.len() != fixture.fixture_case_ids.len() {
        return Err("duplicate declared fixture case id".into());
    }
    for trap in &fixture.traps {
        let deletion = match trap {
            FixtureTrap::RenamedTool { deletion, .. }
            | FixtureTrap::RemovedTool { deletion, .. }
            | FixtureTrap::RenamedParameter { deletion, .. }
            | FixtureTrap::RemovedParameter { deletion, .. } => deletion,
        };
        if deletion
            .fixture_case_ids
            .iter()
            .any(|id| !declared.contains(id))
        {
            return Err("deletion bundle references unknown fixture case".into());
        }
    }
    Ok(())
}

fn lifecycle_mut(trap: &mut CompatibilityTrap) -> &mut TrapLifecycle {
    match trap {
        CompatibilityTrap::RenamedTool(t) => &mut t.lifecycle,
        CompatibilityTrap::RemovedTool(t) => &mut t.lifecycle,
        CompatibilityTrap::RenamedParameter(t) => &mut t.lifecycle,
        CompatibilityTrap::RemovedParameter(t) => &mut t.lifecycle,
    }
}

fn remedy_mut(trap: &mut CompatibilityTrap) -> &mut TrustedRemedyCode {
    match trap {
        CompatibilityTrap::RenamedTool(t) => &mut t.remedy,
        CompatibilityTrap::RemovedTool(t) => &mut t.remedy,
        CompatibilityTrap::RenamedParameter(t) => &mut t.remedy,
        CompatibilityTrap::RemovedParameter(t) => &mut t.remedy,
    }
}

/// The requested selector intentionally resolves this single root contract test.
#[test]
fn compatibility_traps() {
    assert!(djinn_mcp_extension::compatibility::PRODUCTION_REGISTRY.is_empty());
    let fixture: Fixture = serde_json::from_str(FIXTURE).expect("strict checked-in fixture");
    validate_fixture(&fixture).expect("all deletion fixture cases resolve");
    assert_eq!(
        fixture
            .synthetic_expected_metadata
            .agent_local_warning_envelope["warnings"]
            .as_array()
            .map(Vec::len),
        Some(1)
    );
    let expected = fixture
        .traps
        .iter()
        .map(|trap| match trap {
            FixtureTrap::RenamedTool {
                id,
                expected_metadata,
                ..
            }
            | FixtureTrap::RemovedTool {
                id,
                expected_metadata,
                ..
            }
            | FixtureTrap::RenamedParameter {
                id,
                expected_metadata,
                ..
            }
            | FixtureTrap::RemovedParameter {
                id,
                expected_metadata,
                ..
            } => (id.as_str(), expected_metadata.clone()),
        })
        .collect::<std::collections::BTreeMap<_, _>>();
    let (registry, surfaces, calendar) = registry(fixture.clone());
    validate_registry(&registry, &surfaces, &calendar)
        .expect("fixture lifecycle and deletion bundles");

    // Fresh discovery derives only current schemas; neither synthetic obsolete
    // names nor parameter keys may be advertised, while all replacements exist.
    let inventory =
        djinn_mcp_extension::tool_surface::collect_tool_surface_union().expect("inventory");
    for trap in &registry {
        match trap {
            CompatibilityTrap::RenamedTool(t) => {
                assert!(!inventory.iter().any(|v| v["name"] == t.old_name));
                assert!(inventory.iter().any(|v| v["name"] == t.replacement_tool));
            }
            CompatibilityTrap::RemovedTool(t) => {
                assert!(!inventory.iter().any(|v| v["name"] == t.old_name));
                if let Some(replacement) = t.replacement_tool {
                    assert!(inventory.iter().any(|v| v["name"] == replacement));
                }
            }
            CompatibilityTrap::RenamedParameter(t) => {
                let schema = inventory
                    .iter()
                    .find(|v| v["name"] == t.tool)
                    .expect("replacement tool");
                assert!(
                    schema["inputSchema"]["properties"]
                        .get(t.old_name)
                        .is_none()
                );
                assert!(
                    schema["inputSchema"]["properties"]
                        .get(t.replacement_parameter)
                        .is_some()
                );
            }
            CompatibilityTrap::RemovedParameter(t) => {
                let schema = inventory
                    .iter()
                    .find(|v| v["name"] == t.tool)
                    .expect("current tool");
                assert!(
                    schema["inputSchema"]["properties"]
                        .get(t.old_name)
                        .is_none()
                );
            }
        }
    }

    let current = &calendar.current;
    let safe = normalize_call(
        &registry,
        current,
        "cached_task_show",
        args(json!({"id":"x"})),
    );
    let djinn_mcp_extension::compatibility::NormalizationResult::Prepared(safe) = safe else {
        panic!("safe renamed tool");
    };
    assert_eq!(safe.name, "task_show");
    assert_eq!(safe.arguments, args(json!({"id":"x"})));
    assert_eq!(
        serde_json::to_value(&safe.compatibility_warnings[0]).unwrap(),
        expected["safe-task-show-rename"]
    );

    let removed = normalize_call(&registry, current, "cached_task_list", None);
    let djinn_mcp_extension::compatibility::NormalizationResult::Failure(removed) = removed else {
        panic!("removed tool must fail");
    };
    assert_eq!(metadata(removed), expected["removed-task-list"]);

    let renamed = normalize_call(
        &registry,
        current,
        "task_list",
        args(json!({"cached_query":{"nested":[1,2]}})),
    );
    let djinn_mcp_extension::compatibility::NormalizationResult::Prepared(renamed) = renamed else {
        panic!("renamed parameter");
    };
    assert_eq!(renamed.arguments, args(json!({"text":{"nested":[1,2]}})));
    assert_eq!(
        serde_json::to_value(&renamed.compatibility_warnings[0]).unwrap(),
        expected["task-list-query-rename"]
    );
    let ambiguous = normalize_call(
        &registry,
        current,
        "task_list",
        args(json!({"cached_query":"a","text":"b"})),
    );
    let djinn_mcp_extension::compatibility::NormalizationResult::Failure(ambiguous) = ambiguous
    else {
        panic!("ambiguous keys must fail");
    };
    assert_eq!(
        metadata(ambiguous),
        json!({"schema_version":1,"code":"invalid_compat_call","surface_kind":"parameter","old_name":"cached_query","tool":"task_list","replacement_parameter":"text","introduced_in":"1.0.0","remove_after":"1.3.0","remedy":{"code":"use_replacement_parameter","text":"Use the replacement parameter named in replacement_parameter."},"reason":"ambiguous_parameter"})
    );

    let ignored = normalize_call(
        &registry,
        current,
        "task_list",
        args(json!({"cached_hint":true,"limit":3})),
    );
    let djinn_mcp_extension::compatibility::NormalizationResult::Prepared(ignored) = ignored else {
        panic!("safe removed parameter");
    };
    assert_eq!(ignored.arguments, args(json!({"limit":3})));
    assert_eq!(
        serde_json::to_value(&ignored.compatibility_warnings[0]).unwrap(),
        expected["task-list-hint-removal"]
    );

    let unsafe_parameter = CompatibilityTrap::RemovedParameter(RemovedParameterTrap {
        tool: "task_list",
        old_name: "unsafe_cached_hint",
        behavior: RemovedParameterBehavior::Reject,
        lifecycle: match &registry[3] {
            CompatibilityTrap::RemovedParameter(t) => t.lifecycle.clone(),
            _ => unreachable!(),
        },
        remedy: TrustedRemedyCode::OmitRemovedParameter,
    });
    let rejected = normalize_call(
        &[unsafe_parameter],
        current,
        "task_list",
        args(json!({"unsafe_cached_hint":true})),
    );
    let djinn_mcp_extension::compatibility::NormalizationResult::Failure(rejected) = rejected
    else {
        panic!("unsafe omission");
    };
    assert_eq!(
        metadata(rejected),
        fixture
            .synthetic_expected_metadata
            .unsafe_removed_parameter
            .clone()
    );

    let unsafe_tool = CompatibilityTrap::RenamedTool(RenamedToolTrap {
        old_name: "unsafe_cached_tool",
        replacement_tool: "task_show",
        semantic_safety: ToolForwardingSafety::Reject,
        lifecycle: match &registry[0] {
            CompatibilityTrap::RenamedTool(t) => t.lifecycle.clone(),
            _ => unreachable!(),
        },
        remedy: TrustedRemedyCode::CallReplacementTool,
    });
    let unsafe_result = normalize_call(
        &[unsafe_tool],
        current,
        "unsafe_cached_tool",
        args(json!({"unexpected":true})),
    );
    let djinn_mcp_extension::compatibility::NormalizationResult::Failure(unsafe_result) =
        unsafe_result
    else {
        panic!("unsafe forwarding precedes allowlist/schema/handler");
    };
    assert_eq!(
        metadata(unsafe_result),
        fixture
            .synthetic_expected_metadata
            .unsafe_renamed_tool
            .clone()
    );
    let no_replacement = CompatibilityTrap::RemovedTool(RemovedToolTrap {
        old_name: "gone_forever",
        replacement_tool: None,
        lifecycle: match &registry[1] {
            CompatibilityTrap::RemovedTool(t) => t.lifecycle.clone(),
            _ => unreachable!(),
        },
        remedy: TrustedRemedyCode::NoReplacement,
    });
    let djinn_mcp_extension::compatibility::NormalizationResult::Failure(no_replacement_failure) =
        normalize_call(&[no_replacement], current, "gone_forever", None)
    else {
        panic!("no-replacement removal");
    };
    assert_eq!(
        metadata(no_replacement_failure),
        fixture
            .synthetic_expected_metadata
            .removed_tool_without_replacement
            .clone()
    );
    let djinn_mcp_extension::compatibility::NormalizationResult::Prepared(unknown) =
        normalize_call(&[], current, "unknown_current", args(json!({"x":1})))
    else {
        panic!("unknown current calls retain ordinary behavior");
    };
    assert_eq!(unknown.name, "unknown_current");
    assert_eq!(unknown.arguments, args(json!({"x":1})));
    assert!(unknown.compatibility_warnings.is_empty());

    // Retention is lower-inclusive and upper-exclusive; calls before a break
    // retain ordinary behavior rather than being prospectively trapped.
    let lifecycle = match &registry[0] {
        CompatibilityTrap::RenamedTool(t) => &t.lifecycle,
        _ => unreachable!(),
    };
    assert!(!trap_applies(lifecycle, &"0.9.9".parse().unwrap()));
    assert!(trap_applies(lifecycle, &"1.0.0".parse().unwrap()));
    assert!(trap_applies(lifecycle, &"1.0.1".parse().unwrap()));
    assert!(trap_applies(lifecycle, &"1.2.1".parse().unwrap()));
    assert!(!trap_applies(lifecycle, &"1.3.0".parse().unwrap()));
    assert!(!trap_applies(lifecycle, &"1.3.1".parse().unwrap()));
    assert_eq!(calendar.releases[2].released_on.to_string(), "2030-03-31");
    assert_eq!(calendar.releases[3].released_on.to_string(), "2030-04-01");
    let mut unresolved_fixture_case = fixture.clone();
    unresolved_fixture_case.fixture_case_ids.clear();
    assert_eq!(
        validate_fixture(&unresolved_fixture_case),
        Err("deletion bundle references unknown fixture case".into())
    );
    let mut untrusted_remedy = registry.clone();
    *remedy_mut(&mut untrusted_remedy[0]) = TrustedRemedyCode::NoReplacement;
    assert_eq!(
        validate_registry(&untrusted_remedy, &surfaces, &calendar),
        Err("trap remedy is not trusted for its surface".into())
    );
    let mut invalid_owner = registry.clone();
    lifecycle_mut(&mut invalid_owner[0]).release_note.owner = ReleaseNoteOwner::Server;
    assert_eq!(
        validate_registry(&invalid_owner, &surfaces, &calendar),
        Err("release-note owner must be mcp_api".into())
    );
    let mut missing_note = registry.clone();
    lifecycle_mut(&mut missing_note[0]).release_note.reference = "";
    assert_eq!(
        validate_registry(&missing_note, &surfaces, &calendar),
        Err("invalid release-note or deletion bundle".into())
    );
    let mut mismatched_deletion = registry.clone();
    lifecycle_mut(&mut mismatched_deletion[0])
        .deletion
        .release_note_reference = "other-note";
    assert_eq!(
        validate_registry(&mismatched_deletion, &surfaces, &calendar),
        Err("invalid release-note or deletion bundle".into())
    );
    let mut early_removal = registry.clone();
    lifecycle_mut(&mut early_removal[0]).remove_after = "1.2.0".parse().unwrap();
    assert_eq!(
        validate_registry(&early_removal, &surfaces, &calendar),
        Err("remove_after must be first minor after two releases and 90 days".into())
    );
    let mut late_calendar = calendar.clone();
    late_calendar.releases.push(ServerRelease {
        version: "1.4.0".parse().unwrap(),
        released_on: "2030-06-01".parse().unwrap(),
        kind: ReleaseKind::Minor,
    });
    let mut late_removal = registry.clone();
    lifecycle_mut(&mut late_removal[0]).remove_after = "1.4.0".parse().unwrap();
    assert_eq!(
        validate_registry(&late_removal, &surfaces, &late_calendar),
        Err("remove_after must be first minor after two releases and 90 days".into())
    );
    assert!(serde_json::from_str::<Fixture>(r#"{"current_release":"1.1.0","calendar":[],"current_surfaces":[],"traps":[],"unknown":true}"#).is_err());
    assert!(serde_json::from_str::<Fixture>(r#"{"current_release":"1.1.0","calendar":[{"version":"1.0.0","released_on":"2030-01-01","kind":"minor","bad":true}],"current_surfaces":[],"traps":[]}"#).is_err());
}
