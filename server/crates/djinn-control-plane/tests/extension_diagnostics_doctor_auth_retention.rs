//! End-to-end doctor probe contract: authorization, fresh identity, isolation, and retention.
//!
//! This uses the real agent probe behind the normal control-plane doctor dispatch rather
//! than a recording bridge. The checked fixture is materialized at the canonical
//! `project_dir` while `DJINN_HOME` is held under a process-wide lock.

use std::path::Path;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use djinn_agent::context::AgentContext;
use djinn_control_plane::bridge::ExtensionDiagnosticsProbeOps;
use djinn_control_plane::test_support::McpTestHarness;
use djinn_core::auth_context::SESSION_USER_ID;
use djinn_core::events::EventBus;
use djinn_core::extension_diagnostics::ExtensionLoadDiagnosticV1;
use djinn_core::paths::project_dir;
use djinn_db::{
    EffectiveCreatorProvenance, ExtensionLoadDiagnosticRepository, InsertExtensionLoadDiagnostic,
    ProjectRepository, SessionRepository, TaskRepository,
    repositories::session::CreateSessionParams, repositories::user::UserRepository,
    test_support::delete_session_row,
};
use serde_json::json;
use tokio_util::sync::CancellationToken;

const PROBE_NAME: &str = "extension_load.project_probe";
static DJINN_HOME_LOCK: Mutex<()> = Mutex::new(());

struct ProductionProbe {
    context: AgentContext,
    returned_attempts: Mutex<Vec<Vec<ExtensionLoadDiagnosticV1>>>,
}

impl ProductionProbe {
    fn returned_attempts(&self) -> Vec<Vec<ExtensionLoadDiagnosticV1>> {
        self.returned_attempts
            .lock()
            .expect("probe result lock")
            .clone()
    }
}

#[async_trait]
impl ExtensionDiagnosticsProbeOps for ProductionProbe {
    async fn probe_project_extensions(
        &self,
        project_id: &str,
        canonical_workspace: &Path,
    ) -> Result<Vec<ExtensionLoadDiagnosticV1>, String> {
        let rows = djinn_agent::extension_diagnostics_probe::probe_project_extensions(
            project_id,
            canonical_workspace,
            &self.context,
        )
        .await?;
        self.returned_attempts
            .lock()
            .expect("probe result lock")
            .push(rows.clone());
        Ok(rows)
    }
}

struct DjinnHomeRestore(Option<std::ffi::OsString>);

impl Drop for DjinnHomeRestore {
    fn drop(&mut self) {
        // SAFETY: the process-wide lock remains held for this test's full scope.
        unsafe {
            match self.0.take() {
                Some(value) => std::env::set_var("DJINN_HOME", value),
                None => std::env::remove_var("DJINN_HOME"),
            }
        }
    }
}

async fn create_admin(db: &djinn_db::Database) -> String {
    let users = djinn_db::repositories::user::UserRepository::new(db.clone());
    let user = users
        .upsert_from_github(8_811_991, "doctor-probe-admin", None, None)
        .await
        .expect("create admin user");
    users
        .set_admin_status(&user.id, true)
        .await
        .expect("grant admin");
    user.id
}

fn finding_fields(row: &ExtensionLoadDiagnosticV1) -> serde_json::Value {
    json!({
        "diagnostic_id": row.diagnostic_id,
        "source_kind": row.source_kind.as_str(),
        "source_key": row.source_key,
        "phase": row.phase.as_str(),
        "summary": row.summary,
        "remedy_code": row.remedy_code.as_str(),
        "remedy": row.remedy,
        "severity": row.severity.as_str(),
        "occurrence_count": row.occurrence_count,
    })
}

#[tokio::test(flavor = "current_thread")]
async fn extension_diagnostics_doctor_auth_retention() {
    let _environment_guard = DJINN_HOME_LOCK.lock().expect("DJINN_HOME lock");
    let home = tempfile::tempdir().expect("isolated DJINN_HOME");
    let restore = DjinnHomeRestore(std::env::var_os("DJINN_HOME"));
    // SAFETY: the process-wide lock is held until `restore` runs.
    unsafe { std::env::set_var("DJINN_HOME", home.path()) };

    let fixture: serde_json::Value = serde_json::from_str(include_str!(
        "../../djinn-agent/tests/fixtures/extension_diagnostics/doctor_probe.json"
    ))
    .expect("valid checked doctor probe fixture");
    let harness = McpTestHarness::new().await;
    let projects = ProjectRepository::new(harness.db().clone(), EventBus::noop());
    let project_a = projects
        .create("doctor-probe-a", "doctor-probe", "a")
        .await
        .expect("project A");
    let project_b = projects
        .create("doctor-probe-b", "doctor-probe", "b")
        .await
        .expect("project B");
    projects
        .set_environment_config(&project_a.id, &fixture["environment_config"].to_string())
        .await
        .expect("project A environment config");
    projects
        .set_environment_config(&project_b.id, &fixture["environment_config"].to_string())
        .await
        .expect("project B environment config");

    let workspace_a = project_dir(&project_a.github_owner, &project_a.github_repo);
    std::fs::create_dir_all(&workspace_a).expect("canonical project A workspace");
    std::fs::write(
        workspace_a.join("mcp.json"),
        fixture["mcp_json"].as_str().expect("fixture MCP JSON"),
    )
    .expect("materialize read-only MCP fixture");
    let fixture_before = std::fs::read_to_string(workspace_a.join("mcp.json")).expect("fixture");
    let workspace_b = project_dir(&project_b.github_owner, &project_b.github_repo);
    std::fs::create_dir_all(&workspace_b).expect("canonical project B workspace");
    std::fs::write(
        workspace_b.join("mcp.json"),
        fixture["mcp_json"].as_str().expect("fixture MCP JSON"),
    )
    .expect("materialize project B read-only MCP fixture");

    let context = djinn_agent::test_helpers::agent_context_from_db(
        harness.db().clone(),
        CancellationToken::new(),
    );
    let probe = Arc::new(ProductionProbe {
        context,
        returned_attempts: Mutex::new(Vec::new()),
    });
    let harness = McpTestHarness::from_state(
        harness
            .state()
            .clone()
            .with_extension_diagnostics_probe(probe.clone()),
    );
    let admin = create_admin(harness.db()).await;

    let first_response = SESSION_USER_ID
        .scope(Some(admin.clone()), async {
            harness
                .call_tool(
                    "doctor_run",
                    json!({ "check_names": [PROBE_NAME], "project": project_a.slug() }),
                )
                .await
        })
        .await
        .expect("authorized first probe dispatch");
    let first_findings = first_response["results"][0]["extension_diagnostics"]
        .as_array()
        .expect("first doctor findings")
        .clone();
    let first_ids: Vec<_> = first_findings
        .iter()
        .map(|row| row["diagnostic_id"].as_str().expect("first diagnostic id"))
        .collect();
    assert!(
        first_findings
            .iter()
            .any(|row| row["source_kind"] == "project_mcp")
    );
    assert!(
        first_findings
            .iter()
            .any(|row| row["source_kind"] == "project_skill")
    );

    // Seed an equivalent historical session row through the repository. The second
    // invocation must not reconstruct or return this prior row.
    let repository = ExtensionLoadDiagnosticRepository::new(harness.db().clone());
    let first_probe_rows = probe.returned_attempts();
    let first_row = first_probe_rows
        .first()
        .and_then(|attempt| attempt.first())
        .expect("first persisted doctor diagnostic");
    let historical_creator = UserRepository::new(harness.db().clone())
        .upsert_from_github(8_811_992, "doctor-probe-historical-session", None, None)
        .await
        .expect("create historical-session task user");
    let task = TaskRepository::new(harness.db().clone(), EventBus::noop())
        .create_in_project_with_provenance(
            &project_a.id,
            None,
            EffectiveCreatorProvenance {
                explicit_user_id: Some(&historical_creator.id),
                source_task_id: None,
                proposal_id: None,
            },
            "diagnostic task",
            "",
            "",
            "task",
            1,
            "test",
            None,
            None,
        )
        .await
        .expect("task for historical session");
    let session = SessionRepository::new(harness.db().clone(), EventBus::noop())
        .create(CreateSessionParams {
            project_id: &project_a.id,
            task_id: Some(&task.id),
            model: "test",
            agent_type: "worker",
            metadata_json: None,
            task_run_id: None,
            pricing: None,
            cost_basis: None,
        })
        .await
        .expect("historical session");
    let historical_attempt = uuid::Uuid::now_v7().to_string();
    let historical = repository
        .insert_or_increment(InsertExtensionLoadDiagnostic {
            project_id: project_a.id.clone(),
            task_id: Some(task.id.clone()),
            session_id: Some(session.id.clone()),
            load_attempt_id: historical_attempt.clone(),
            source_kind: first_row.source_kind,
            source_key: first_row.source_key.clone(),
            phase: first_row.phase,
            severity: first_row.severity,
            summary: first_row.summary.clone(),
            summary_fingerprint: uuid::Uuid::now_v7().simple().to_string(),
            remedy_code: first_row.remedy_code,
            remedy: first_row.remedy.clone(),
            first_seen_at: first_row.first_seen_at.clone(),
            last_seen_at: first_row.last_seen_at.clone(),
            created_at: first_row.created_at.clone(),
        })
        .await
        .expect("seed equivalent session diagnostic");
    assert_eq!(historical.task_id.as_deref(), Some(task.id.as_str()));
    assert_eq!(historical.session_id.as_deref(), Some(session.id.as_str()));
    assert_eq!(
        historical.source_kind.as_str(),
        first_findings[0]["source_kind"]
            .as_str()
            .expect("first source kind")
    );
    assert_eq!(
        historical.phase.as_str(),
        first_findings[0]["phase"].as_str().expect("first phase")
    );
    assert_eq!(
        historical.summary,
        first_findings[0]["summary"]
            .as_str()
            .expect("first summary")
    );

    let response = SESSION_USER_ID
        .scope(Some(admin.clone()), async {
            harness
                .call_tool(
                    "doctor_run",
                    json!({ "check_names": [PROBE_NAME], "project": project_a.id }),
                )
                .await
        })
        .await
        .expect("authorized fresh probe dispatch");
    assert_eq!(response["ok"], true);
    let findings = response["results"][0]["extension_diagnostics"]
        .as_array()
        .expect("fresh doctor findings");
    assert!(!findings.is_empty(), "fixture must generate diagnostics");
    assert!(
        findings
            .iter()
            .all(|row| row["diagnostic_id"] != historical.diagnostic_id.as_str())
    );
    assert!(
        findings.iter().all(|row| !first_ids
            .contains(&row["diagnostic_id"].as_str().expect("fresh diagnostic id"),)),
        "a new probe cannot project any prior doctor attempt"
    );

    let fresh_rows = probe
        .returned_attempts()
        .get(1)
        .expect("fresh probe returned persisted rows")
        .clone();
    let fresh = fresh_rows[0].load_attempt_id.clone();
    assert_ne!(
        fresh, historical_attempt,
        "doctor invocation creates a fresh attempt"
    );
    assert!(
        uuid::Uuid::parse_str(&fresh)
            .expect("UUIDv7 attempt")
            .get_version_num()
            == 7
    );

    let canonical = repository
        .list_for_load_attempt(&project_a.id, &fresh)
        .await
        .expect("attempt rows");
    assert_eq!(
        canonical, fresh_rows,
        "probe returns canonical persisted rows"
    );
    assert!(
        canonical
            .iter()
            .all(|row| row.task_id.is_none() && row.session_id.is_none())
    );
    let response_fields: Vec<_> = findings.iter().cloned().collect();
    let canonical_fields: Vec<_> = canonical.iter().map(finding_fields).collect();
    assert_eq!(
        response_fields, canonical_fields,
        "response is the exact canonical attempt projection"
    );
    let fresh_ids: Vec<_> = findings
        .iter()
        .map(|row| {
            row["diagnostic_id"]
                .as_str()
                .expect("fresh diagnostic id")
                .to_owned()
        })
        .collect();
    assert!(
        repository
            .list_for_load_attempt(&project_b.id, &fresh)
            .await
            .expect("project B scoped read")
            .is_empty()
    );
    let project_b_response = SESSION_USER_ID
        .scope(Some(admin.clone()), async {
            harness
                .call_tool(
                    "doctor_run",
                    json!({ "check_names": [PROBE_NAME], "project": project_b.id }),
                )
                .await
        })
        .await
        .expect("authorized project B probe dispatch");
    assert_eq!(project_b_response["ok"], true);
    let project_b_findings = project_b_response["results"][0]["extension_diagnostics"]
        .as_array()
        .expect("project B doctor findings");
    assert!(
        !project_b_findings.is_empty(),
        "project B fixture generates diagnostics"
    );
    assert!(
        project_b_findings.iter().all(|row| !fresh_ids.contains(
            &row["diagnostic_id"]
                .as_str()
                .expect("project B diagnostic id")
                .to_owned(),
        )),
        "project B invocation cannot project project A attempt diagnostics"
    );
    let project_b_rows = probe
        .returned_attempts()
        .get(2)
        .expect("project B probe returned persisted rows")
        .clone();
    let project_b_attempt = &project_b_rows[0].load_attempt_id;
    assert_ne!(
        project_b_attempt, &fresh,
        "project B invocation creates its own fresh attempt"
    );
    assert_ne!(
        project_b_attempt, &historical_attempt,
        "project B invocation cannot reuse the historical session attempt"
    );
    assert_eq!(
        uuid::Uuid::parse_str(project_b_attempt)
            .expect("project B UUIDv7 attempt")
            .get_version_num(),
        7
    );
    assert!(
        project_b_rows
            .iter()
            .all(|row| row.task_id.is_none() && row.session_id.is_none()),
        "project B doctor rows remain task- and session-independent"
    );
    let project_b_canonical = repository
        .list_for_load_attempt(&project_b.id, &project_b_attempt)
        .await
        .expect("project B attempt rows");
    assert_eq!(
        project_b_canonical, project_b_rows,
        "project B probe returns its own canonical persisted rows"
    );
    assert_eq!(
        project_b_findings.iter().cloned().collect::<Vec<_>>(),
        project_b_canonical
            .iter()
            .map(finding_fields)
            .collect::<Vec<_>>(),
        "project B response projects only project B's new persisted attempt"
    );
    assert_eq!(
        std::fs::read_to_string(workspace_a.join("mcp.json")).expect("fixture after probe"),
        fixture_before,
        "probe never mutates project files"
    );

    delete_session_row(harness.db(), &session.id).await;
    assert!(
        repository
            .list_for_load_attempt(&project_a.id, &fresh)
            .await
            .expect("doctor rows survive session deletion")
            .len()
            == canonical.len()
    );
    projects
        .delete(&project_a.id)
        .await
        .expect("delete project A");
    assert!(
        repository
            .list_for_load_attempt(&project_a.id, &fresh)
            .await
            .expect("project cascade")
            .is_empty()
    );

    drop(restore);
}
