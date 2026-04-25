// ADR-051 Epic C — Proposal pipeline backend.
//
// Manages architect-drafted ADRs as Dolt-backed notes with
// `note_type = "proposed_adr"` and promotes them to `note_type = "adr"`
// on acceptance.
//
// The architect (and chat) produce ADR drafts via `memory_write` with
// `type=proposed_adr`. Humans (or a conversion planner) then either
// accept them (changes the note's type to `adr`, optionally creating an
// Epic shell threaded with `originating_adr_id`) or reject them
// (deletes the note).
//
// ### Live-coordinator safety rule
// Epics created by `propose_adr_accept` are written with
// `auto_breakdown = true` so the normal breakdown Planner kicks in on
// the next coordinator tick.  Callers that want a manually-managed
// epic should pass `auto_breakdown: Some(false)` via `epic_create`
// instead.

use std::collections::BTreeMap;

use rmcp::{Json, handler::server::wrapper::Parameters, schemars, tool, tool_router};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::server::DjinnMcpServer;
use crate::tools::epic_ops::EpicModel;
use crate::tools::task_tools::ErrorOr;
use crate::tools::task_tools::ops::{CommentTaskRequest, add_task_comment};
use djinn_core::events::EventBus;
use djinn_db::{Database, EpicRepository, NoteRepository, ProjectRepository, folder_for_type};
use djinn_memory::Note;

// ── Permalink layout ─────────────────────────────────────────────────────────

const PROPOSED_FOLDER: &str = "decisions/proposed";

fn proposed_permalink(id: &str) -> String {
    format!("{PROPOSED_FOLDER}/{id}")
}

fn note_repository(db: &Database, events: &EventBus) -> NoteRepository {
    NoteRepository::new(db.clone(), events.clone())
}

// ── Frontmatter parser ───────────────────────────────────────────────────────

/// Minimal YAML-ish frontmatter parser.  Handles `---\n<k: v>\n---\n`
/// headers with unquoted scalar values (optionally wrapped in `"`).
/// Nested structures are not supported — Djinn ADR templates only use
/// top-level scalars.
fn parse_frontmatter(text: &str) -> (BTreeMap<String, String>, String) {
    let mut map: BTreeMap<String, String> = BTreeMap::new();
    // Strip BOM if present.
    let normalized = text.strip_prefix('\u{FEFF}').unwrap_or(text);

    // Only a leading `---` (optionally with trailing whitespace / CRLF)
    // counts as a frontmatter opener.
    let mut lines = normalized.split_inclusive('\n').peekable();
    let Some(first) = lines.peek().copied() else {
        return (map, text.to_owned());
    };
    if first.trim_end() != "---" {
        return (map, text.to_owned());
    }
    // Consume the opening `---`.
    lines.next();

    // Accumulate frontmatter lines until the closing `---`.
    let mut closed = false;
    let mut front_lines: Vec<&str> = Vec::new();
    for line in lines.by_ref() {
        if line.trim_end() == "---" {
            closed = true;
            break;
        }
        front_lines.push(line);
    }
    if !closed {
        return (map, text.to_owned());
    }

    // Parse `key: value` scalar lines.
    for raw in front_lines {
        let line = raw.trim_end_matches(['\n', '\r']);
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        let key = k.trim().to_string();
        if key.is_empty() {
            continue;
        }
        let mut value = v.trim().to_string();
        if value.len() >= 2
            && ((value.starts_with('"') && value.ends_with('"'))
                || (value.starts_with('\'') && value.ends_with('\'')))
        {
            value = value[1..value.len() - 1].to_string();
        }
        map.insert(key, value);
    }

    // Body: everything after the closing `---` line.
    let body: String = lines.collect();
    (map, body)
}

// ── Models ───────────────────────────────────────────────────────────────────

/// A parsed proposed ADR.  Serialized back to the MCP client.
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct ProposedAdr {
    /// Slug — the part of the permalink after `decisions/proposed/`.
    /// e.g. `"adr-052-new-pipeline"` for permalink
    /// `"decisions/proposed/adr-052-new-pipeline"`.
    pub id: String,
    /// Canonical permalink of the proposed-ADR note in Dolt.
    pub path: String,
    /// Note title (from `memory_write` `title=`).
    pub title: String,
    /// Work shape hint (`"task"` | `"epic"` | `"architectural"` | `"spike"`),
    /// parsed from the note content's leading YAML frontmatter when present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub work_shape: Option<String>,
    /// Optional short_id of the originating spike task — frontmatter-sourced.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub originating_spike_id: Option<String>,
    /// Last update timestamp on the underlying note, RFC 3339.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mtime: Option<String>,
    /// Raw note content. Omitted in list responses to keep them small.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    /// Djinn project UUID that owns this proposal.  Populated by the
    /// list handler so cross-project aggregation can thread items back
    /// to their project in the UI.  Omitted when the project could not
    /// be resolved from the Dolt registry.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    /// Display name of the owning project, sourced from the Dolt
    /// registry.  Used by the Proposals page to render per-row project
    /// chips when the "All projects" filter is active.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_path: Option<String>,
    /// Display name of the owning project — kept separate from
    /// `project_path` since the Proposals UI shows the human name in
    /// per-row chips.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_name: Option<String>,
}

impl ProposedAdr {
    /// Build a `ProposedAdr` from a Dolt-stored note. The note's
    /// `note_type` should be `"proposed_adr"`. `include_body` controls
    /// whether the full content is returned (set to `false` for list
    /// responses).
    fn from_note(note: &Note, include_body: bool) -> Self {
        let (front, _body) = parse_frontmatter(&note.content);
        let id = note
            .permalink
            .strip_prefix(&format!("{PROPOSED_FOLDER}/"))
            .unwrap_or(&note.permalink)
            .to_string();
        Self {
            id,
            path: note.permalink.clone(),
            title: note.title.clone(),
            work_shape: front.get("work_shape").cloned(),
            originating_spike_id: front.get("originating_spike_id").cloned(),
            mtime: Some(note.updated_at.clone()),
            body: if include_body {
                Some(note.content.clone())
            } else {
                None
            },
            project_id: None,
            project_name: None,
            project_path: None,
        }
    }
}

fn nullable_proposed_adr_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({
        "type": "object",
        "nullable": true,
        "allOf": [generator.subschema_for::<ProposedAdr>()]
    })
}

fn nullable_epic_model_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({
        "type": "object",
        "nullable": true,
        "allOf": [generator.subschema_for::<EpicModel>()]
    })
}

// ── Param / response structs ─────────────────────────────────────────────────

#[derive(Deserialize, JsonSchema)]
pub struct ProposeAdrListParams {
    /// Absolute project path.  Omit to list proposals across every
    /// registered project.  Each item in the response is tagged with
    /// `project_id` / `project_name` / `project_path` so the Proposals
    /// page can render cross-project lists without a second lookup.
    #[serde(default)]
    pub project: Option<String>,
}

#[derive(Serialize, JsonSchema)]
pub struct ProposeAdrListResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub items: Option<Vec<ProposedAdr>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
pub struct ProposeAdrShowParams {
    /// Absolute project path.
    pub project: String,
    /// File stem of the proposed ADR, e.g. `"adr-052-foo"`.
    pub id: String,
}

#[derive(Serialize, JsonSchema)]
pub struct ProposeAdrShowResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "nullable_proposed_adr_schema")]
    pub adr: Option<ProposedAdr>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
pub struct ProposeAdrAcceptParams {
    /// Absolute project path.
    pub project: String,
    /// File stem of the proposed ADR, e.g. `"adr-052-foo"`.
    pub id: String,
    /// Optional accepted title override. Defaults to the draft title.
    pub title: Option<String>,
    /// When `true` (default), a matching `Epic` shell is created for
    /// `work_shape ∈ {task, epic, spike}` ADRs.  Architectural ADRs
    /// never spawn an epic regardless of this flag.
    pub create_epic: Option<bool>,
    /// When creating the epic, set `auto_breakdown` to this value.
    /// Defaults to `true` so the normal breakdown Planner fires.
    pub auto_breakdown: Option<bool>,
}

#[derive(Serialize, JsonSchema)]
pub struct ProposeAdrAcceptResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub accepted_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "nullable_epic_model_schema")]
    pub epic: Option<EpicModel>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
pub struct ProposeAdrRejectParams {
    /// Absolute project path.
    pub project: String,
    /// File stem of the proposed ADR.
    pub id: String,
    /// Required rejection reason, persisted back to the originating spike when available.
    pub reason: String,
}

#[derive(Serialize, JsonSchema)]
pub struct ProposeAdrRejectResponse {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub feedback_target: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

// ── Tool router ──────────────────────────────────────────────────────────────

#[tool_router(router = proposal_tool_router, vis = "pub")]
impl DjinnMcpServer {
    /// List all ADR drafts (notes with `note_type = "proposed_adr"`).
    #[tool(
        description = "ADR-051 Epic C: list architect-drafted ADRs (notes with type=proposed_adr). Omit `project` to aggregate proposals across every registered project; each item is tagged with project_id/project_name/project_path for cross-project rendering. Returns parsed metadata (id, title, work_shape, originating_spike_id) plus the note's `updated_at` in `mtime` (RFC 3339). Body text is omitted; use propose_adr_show to read a single draft in full."
    )]
    pub async fn propose_adr_list(
        &self,
        Parameters(p): Parameters<ProposeAdrListParams>,
    ) -> Json<ProposeAdrListResponse> {
        let project_repo = ProjectRepository::new(self.state.db().clone(), self.state.event_bus());

        // Build the scan set: one entry per project. The single-project
        // branch still does a registry lookup so the response shape is
        // identical regardless of scope — the UI always gets
        // project_name / project_id when they're knowable.
        let scan_targets: Vec<(String, String)> = match &p.project {
            Some(project_ref) => {
                let found = match project_repo.resolve(project_ref).await.ok().flatten() {
                    Some(id) => project_repo.get(&id).await.ok().flatten(),
                    None => None,
                };
                match found {
                    Some(proj) => vec![(proj.id, proj.name)],
                    None => {
                        return Json(ProposeAdrListResponse {
                            items: None,
                            error: Some(format!("project not found: {project_ref}")),
                        });
                    }
                }
            }
            None => match project_repo.list().await {
                Ok(list) => list.into_iter().map(|proj| (proj.id, proj.name)).collect(),
                Err(e) => {
                    return Json(ProposeAdrListResponse {
                        items: None,
                        error: Some(format!("failed to list projects: {e}")),
                    });
                }
            },
        };

        let note_repo = note_repository(self.state.db(), &self.state.event_bus());
        let mut items: Vec<ProposedAdr> = Vec::new();
        for (project_id, project_name) in scan_targets {
            match note_repo
                .list(&project_id, Some(folder_for_type("proposed_adr")))
                .await
            {
                Ok(notes) => {
                    for note in notes {
                        if note.note_type != "proposed_adr" {
                            continue;
                        }
                        let mut adr = ProposedAdr::from_note(&note, false);
                        adr.project_id = Some(project_id.clone());
                        adr.project_name = Some(project_name.clone());
                        items.push(adr);
                    }
                }
                Err(e) => {
                    tracing::warn!(project_id = %project_id, error = %e, "skipping unreadable proposed-adr listing");
                }
            }
        }
        items.sort_by(|a, b| a.id.cmp(&b.id));
        Json(ProposeAdrListResponse {
            items: Some(items),
            error: None,
        })
    }

    /// Read a single proposed ADR in full.
    #[tool(
        description = "ADR-051 Epic C: read the full contents of a single proposed ADR by its slug (the part of the permalink after `decisions/proposed/`, e.g. \"adr-052-foo\")."
    )]
    pub async fn propose_adr_show(
        &self,
        Parameters(p): Parameters<ProposeAdrShowParams>,
    ) -> Json<ProposeAdrShowResponse> {
        let project_id = match self.resolve_project_id(&p.project).await {
            Ok(id) => id,
            Err(e) => {
                return Json(ProposeAdrShowResponse {
                    adr: None,
                    error: Some(e),
                });
            }
        };
        let note_repo = note_repository(self.state.db(), &self.state.event_bus());
        let note = match note_repo
            .get_by_permalink(&project_id, &proposed_permalink(&p.id))
            .await
        {
            Ok(Some(note)) if note.note_type == "proposed_adr" => note,
            Ok(_) => {
                return Json(ProposeAdrShowResponse {
                    adr: None,
                    error: Some(format!("proposed ADR not found: {}", p.id)),
                });
            }
            Err(e) => {
                return Json(ProposeAdrShowResponse {
                    adr: None,
                    error: Some(e.to_string()),
                });
            }
        };
        Json(ProposeAdrShowResponse {
            adr: Some(ProposedAdr::from_note(&note, true)),
            error: None,
        })
    }

    /// Accept a proposed ADR: changes its `note_type` from
    /// `proposed_adr` to `adr` (which moves it from
    /// `decisions/proposed/` to `decisions/`), and, for non-architectural
    /// work shapes, creates an Epic shell threaded with
    /// `originating_adr_id`.
    #[tool(
        description = "ADR-051 Epic C: accept a proposed ADR. Changes its note_type from proposed_adr to adr (permalink moves from decisions/proposed/<slug> to decisions/<slug>) and, unless create_epic=false or the ADR's work_shape is \"architectural\", creates an Epic shell (status=open, originating_adr_id=<id>, auto_breakdown default true) so the normal breakdown Planner can decompose it."
    )]
    pub async fn propose_adr_accept(
        &self,
        Parameters(p): Parameters<ProposeAdrAcceptParams>,
    ) -> Json<ProposeAdrAcceptResponse> {
        let project_id = match self.resolve_project_id(&p.project).await {
            Ok(id) => id,
            Err(e) => {
                return Json(ProposeAdrAcceptResponse {
                    accepted_path: None,
                    epic: None,
                    error: Some(e),
                });
            }
        };

        let note_repo = note_repository(self.state.db(), &self.state.event_bus());
        let proposed = match note_repo
            .get_by_permalink(&project_id, &proposed_permalink(&p.id))
            .await
        {
            Ok(Some(note)) if note.note_type == "proposed_adr" => note,
            Ok(_) => {
                return Json(ProposeAdrAcceptResponse {
                    accepted_path: None,
                    epic: None,
                    error: Some(format!("proposed ADR not found: {}", p.id)),
                });
            }
            Err(e) => {
                return Json(ProposeAdrAcceptResponse {
                    accepted_path: None,
                    epic: None,
                    error: Some(e.to_string()),
                });
            }
        };

        let adr = ProposedAdr::from_note(&proposed, true);

        let new_title = p
            .title
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| {
                if adr.title.is_empty() {
                    adr.id.clone()
                } else {
                    adr.title.clone()
                }
            });

        // Collision check: refuse to overwrite an existing accepted ADR
        // with the same target permalink.
        let target_permalink =
            djinn_db::permalink_for("adr", &new_title);
        if note_repo
            .get_by_permalink(&project_id, &target_permalink)
            .await
            .ok()
            .flatten()
            .is_some()
        {
            return Json(ProposeAdrAcceptResponse {
                accepted_path: None,
                epic: None,
                error: Some(format!(
                    "destination already exists: {target_permalink} (remove or rename before accepting)"
                )),
            });
        }

        // Promote: change note_type from proposed_adr → adr. The
        // permalink folder moves from `decisions/proposed/` to
        // `decisions/` automatically via folder_for_type.
        let accepted = match note_repo
            .move_note(&proposed.id, std::path::Path::new(""), &new_title, "adr")
            .await
        {
            Ok(note) => note,
            Err(e) => {
                return Json(ProposeAdrAcceptResponse {
                    accepted_path: None,
                    epic: None,
                    error: Some(format!("failed to promote proposed ADR: {e}")),
                });
            }
        };

        // Architectural ADRs (pure design decisions) never spawn epics.
        // Neither do callers who explicitly opt out via create_epic=false.
        let create_epic =
            p.create_epic.unwrap_or(true) && adr.work_shape.as_deref() != Some("architectural");
        if !create_epic {
            return Json(ProposeAdrAcceptResponse {
                accepted_path: Some(accepted.permalink.clone()),
                epic: None,
                error: None,
            });
        }

        let description = format!(
            "Epic shell spawned from accepted ADR `{}`.\n\n\
             Work shape: {}\n\
             Originating spike: {}\n\n\
             Read [[{}]] for the full architectural rationale before planning.",
            adr.id,
            adr.work_shape.as_deref().unwrap_or("<unspecified>"),
            adr.originating_spike_id.as_deref().unwrap_or("<none>"),
            accepted.permalink,
        );
        let memory_refs_json = serde_json::to_string(&vec![accepted.permalink.clone()])
            .unwrap_or_else(|_| "[]".into());

        let epic_repo = EpicRepository::new(self.state.db().clone(), self.state.event_bus());
        match epic_repo
            .create_for_project(
                &project_id,
                djinn_db::EpicCreateInput {
                    title: &new_title,
                    description: &description,
                    emoji: "",
                    color: "",
                    owner: "",
                    memory_refs: Some(&memory_refs_json),
                    status: Some("open"),
                    auto_breakdown: Some(p.auto_breakdown.unwrap_or(true)),
                    originating_adr_id: Some(&adr.id),
                },
            )
            .await
        {
            Ok(epic) => Json(ProposeAdrAcceptResponse {
                accepted_path: Some(accepted.permalink.clone()),
                epic: Some(EpicModel::from(&epic)),
                error: None,
            }),
            Err(e) => Json(ProposeAdrAcceptResponse {
                accepted_path: Some(accepted.permalink.clone()),
                epic: None,
                error: Some(format!(
                    "promotion succeeded, but epic creation failed: {e}"
                )),
            }),
        }
    }

    /// Reject a proposed ADR: deletes the underlying note. Posts the
    /// rejection reason as a comment on the originating spike when one
    /// is referenced in the proposal's frontmatter.
    #[tool(
        description = "ADR-051 Epic C: reject a proposed ADR by deleting the underlying note. When the proposal's frontmatter references an `originating_spike_id`, the rejection reason is persisted as a comment on that task before deletion."
    )]
    pub async fn propose_adr_reject(
        &self,
        Parameters(p): Parameters<ProposeAdrRejectParams>,
    ) -> Json<ProposeAdrRejectResponse> {
        if p.reason.trim().is_empty() {
            return Json(ProposeAdrRejectResponse {
                ok: false,
                feedback_target: None,
                error: Some("rejection reason is required".to_string()),
            });
        }
        let project_id = match self.resolve_project_id(&p.project).await {
            Ok(id) => id,
            Err(e) => {
                return Json(ProposeAdrRejectResponse {
                    ok: false,
                    feedback_target: None,
                    error: Some(e),
                });
            }
        };

        let note_repo = note_repository(self.state.db(), &self.state.event_bus());
        let note = match note_repo
            .get_by_permalink(&project_id, &proposed_permalink(&p.id))
            .await
        {
            Ok(Some(note)) if note.note_type == "proposed_adr" => note,
            Ok(_) => {
                return Json(ProposeAdrRejectResponse {
                    ok: false,
                    feedback_target: None,
                    error: Some(format!("proposed ADR not found: {}", p.id)),
                });
            }
            Err(e) => {
                return Json(ProposeAdrRejectResponse {
                    ok: false,
                    feedback_target: None,
                    error: Some(e.to_string()),
                });
            }
        };
        let adr = ProposedAdr::from_note(&note, true);

        let feedback_target = if let Some(originating_spike_id) =
            adr.originating_spike_id.as_deref()
        {
            let body = format!(
                "Proposal `{}` rejected.\n\nReason: {}",
                adr.id,
                p.reason.trim()
            );
            match add_task_comment(
                self,
                &project_id,
                CommentTaskRequest {
                    id: originating_spike_id.to_string(),
                    body,
                    actor_id: "pulse".to_string(),
                    actor_role: "user".to_string(),
                },
            )
            .await
            .0
            {
                ErrorOr::Ok(_) => Some(format!("originating spike {originating_spike_id}")),
                ErrorOr::Error(error) => {
                    return Json(ProposeAdrRejectResponse {
                        ok: false,
                        feedback_target: None,
                        error: Some(format!(
                            "failed to persist rejection feedback to originating spike {originating_spike_id}: {}",
                            error.error
                        )),
                    });
                }
            }
        } else {
            None
        };

        match note_repo.delete(&note.id).await {
            Ok(()) => Json(ProposeAdrRejectResponse {
                ok: true,
                feedback_target,
                error: None,
            }),
            Err(e) => Json(ProposeAdrRejectResponse {
                ok: false,
                feedback_target: None,
                error: Some(format!("failed to delete proposed ADR: {e}")),
            }),
        }
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_note(permalink: &str, title: &str, content: &str) -> Note {
        Note {
            id: "note-id".to_string(),
            project_id: "project-id".to_string(),
            permalink: permalink.to_string(),
            title: title.to_string(),
            file_path: String::new(),
            storage: "db".to_string(),
            note_type: "proposed_adr".to_string(),
            folder: PROPOSED_FOLDER.to_string(),
            tags: "[]".to_string(),
            content: content.to_string(),
            created_at: "2026-04-24T12:00:00.000Z".to_string(),
            updated_at: "2026-04-24T12:30:00.000Z".to_string(),
            last_accessed: "2026-04-24T12:30:00.000Z".to_string(),
            access_count: 0,
            confidence: 1.0,
            abstract_: None,
            overview: None,
            scope_paths: "[]".to_string(),
        }
    }

    #[test]
    fn frontmatter_parses_simple_scalars() {
        let text = "---\ntitle: Hello\nwork_shape: epic\noriginating_spike_id: ab12\n---\n\n# Body heading\n\nSome body text.\n";
        let (front, body) = parse_frontmatter(text);
        assert_eq!(front.get("title").map(String::as_str), Some("Hello"));
        assert_eq!(front.get("work_shape").map(String::as_str), Some("epic"));
        assert_eq!(
            front.get("originating_spike_id").map(String::as_str),
            Some("ab12")
        );
        assert!(body.starts_with("\n# Body heading"));
    }

    #[test]
    fn frontmatter_handles_quoted_values() {
        let text = "---\ntitle: \"Quoted Title\"\n---\nBody\n";
        let (front, _) = parse_frontmatter(text);
        assert_eq!(front.get("title").map(String::as_str), Some("Quoted Title"));
    }

    #[test]
    fn frontmatter_no_leading_dashes_returns_raw() {
        let text = "# Heading\n\nBody with no frontmatter.\n";
        let (front, body) = parse_frontmatter(text);
        assert!(front.is_empty());
        assert_eq!(body, text);
    }

    #[test]
    fn proposed_adr_from_note_extracts_metadata_and_strips_folder_prefix() {
        let note = fake_note(
            "decisions/proposed/adr-999-demo",
            "Demo ADR",
            "---\nwork_shape: epic\noriginating_spike_id: spk7\n---\n\n# Demo ADR\n\nBody\n",
        );
        let adr = ProposedAdr::from_note(&note, true);
        assert_eq!(adr.id, "adr-999-demo");
        assert_eq!(adr.title, "Demo ADR");
        assert_eq!(adr.work_shape.as_deref(), Some("epic"));
        assert_eq!(adr.originating_spike_id.as_deref(), Some("spk7"));
        assert_eq!(adr.path, "decisions/proposed/adr-999-demo");
        assert!(
            adr.mtime
                .as_deref()
                .is_some_and(|value| value.contains('T'))
        );
        assert!(adr.body.as_ref().unwrap().contains("Body"));
    }

    #[test]
    fn proposed_adr_from_note_omits_body_in_list_view() {
        let note = fake_note(
            "decisions/proposed/adr-1-skinny",
            "Skinny",
            "no frontmatter, just body\n",
        );
        let adr = ProposedAdr::from_note(&note, false);
        assert!(adr.body.is_none());
        assert!(adr.work_shape.is_none());
    }
}
