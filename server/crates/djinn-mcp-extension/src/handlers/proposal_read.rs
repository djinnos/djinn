use djinn_db::{ProjectRepository, ProposalRepository};

use crate::context::ExtensionContext;
use crate::helpers::parse_args;
use crate::types::ProposalShowParams;

use super::proposal_authoring::committed_latest_lint;

/// Assemble proposal read results from immutable repository snapshots.
pub(super) async fn call_proposal_show(
    ctx: &dyn ExtensionContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
) -> Result<serde_json::Value, String> {
    let p: ProposalShowParams = parse_args(arguments)?;
    if let Some(ref fields) = p.fields {
        djinn_control_plane::tools::proposal_ops::validate_show_fields(fields)?;
    }
    if let Some(ref revision_bodies) = p.revision_bodies {
        djinn_control_plane::tools::proposal_ops::validate_revision_bodies_value(revision_bodies)?;
    }
    let field_selected = |name: &str| {
        p.fields
            .as_ref()
            .is_none_or(|fields| fields.iter().any(|field| field == name))
    };

    let proposal_repo = ProposalRepository::new(ctx.db(), ctx.event_bus());
    let Some(proposal) = proposal_repo.resolve(&p.id).await.ok().flatten() else {
        return Err(format!("proposal not found: {}", p.id));
    };
    let latest_lint = committed_latest_lint(&proposal_repo, &proposal).await?;
    let mut result = serde_json::json!({ "latest_lint": latest_lint });

    if field_selected("proposal") {
        let acceptance =
            serde_json::from_str(&proposal.acceptance_criteria).unwrap_or(serde_json::json!([]));
        result["id"] = serde_json::json!(proposal.id);
        result["short_id"] = serde_json::json!(proposal.short_id);
        result["title"] = serde_json::json!(proposal.title);
        result["body"] = serde_json::json!(proposal.body);
        result["status"] = serde_json::json!(proposal.status);
        result["acceptance_criteria"] = acceptance;
    }

    if field_selected("revisions") {
        let stored_revisions = proposal_repo
            .revisions(&proposal.id)
            .await
            .map_err(|error| error.to_string())?;
        let mut revisions = Vec::with_capacity(stored_revisions.len());
        for revision in &stored_revisions {
            let lint = proposal_repo
                .lint_for_revision(revision)
                .await
                .map_err(|error| error.to_string())?;
            let mut model =
                djinn_control_plane::tools::proposal_ops::ProposalRevisionModel::from(revision);
            model.lint = Some(lint);
            revisions.push(model);
        }
        djinn_control_plane::tools::proposal_ops::apply_revision_body_mode(
            &mut revisions,
            p.revision_bodies.as_deref().unwrap_or("excerpt"),
        );
        result["revisions"] = serde_json::to_value(revisions).map_err(|error| error.to_string())?;
    }

    if field_selected("targets") {
        let targets = proposal_repo
            .targets(&proposal.id)
            .await
            .map_err(|error| error.to_string())?;
        let project_repo = ProjectRepository::new(ctx.db(), ctx.event_bus());
        let mut target_json = Vec::with_capacity(targets.len());
        for target in &targets {
            let project = match project_repo.get(&target.project_id).await {
                Ok(Some(project)) => format!("{}/{}", project.github_owner, project.github_repo),
                _ => target.project_id.clone(),
            };
            target_json.push(serde_json::json!({
                "project_id": target.project_id,
                "project": project,
                "role": target.role,
            }));
        }
        result["targets"] = serde_json::json!(target_json);
    }
    Ok(result)
}
