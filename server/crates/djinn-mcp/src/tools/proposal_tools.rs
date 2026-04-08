// ADR-051 Epic C — Proposal pipeline backend.
//
// Manages architect-drafted ADRs under `.djinn/decisions/proposed/` and
// their promotion to `.djinn/decisions/` on acceptance.
//
// The architect (and chat) produce ADR drafts into `proposed/`.  Humans
// (or a conversion planner) then either accept them (moves the file out
// of `proposed/`, optionally creating an Epic shell threaded with
// `originating_adr_id`) or reject them (removes the file).
//
// ### Live-coordinator safety rule
// Epics created by `propose_adr_accept` are written with
// `auto_breakdown = true` so the normal breakdown Planner kicks in on
// the next coordinator tick.  Callers that want a manually-managed
// epic should pass `auto_breakdown: Some(false)` via `epic_create`
// instead.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use rmcp::{Json, handler::server::wrapper::Parameters, schemars, tool, tool_router};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::server::DjinnMcpServer;
use crate::tools::epic_ops::EpicModel;
use djinn_db::EpicRepository;

// ── Filesystem layout ────────────────────────────────────────────────────────

const DECISIONS_SUBDIR: &str = ".djinn/decisions";
const PROPOSED_SUBDIR: &str = ".djinn/decisions/proposed";

fn decisions_dir(project_root: &Path) -> PathBuf {
    project_root.join(DECISIONS_SUBDIR)
}

fn proposed_dir(project_root: &Path) -> PathBuf {
    project_root.join(PROPOSED_SUBDIR)
}

fn proposed_file_for(project_root: &Path, id: &str) -> PathBuf {
    proposed_dir(project_root).join(format!("{id}.md"))
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

/// Extract a title from the frontmatter (`title:`) or, failing that,
/// the first markdown H1 in the body.  Returns the empty string when
/// neither source is available.
fn extract_title(front: &BTreeMap<String, String>, body: &str) -> String {
    if let Some(t) = front.get("title")
        && !t.is_empty()
    {
        return t.clone();
    }
    for line in body.lines() {
        if let Some(rest) = line.strip_prefix("# ") {
            return rest.trim().to_string();
        }
    }
    String::new()
}

// ── Models ───────────────────────────────────────────────────────────────────

/// A parsed proposed ADR.  Serialized back to the MCP client.
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct ProposedAdr {
    /// File stem, e.g. `"adr-052-new-pipeline"`.
    pub id: String,
    /// Absolute filesystem path of the source file (under `proposed/`).
    pub path: String,
    /// Title pulled from frontmatter or the first H1.
    pub title: String,
    /// Work shape hint (`"task"` | `"epic"` | `"architectural"` | `"spike"`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub work_shape: Option<String>,
    /// Optional short_id of the originating spike task.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub originating_spike_id: Option<String>,
    /// Raw markdown body (frontmatter stripped).  Omitted in list
    /// responses to keep them small.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
}

impl ProposedAdr {
    fn from_file(path: &Path, include_body: bool) -> Result<Self, String> {
        let content = fs::read_to_string(path)
            .map_err(|e| format!("failed to read {}: {e}", path.display()))?;
        let (front, body) = parse_frontmatter(&content);
        let id = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();
        let title = extract_title(&front, &body);
        let work_shape = front.get("work_shape").cloned();
        let originating_spike_id = front.get("originating_spike_id").cloned();
        Ok(Self {
            id,
            path: path.display().to_string(),
            title,
            work_shape,
            originating_spike_id,
            body: if include_body { Some(body) } else { None },
        })
    }
}

// ── Param / response structs ─────────────────────────────────────────────────

#[derive(Deserialize, JsonSchema)]
pub struct ProposeAdrListParams {
    /// Absolute project path.
    pub project: String,
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
    #[serde(flatten, skip_serializing_if = "Option::is_none")]
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
    #[serde(flatten, skip_serializing_if = "Option::is_none")]
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
}

#[derive(Serialize, JsonSchema)]
pub struct ProposeAdrRejectResponse {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

// ── Tool router ──────────────────────────────────────────────────────────────

#[tool_router(router = proposal_tool_router, vis = "pub")]
impl DjinnMcpServer {
    /// List all ADR drafts under `.djinn/decisions/proposed/`.
    #[tool(
        description = "ADR-051 Epic C: list ADR drafts under .djinn/decisions/proposed/. Returns frontmatter-parsed metadata (id, title, work_shape, originating_spike_id). Body text is omitted; use propose_adr_show to read a single draft in full."
    )]
    pub async fn propose_adr_list(
        &self,
        Parameters(p): Parameters<ProposeAdrListParams>,
    ) -> Json<ProposeAdrListResponse> {
        let root = PathBuf::from(&p.project);
        let dir = proposed_dir(&root);
        if !dir.exists() {
            return Json(ProposeAdrListResponse {
                items: Some(vec![]),
                error: None,
            });
        }
        let entries = match fs::read_dir(&dir) {
            Ok(r) => r,
            Err(e) => {
                return Json(ProposeAdrListResponse {
                    items: None,
                    error: Some(format!("failed to read {}: {e}", dir.display())),
                });
            }
        };

        let mut items: Vec<ProposedAdr> = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("md") {
                continue;
            }
            match ProposedAdr::from_file(&path, false) {
                Ok(adr) => items.push(adr),
                Err(e) => {
                    tracing::warn!(path = %path.display(), error = %e, "skipping unreadable proposed ADR");
                }
            }
        }
        items.sort_by(|a, b| a.id.cmp(&b.id));
        Json(ProposeAdrListResponse {
            items: Some(items),
            error: None,
        })
    }

    /// Read a single proposed ADR draft in full (frontmatter + body).
    #[tool(
        description = "ADR-051 Epic C: read the full contents of a single proposed ADR draft by its file stem (e.g. \"adr-052-foo\")."
    )]
    pub async fn propose_adr_show(
        &self,
        Parameters(p): Parameters<ProposeAdrShowParams>,
    ) -> Json<ProposeAdrShowResponse> {
        let root = PathBuf::from(&p.project);
        let path = proposed_file_for(&root, &p.id);
        if !path.exists() {
            return Json(ProposeAdrShowResponse {
                adr: None,
                error: Some(format!("proposed ADR not found: {}", p.id)),
            });
        }
        match ProposedAdr::from_file(&path, true) {
            Ok(adr) => Json(ProposeAdrShowResponse {
                adr: Some(adr),
                error: None,
            }),
            Err(e) => Json(ProposeAdrShowResponse {
                adr: None,
                error: Some(e),
            }),
        }
    }

    /// Accept a proposed ADR: atomically moves the file out of
    /// `proposed/` into `.djinn/decisions/`, and (for non-architectural
    /// work shapes) creates an Epic shell threaded with
    /// `originating_adr_id`.
    #[tool(
        description = "ADR-051 Epic C: accept a proposed ADR. Moves the draft from .djinn/decisions/proposed/ to .djinn/decisions/ and, unless create_epic=false or the ADR's work_shape is \"architectural\", creates an Epic shell (status=open, originating_adr_id=<id>, auto_breakdown default true) so the normal breakdown Planner can decompose it."
    )]
    pub async fn propose_adr_accept(
        &self,
        Parameters(p): Parameters<ProposeAdrAcceptParams>,
    ) -> Json<ProposeAdrAcceptResponse> {
        let root = PathBuf::from(&p.project);
        let src = proposed_file_for(&root, &p.id);
        if !src.exists() {
            return Json(ProposeAdrAcceptResponse {
                accepted_path: None,
                epic: None,
                error: Some(format!("proposed ADR not found: {}", p.id)),
            });
        }

        // Parse before moving so we can use the metadata to build the
        // epic shell even if the file is later gone from its original
        // location.
        let adr = match ProposedAdr::from_file(&src, true) {
            Ok(adr) => adr,
            Err(e) => {
                return Json(ProposeAdrAcceptResponse {
                    accepted_path: None,
                    epic: None,
                    error: Some(e),
                });
            }
        };

        let decisions = decisions_dir(&root);
        if let Err(e) = fs::create_dir_all(&decisions) {
            return Json(ProposeAdrAcceptResponse {
                accepted_path: None,
                epic: None,
                error: Some(format!(
                    "failed to create {}: {e}",
                    decisions.display()
                )),
            });
        }
        let dst = decisions.join(format!("{}.md", adr.id));
        if dst.exists() {
            return Json(ProposeAdrAcceptResponse {
                accepted_path: None,
                epic: None,
                error: Some(format!(
                    "destination already exists: {} (remove or rename before accepting)",
                    dst.display()
                )),
            });
        }
        if let Err(e) = fs::rename(&src, &dst) {
            return Json(ProposeAdrAcceptResponse {
                accepted_path: None,
                epic: None,
                error: Some(format!(
                    "failed to move {} → {}: {e}",
                    src.display(),
                    dst.display()
                )),
            });
        }

        // Architectural ADRs (pure design decisions) never spawn epics.
        // Neither do callers who explicitly opt out via create_epic=false.
        let create_epic = p.create_epic.unwrap_or(true)
            && adr.work_shape.as_deref() != Some("architectural");
        if !create_epic {
            return Json(ProposeAdrAcceptResponse {
                accepted_path: Some(dst.display().to_string()),
                epic: None,
                error: None,
            });
        }

        // Resolve project ID for the epic create.
        let project_id = match self.resolve_project_id(&p.project).await {
            Ok(id) => id,
            Err(e) => {
                return Json(ProposeAdrAcceptResponse {
                    accepted_path: Some(dst.display().to_string()),
                    epic: None,
                    error: Some(format!(
                        "file accepted, but epic creation failed: {e}"
                    )),
                });
            }
        };

        let title = if adr.title.is_empty() {
            adr.id.clone()
        } else {
            adr.title.clone()
        };
        let description = format!(
            "Epic shell spawned from accepted ADR `{}`.\n\n\
             Work shape: {}\n\
             Originating spike: {}\n\n\
             Read `{}` for the full architectural rationale before planning.",
            adr.id,
            adr.work_shape.as_deref().unwrap_or("<unspecified>"),
            adr.originating_spike_id.as_deref().unwrap_or("<none>"),
            dst.display(),
        );
        let memory_refs_json =
            serde_json::to_string(&vec![dst.display().to_string()]).unwrap_or_else(|_| "[]".into());

        let repo = EpicRepository::new(self.state.db().clone(), self.state.event_bus());
        match repo
            .create_for_project(
                &project_id,
                djinn_db::EpicCreateInput {
                    title: &title,
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
                accepted_path: Some(dst.display().to_string()),
                epic: Some(EpicModel::from(&epic)),
                error: None,
            }),
            Err(e) => Json(ProposeAdrAcceptResponse {
                accepted_path: Some(dst.display().to_string()),
                epic: None,
                error: Some(format!("file accepted, but epic creation failed: {e}")),
            }),
        }
    }

    /// Reject a proposed ADR: deletes the draft file.
    #[tool(
        description = "ADR-051 Epic C: reject a proposed ADR by deleting its draft file from .djinn/decisions/proposed/. No epic or audit record is created."
    )]
    pub async fn propose_adr_reject(
        &self,
        Parameters(p): Parameters<ProposeAdrRejectParams>,
    ) -> Json<ProposeAdrRejectResponse> {
        let root = PathBuf::from(&p.project);
        let path = proposed_file_for(&root, &p.id);
        if !path.exists() {
            return Json(ProposeAdrRejectResponse {
                ok: false,
                error: Some(format!("proposed ADR not found: {}", p.id)),
            });
        }
        match fs::remove_file(&path) {
            Ok(()) => Json(ProposeAdrRejectResponse {
                ok: true,
                error: None,
            }),
            Err(e) => Json(ProposeAdrRejectResponse {
                ok: false,
                error: Some(format!("failed to remove {}: {e}", path.display())),
            }),
        }
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(
            front.get("title").map(String::as_str),
            Some("Quoted Title")
        );
    }

    #[test]
    fn frontmatter_no_leading_dashes_returns_raw() {
        let text = "# Heading\n\nBody with no frontmatter.\n";
        let (front, body) = parse_frontmatter(text);
        assert!(front.is_empty());
        assert_eq!(body, text);
    }

    #[test]
    fn extract_title_falls_back_to_first_h1() {
        let front = BTreeMap::new();
        let body = "Some intro\n# Real Title\nmore";
        assert_eq!(extract_title(&front, body), "Real Title");
    }

    #[test]
    fn proposed_adr_from_file_reads_metadata() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = proposed_dir(tmp.path());
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("adr-999-demo.md");
        fs::write(
            &file,
            "---\ntitle: Demo ADR\nwork_shape: epic\n---\n\n# Demo ADR\n\nBody\n",
        )
        .unwrap();

        let adr = ProposedAdr::from_file(&file, true).unwrap();
        assert_eq!(adr.id, "adr-999-demo");
        assert_eq!(adr.title, "Demo ADR");
        assert_eq!(adr.work_shape.as_deref(), Some("epic"));
        assert!(adr.body.as_ref().unwrap().contains("Body"));
    }
}
