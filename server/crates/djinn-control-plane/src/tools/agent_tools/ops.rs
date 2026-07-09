use crate::tools::json_object::AnyJson;
use djinn_core::models::Agent;
use djinn_db::repositories::agent::ExtractionQualityMetrics as DbExtractionQualityMetrics;
use djinn_db::{
    AgentCreateInput, AgentListQuery, AgentMetrics as DbAgentMetrics, AgentRepository,
    VALID_BASE_ROLES,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

// ─── Native skill name protection ───────────────────────────────────────────

/// Canonical list of native (platform-owned) skill name prefixes.
///
/// These names are **immutable** at the platform level: they cannot be added,
/// edited, or removed through the user-editable `skills` field on agent
/// create/update.  Native skill roles are bound through the native registry
/// (see `djinn_agent::native_skills`), not through the mutable agent skills
/// list.
///
/// Keep this list aligned with the native registry in `djinn-agent`.  We use
/// a local allowlist here to avoid introducing a production dependency cycle
/// (`djinn-agent` already depends on `djinn-control-plane`).
const NATIVE_SKILL_NAME_PREFIXES: &[&str] = &["visual-spec"];

/// Returns `true` when `name` matches a native skill name prefix.
///
/// A name is native when it exactly equals or starts with one of the
/// [`NATIVE_SKILL_NAME_PREFIXES`] followed by a hyphen or end-of-string.
/// This covers both exact matches (e.g. `"visual-spec"`) and versioned
/// sub-skills (e.g. `"visual-spec-v2"`) defensively.
pub fn is_native_skill_name(name: &str) -> bool {
    NATIVE_SKILL_NAME_PREFIXES.iter().any(|prefix| {
        name == *prefix
            || (name.starts_with(prefix) && name.as_bytes().get(prefix.len()) == Some(&b'-'))
    })
}

/// Extract a human-readable skill identifier from an [`AnyJson`] entry.
///
/// Skill entries can arrive as bare strings (e.g. `"my-skill"`) or as
/// objects with obvious name keys (`name`, `id`, `skill`, or `skill_name`).
/// Returns `None` if no identifier can be extracted.
fn skill_name_from_entry(entry: &AnyJson) -> Option<String> {
    match &entry.0 {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Object(map) => {
            for key in &["name", "id", "skill", "skill_name"] {
                if let Some(serde_json::Value::String(v)) = map.get(*key) {
                    return Some(v.clone());
                }
            }
            None
        }
        _ => None,
    }
}

/// Reject a skills payload that contains any native skill names.
///
/// Returns `Ok(())` when the payload is clean, or `Err(message)` listing
/// the offending native names.  Call this before persisting `skills` in
/// `agent_create` and `agent_update`.
pub fn reject_native_skill_entries(skills: &[AnyJson]) -> Result<(), String> {
    let offenders: Vec<String> = skills
        .iter()
        .filter_map(|entry| skill_name_from_entry(entry).filter(|name| is_native_skill_name(name)))
        .collect();

    if offenders.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "native/immutable skill names cannot be set through the mutable skills API: {}. \
             Native skills are platform-owned and role-bound through the native registry. \
             They cannot be added, edited, or removed via agent_create or agent_update.",
            offenders
                .iter()
                .map(|n| format!("'{n}'"))
                .collect::<Vec<_>>()
                .join(", ")
        ))
    }
}

/// Filter native skill names out of a skills array.
///
/// Used by [`AgentModel::from`] so that stale persisted native skill names
/// in legacy database rows are hidden from user-editable API responses
/// (`agent_show`, `agent_list`).
pub fn filter_native_skill_entries(skills: Vec<AnyJson>) -> Vec<AnyJson> {
    skills
        .into_iter()
        .filter(|entry| {
            skill_name_from_entry(entry)
                .map(|name| !is_native_skill_name(&name))
                .unwrap_or(true)
        })
        .collect()
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct AgentModel {
    pub id: String,
    pub project_id: String,
    pub name: String,
    pub base_role: String,
    pub description: String,
    pub system_prompt_extensions: String,
    pub model_preference: Option<String>,
    pub mcp_servers: Vec<AnyJson>,
    pub skills: Vec<AnyJson>,
    pub is_default: bool,
    pub created_at: String,
    pub updated_at: String,
}

impl From<&Agent> for AgentModel {
    fn from(r: &Agent) -> Self {
        Self {
            id: r.id.clone(),
            project_id: r.project_id.clone(),
            name: r.name.clone(),
            base_role: r.base_role.clone(),
            description: r.description.clone(),
            system_prompt_extensions: r.system_prompt_extensions.clone(),
            model_preference: r.model_preference.clone(),
            mcp_servers: parse_json_array_any(&r.mcp_servers),
            skills: filter_native_skill_entries(parse_json_array_any(&r.skills)),
            is_default: r.is_default,
            created_at: r.created_at.clone(),
            updated_at: r.updated_at.clone(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct AgentSingleResponse {
    #[serde(flatten, skip_serializing_if = "Option::is_none")]
    pub agent: Option<AgentModel>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct AgentCreateParams {
    /// Absolute project path.
    pub project: String,
    /// Unique agent name within the project.
    pub name: String,
    /// Base role to extend. One of: worker, lead, planner, architect, reviewer.
    pub base_role: String,
    pub description: Option<String>,
    /// Additional system prompt content appended to the base role prompt.
    pub system_prompt_extensions: Option<String>,
    /// Preferred model ID (falls back to project default).
    pub model_preference: Option<String>,
    /// Additional MCP server refs for this agent.
    pub mcp_servers: Option<Vec<AnyJson>>,
    /// Skills (prompt templates) available to this agent.
    pub skills: Option<Vec<AnyJson>>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct AgentMetricEntry {
    pub agent_id: String,
    pub agent_name: String,
    pub base_role: String,
    pub success_rate: f64,
    pub avg_tokens: f64,
    pub avg_tokens_in: f64,
    pub avg_tokens_out: f64,
    pub avg_time_seconds: f64,
    pub avg_reopens: f64,
    pub completed_task_count: i64,
    pub extraction_quality: ExtractionQualityMetricEntry,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct ExtractionQualityMetricEntry {
    pub extracted: i64,
    pub dedup_skipped: i64,
    pub novelty_skipped: i64,
    pub written: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct AgentMetricsResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agents: Option<Vec<AgentMetricEntry>>,
    pub window_days: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct AgentMetricsParams {
    pub project: String,
    pub agent_id: Option<String>,
    pub window_days: Option<i64>,
}

pub fn parse_json_array_any(raw: &str) -> Vec<AnyJson> {
    serde_json::from_str(raw).unwrap_or_default()
}

pub fn agent_not_found_error(id: &str) -> String {
    format!("agent not found: {id}")
}

pub fn validate_base_role(base_role: &str) -> Result<(), String> {
    if VALID_BASE_ROLES.contains(&base_role) {
        Ok(())
    } else {
        Err(format!(
            "invalid base_role '{}'; must be one of: {}",
            base_role,
            VALID_BASE_ROLES.join(", ")
        ))
    }
}

pub fn validate_agent_name(name: &str) -> Result<String, String> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err("name must not be empty".to_string());
    }
    if trimmed.len() > 100 {
        return Err("name must be 100 characters or fewer".to_string());
    }
    Ok(trimmed.to_string())
}

pub async fn create_agent(
    repo: &AgentRepository,
    project_id: &str,
    params: AgentCreateParams,
) -> AgentSingleResponse {
    let name = match validate_agent_name(&params.name) {
        Ok(n) => n,
        Err(e) => {
            return AgentSingleResponse {
                agent: None,
                error: Some(e),
            };
        }
    };

    if let Err(e) = validate_base_role(&params.base_role) {
        return AgentSingleResponse {
            agent: None,
            error: Some(e),
        };
    }

    // Reject native skill names before persisting.
    if let Some(ref skills) = params.skills
        && let Err(e) = reject_native_skill_entries(skills)
    {
        return AgentSingleResponse {
            agent: None,
            error: Some(e),
        };
    }

    match repo.get_by_name_for_project(project_id, &name).await {
        Ok(Some(_)) => {
            return AgentSingleResponse {
                agent: None,
                error: Some(format!(
                    "an agent named '{name}' already exists in this project"
                )),
            };
        }
        Err(e) => {
            return AgentSingleResponse {
                agent: None,
                error: Some(e.to_string()),
            };
        }
        Ok(None) => {}
    }

    let mcp_servers_json = params
        .mcp_servers
        .as_ref()
        .map(|v| serde_json::to_string(v).unwrap_or_else(|_| "[]".to_string()));
    let skills_json = params
        .skills
        .as_ref()
        .map(|v| serde_json::to_string(v).unwrap_or_else(|_| "[]".to_string()));

    match repo
        .create_for_project(
            project_id,
            AgentCreateInput {
                name: &name,
                base_role: &params.base_role,
                description: params.description.as_deref().unwrap_or(""),
                system_prompt_extensions: params.system_prompt_extensions.as_deref().unwrap_or(""),
                model_preference: params.model_preference.as_deref(),
                mcp_servers: mcp_servers_json.as_deref(),
                skills: skills_json.as_deref(),
                is_default: false,
            },
        )
        .await
    {
        Ok(agent) => AgentSingleResponse {
            agent: Some(AgentModel::from(&agent)),
            error: None,
        },
        Err(e) => AgentSingleResponse {
            agent: None,
            error: Some(e.to_string()),
        },
    }
}

fn base_role_to_agent_type(base_role: &str) -> &str {
    match base_role {
        "worker" => "worker",
        "reviewer" => "reviewer",
        "planner" => "planner",
        "lead" => "lead",
        other => other,
    }
}

pub async fn metrics_for_agents(
    repo: &AgentRepository,
    project_id: &str,
    params: AgentMetricsParams,
) -> AgentMetricsResponse {
    let window_days = params.window_days.unwrap_or(30).max(1);
    let agent_id = params.agent_id.filter(|s| !s.trim().is_empty());

    let agents: Vec<Agent> = if let Some(ref id_or_name) = agent_id {
        match resolve_agent(repo, project_id, id_or_name).await {
            Ok(Some(r)) => vec![r],
            Ok(None) => {
                return AgentMetricsResponse {
                    agents: None,
                    window_days,
                    error: Some(agent_not_found_error(id_or_name)),
                };
            }
            Err(e) => {
                return AgentMetricsResponse {
                    agents: None,
                    window_days,
                    error: Some(e),
                };
            }
        }
    } else {
        match repo
            .list_for_project(AgentListQuery {
                project_id: project_id.to_string(),
                base_role: None,
                limit: 200,
                offset: 0,
            })
            .await
        {
            Ok(result) => result.agents,
            Err(e) => {
                return AgentMetricsResponse {
                    agents: None,
                    window_days,
                    error: Some(e.to_string()),
                };
            }
        }
    };

    let mut entries: Vec<AgentMetricEntry> = Vec::with_capacity(agents.len());

    for agent in &agents {
        let agent_type = base_role_to_agent_type(&agent.base_role);
        let m: DbAgentMetrics = repo
            .get_metrics(project_id, agent_type, window_days)
            .await
            .unwrap_or(DbAgentMetrics {
                success_rate: 0.0,
                avg_reopens: 0.0,
                completed_task_count: 0,
                avg_tokens: 0.0,
                avg_tokens_in: 0.0,
                avg_tokens_out: 0.0,
                avg_time_seconds: 0.0,
                extraction_quality: DbExtractionQualityMetrics::default(),
            });

        entries.push(AgentMetricEntry {
            agent_id: agent.id.clone(),
            agent_name: agent.name.clone(),
            base_role: agent.base_role.clone(),
            success_rate: m.success_rate,
            avg_reopens: m.avg_reopens,
            completed_task_count: m.completed_task_count,
            avg_tokens: m.avg_tokens,
            avg_tokens_in: m.avg_tokens_in,
            avg_tokens_out: m.avg_tokens_out,
            avg_time_seconds: m.avg_time_seconds,
            extraction_quality: ExtractionQualityMetricEntry {
                extracted: m.extraction_quality.extracted,
                dedup_skipped: m.extraction_quality.dedup_skipped,
                novelty_skipped: m.extraction_quality.novelty_skipped,
                written: m.extraction_quality.written,
            },
        });
    }

    AgentMetricsResponse {
        agents: Some(entries),
        window_days,
        error: None,
    }
}

pub async fn resolve_agent(
    repo: &AgentRepository,
    project_id: &str,
    id_or_name: &str,
) -> Result<Option<Agent>, String> {
    if let Ok(Some(role)) = repo.get(id_or_name).await
        && role.project_id == project_id
    {
        return Ok(Some(role));
    }

    repo.get_by_name_for_project(project_id, id_or_name)
        .await
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_native_skill_name_matches_visual_spec() {
        assert!(is_native_skill_name("visual-spec"));
    }

    #[test]
    fn is_native_skill_name_matches_versioned_prefix() {
        assert!(is_native_skill_name("visual-spec-v2"));
    }

    #[test]
    fn is_native_skill_name_rejects_non_native() {
        assert!(!is_native_skill_name("my-skill"));
        assert!(!is_native_skill_name("visual")); // prefix only, no hyphen
        assert!(!is_native_skill_name("")); // empty
        assert!(!is_native_skill_name("visual-specification")); // different word
    }

    #[test]
    fn skill_name_from_entry_extracts_string() {
        let entry = AnyJson(serde_json::json!("visual-spec"));
        assert_eq!(
            skill_name_from_entry(&entry),
            Some("visual-spec".to_string())
        );
    }

    #[test]
    fn skill_name_from_entry_extracts_object_name_key() {
        let entry = AnyJson(serde_json::json!({"name": "visual-spec"}));
        assert_eq!(
            skill_name_from_entry(&entry),
            Some("visual-spec".to_string())
        );
    }

    #[test]
    fn skill_name_from_entry_extracts_object_id_key() {
        let entry = AnyJson(serde_json::json!({"id": "visual-spec"}));
        assert_eq!(
            skill_name_from_entry(&entry),
            Some("visual-spec".to_string())
        );
    }

    #[test]
    fn skill_name_from_entry_extracts_object_skill_key() {
        let entry = AnyJson(serde_json::json!({"skill": "visual-spec"}));
        assert_eq!(
            skill_name_from_entry(&entry),
            Some("visual-spec".to_string())
        );
    }

    #[test]
    fn skill_name_from_entry_extracts_object_skill_name_key() {
        let entry = AnyJson(serde_json::json!({"skill_name": "visual-spec"}));
        assert_eq!(
            skill_name_from_entry(&entry),
            Some("visual-spec".to_string())
        );
    }

    #[test]
    fn skill_name_from_entry_returns_none_for_number() {
        let entry = AnyJson(serde_json::json!(42));
        assert_eq!(skill_name_from_entry(&entry), None);
    }

    #[test]
    fn reject_native_skill_entries_allows_non_native() {
        let skills = vec![
            AnyJson(serde_json::json!("my-skill")),
            AnyJson(serde_json::json!("another-skill")),
        ];
        assert!(reject_native_skill_entries(&skills).is_ok());
    }

    #[test]
    fn reject_native_skill_entries_rejects_string_native() {
        let skills = vec![
            AnyJson(serde_json::json!("my-skill")),
            AnyJson(serde_json::json!("visual-spec")),
        ];
        let err = reject_native_skill_entries(&skills).unwrap_err();
        assert!(err.contains("'visual-spec'"));
        assert!(err.contains("native/immutable"));
        assert!(err.contains("platform-owned"));
    }

    #[test]
    fn reject_native_skill_entries_rejects_object_native() {
        let skills = vec![AnyJson(serde_json::json!({"name": "visual-spec"}))];
        let err = reject_native_skill_entries(&skills).unwrap_err();
        assert!(err.contains("'visual-spec'"));
    }

    #[test]
    fn reject_native_skill_entries_rejects_versioned_native() {
        let skills = vec![AnyJson(serde_json::json!("visual-spec-v2"))];
        let err = reject_native_skill_entries(&skills).unwrap_err();
        assert!(err.contains("'visual-spec-v2'"));
    }

    #[test]
    fn reject_native_skill_entries_allows_empty() {
        assert!(reject_native_skill_entries(&[]).is_ok());
    }

    #[test]
    fn filter_native_skill_entries_removes_native() {
        let skills = vec![
            AnyJson(serde_json::json!("my-skill")),
            AnyJson(serde_json::json!("visual-spec")),
            AnyJson(serde_json::json!("another-skill")),
        ];
        let filtered = filter_native_skill_entries(skills);
        assert_eq!(filtered.len(), 2);
        assert_eq!(filtered[0].0, serde_json::json!("my-skill"));
        assert_eq!(filtered[1].0, serde_json::json!("another-skill"));
    }

    #[test]
    fn filter_native_skill_entries_removes_object_native() {
        let skills = vec![
            AnyJson(serde_json::json!({"name": "visual-spec"})),
            AnyJson(serde_json::json!({"name": "good-skill"})),
        ];
        let filtered = filter_native_skill_entries(skills);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].0, serde_json::json!({"name": "good-skill"}));
    }

    #[test]
    fn filter_native_skill_entries_preserves_non_native() {
        let skills = vec![
            AnyJson(serde_json::json!("skill-a")),
            AnyJson(serde_json::json!("skill-b")),
        ];
        let filtered = filter_native_skill_entries(skills.clone());
        assert_eq!(filtered.len(), skills.len());
    }

    #[test]
    fn filter_native_skill_entries_handles_empty() {
        let filtered = filter_native_skill_entries(vec![]);
        assert!(filtered.is_empty());
    }

    #[test]
    fn agent_model_from_filters_native_skills() {
        use djinn_core::models::Agent;

        let agent = Agent {
            id: "test-id".to_string(),
            project_id: "proj".to_string(),
            name: "test-agent".to_string(),
            base_role: "worker".to_string(),
            description: "".to_string(),
            system_prompt_extensions: "".to_string(),
            model_preference: None,
            mcp_servers: "[]".to_string(),
            skills: r#"["my-skill", "visual-spec", "another-skill"]"#.to_string(),
            is_default: false,
            created_at: "2024-01-01".to_string(),
            updated_at: "2024-01-01".to_string(),
        };

        let model = AgentModel::from(&agent);
        assert_eq!(model.skills.len(), 2);
        assert_eq!(model.skills[0].0, serde_json::json!("my-skill"));
        assert_eq!(model.skills[1].0, serde_json::json!("another-skill"));
    }

    #[test]
    fn agent_model_from_preserves_non_native_skills() {
        use djinn_core::models::Agent;

        let agent = Agent {
            id: "test-id".to_string(),
            project_id: "proj".to_string(),
            name: "test-agent".to_string(),
            base_role: "worker".to_string(),
            description: "".to_string(),
            system_prompt_extensions: "".to_string(),
            model_preference: None,
            mcp_servers: "[]".to_string(),
            skills: r#"["skill-a", "skill-b"]"#.to_string(),
            is_default: false,
            created_at: "2024-01-01".to_string(),
            updated_at: "2024-01-01".to_string(),
        };

        let model = AgentModel::from(&agent);
        assert_eq!(model.skills.len(), 2);
    }
}
