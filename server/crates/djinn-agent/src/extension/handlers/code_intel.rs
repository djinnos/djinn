use super::*;
use crate::extension::github_search;
use djinn_control_plane::bridge::{ProjectCtx, RepoGraphOps, ResolveOutcome};

/// PR C2 mirror of the MCP-side dispatcher's pre-resolve. When the chat
/// tool's caller passes a short identifier (`User`, `helper`) we go
/// through the bridge's `resolve` op so we can return a structured JSON
/// payload describing the ambiguity / hard miss instead of failing the
/// tool call.
///
/// On `Found(uid)`, mutate `params.key` (or `from`/`to`) so the
/// downstream op sees the canonical RepoNodeKey. Return `Ok(None)` to
/// continue dispatch as usual.
async fn pre_resolve_chat_key(
    graph: &dyn RepoGraphOps,
    ctx: &ProjectCtx,
    params: &mut CodeGraphParams,
) -> Result<Option<serde_json::Value>, String> {
    let single_key_ops = ["neighbors", "impact", "implementations", "describe", "context"];
    if single_key_ops.contains(&params.operation.as_str()) {
        if let Some(key) = params.key.as_deref().filter(|k| !k.is_empty()) {
            let kind_hint = params.kind_hint.as_deref();
            match graph.resolve(ctx, key, kind_hint).await? {
                ResolveOutcome::Found(uid) => {
                    params.key = Some(uid);
                }
                ResolveOutcome::Ambiguous(candidates) => {
                    return Ok(Some(serde_json::json!({ "candidates": candidates })));
                }
                ResolveOutcome::NotFound => {
                    return Ok(Some(serde_json::json!({
                        "not_found": {
                            "query": key,
                            "kind_hint": kind_hint,
                        }
                    })));
                }
            }
        }
    }

    if params.operation == "path" {
        // Validate required args BEFORE the resolve loop so a missing
        // `to` (or `from`) returns the user-facing arg-validation error,
        // not whatever the bridge stub happened to say. The dispatch
        // arm at `match params.operation` would also catch this — but
        // by the time we get there, `graph.resolve` has already been
        // called for whichever field IS present, propagating any
        // bridge error and masking the real problem.
        if params.from.as_deref().filter(|s| !s.is_empty()).is_none() {
            return Err("'from' is required for 'path'".to_string());
        }
        if params.to.as_deref().filter(|s| !s.is_empty()).is_none() {
            return Err("'to' is required for 'path'".to_string());
        }
        for which in ["from", "to"] {
            // After the validation above both are guaranteed Some/non-empty.
            let key = match which {
                "from" => params.from.as_deref().expect("validated above"),
                _ => params.to.as_deref().expect("validated above"),
            };
            let kind_hint = params.kind_hint.as_deref();
            match graph.resolve(ctx, key, kind_hint).await? {
                ResolveOutcome::Found(uid) => {
                    if which == "from" {
                        params.from = Some(uid);
                    } else {
                        params.to = Some(uid);
                    }
                }
                ResolveOutcome::Ambiguous(candidates) => {
                    return Ok(Some(serde_json::json!({ "candidates": candidates })));
                }
                ResolveOutcome::NotFound => {
                    return Ok(Some(serde_json::json!({
                        "not_found": {
                            "query": key,
                            "kind_hint": kind_hint,
                        }
                    })));
                }
            }
        }
    }

    Ok(None)
}

pub(crate) async fn call_lsp(
    state: &AgentContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
    worktree_path: &Path,
) -> Result<serde_json::Value, String> {
    let p: LspParams = parse_args(arguments)?;
    validate_symbol_only_params(p.operation.as_str(), &p)?;
    let path = resolve_path(&p.file_path, worktree_path);

    match p.operation.as_str() {
        "hover" => {
            let result = match (&p.symbol, p.line, p.character) {
                (Some(symbol), None, None) => {
                    state.lsp.hover_symbol(worktree_path, &path, symbol).await?
                }
                (None, Some(line), Some(character)) => {
                    // LSP uses 0-based positions; accept 1-based from agents
                    state
                        .lsp
                        .hover(
                            worktree_path,
                            &path,
                            line.saturating_sub(1),
                            character.saturating_sub(1),
                        )
                        .await?
                }
                (Some(_), Some(_), _) | (Some(_), _, Some(_)) => {
                    return Err(
                        "hover accepts either symbol or line+character, but not both".to_string(),
                    );
                }
                (None, Some(_), None) | (None, None, Some(_)) => {
                    return Err(
                        "hover requires both line and character when symbol is omitted".to_string(),
                    );
                }
                (None, None, None) => {
                    return Err("hover requires either symbol or line+character".to_string());
                }
            };
            Ok(serde_json::json!({ "operation": "hover", "result": result }))
        }
        "definition" => {
            let result = match (&p.symbol, p.line, p.character) {
                (Some(symbol), None, None) => {
                    state
                        .lsp
                        .go_to_definition_symbol(worktree_path, &path, symbol)
                        .await?
                }
                (None, Some(line), Some(character)) => {
                    state
                        .lsp
                        .go_to_definition(
                            worktree_path,
                            &path,
                            line.saturating_sub(1),
                            character.saturating_sub(1),
                        )
                        .await?
                }
                (Some(_), Some(_), _) | (Some(_), _, Some(_)) => {
                    return Err(
                        "definition accepts either symbol or line+character, but not both"
                            .to_string(),
                    );
                }
                (None, Some(_), None) | (None, None, Some(_)) => {
                    return Err(
                        "definition requires both line and character when symbol is omitted"
                            .to_string(),
                    );
                }
                (None, None, None) => {
                    return Err("definition requires either symbol or line+character".to_string());
                }
            };
            Ok(serde_json::json!({ "operation": "definition", "result": result }))
        }
        "references" => {
            let result = match (&p.symbol, p.line, p.character) {
                (Some(symbol), None, None) => {
                    state
                        .lsp
                        .find_references_symbol(worktree_path, &path, symbol)
                        .await?
                }
                (None, Some(line), Some(character)) => {
                    state
                        .lsp
                        .find_references(
                            worktree_path,
                            &path,
                            line.saturating_sub(1),
                            character.saturating_sub(1),
                        )
                        .await?
                }
                (Some(_), Some(_), _) | (Some(_), _, Some(_)) => {
                    return Err(
                        "references accepts either symbol or line+character, but not both"
                            .to_string(),
                    );
                }
                (None, Some(_), None) | (None, None, Some(_)) => {
                    return Err(
                        "references requires both line and character when symbol is omitted"
                            .to_string(),
                    );
                }
                (None, None, None) => {
                    return Err("references requires either symbol or line+character".to_string());
                }
            };
            Ok(serde_json::json!({ "operation": "references", "result": result }))
        }
        "symbols" => {
            let query = SymbolQuery {
                depth: p.depth,
                kinds: p
                    .kind
                    .as_deref()
                    .map(parse_symbol_kind_filter)
                    .transpose()?,
                name_filter: p.name_filter,
            };
            let result = state
                .lsp
                .document_symbols(worktree_path, &path, query)
                .await?;
            Ok(serde_json::json!({ "operation": "symbols", "result": result }))
        }
        other => Err(format!(
            "unknown LSP operation: {other}. Use: hover, definition, references, or symbols"
        )),
    }
}

pub(crate) async fn call_code_graph(
    state: &AgentContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
    project_id: &str,
    project_path: &str,
) -> Result<serde_json::Value, String> {
    let mut p: CodeGraphParams = parse_args(arguments)?;
    let mcp_state = state.to_mcp_state();
    let graph_ops = mcp_state.repo_graph();
    // Build the resolved ProjectCtx once; pass by reference to each op.
    // We ignore any caller-supplied `project_path` in `p` — the task's
    // resolved project_id + its canonical clone path are authoritative.
    let ctx = djinn_control_plane::bridge::ProjectCtx {
        id: project_id.to_string(),
        clone_path: project_path.to_string(),
    };

    // PR C2: pre-resolve key-bearing ops so the chat tool surfaces
    // `Ambiguous` / `NotFound` as structured JSON the model can act on,
    // instead of failing the call with a generic "not found" string.
    if let Some(short_circuit) =
        pre_resolve_chat_key(graph_ops.as_ref(), &ctx, &mut p).await?
    {
        return Ok(short_circuit);
    }

    let result: serde_json::Value = match p.operation.as_str() {
        "neighbors" => {
            let key = p
                .key
                .as_deref()
                .filter(|k| !k.is_empty())
                .ok_or("'key' is required for 'neighbors'")?;
            let neighbors = graph_ops
                .neighbors(
                    &ctx,
                    key,
                    p.direction.as_deref(),
                    p.group_by.as_deref(),
                    p.kind_filter.as_deref(),
                )
                .await?;
            serde_json::to_value(&neighbors).map_err(|e| format!("serialize error: {e}"))?
        }
        "ranked" => {
            let limit = p.limit.unwrap_or(20);
            let ranked = graph_ops
                .ranked(
                    &ctx,
                    p.kind_filter.as_deref(),
                    p.sort_by.as_deref(),
                    limit,
                )
                .await?;
            serde_json::to_value(&ranked).map_err(|e| format!("serialize error: {e}"))?
        }
        "implementations" => {
            let key = p
                .key
                .as_deref()
                .filter(|k| !k.is_empty())
                .ok_or("'key' is required for 'implementations'")?;
            let impls = graph_ops.implementations(&ctx, key).await?;
            serde_json::to_value(&impls).map_err(|e| format!("serialize error: {e}"))?
        }
        "impact" => {
            let key = p
                .key
                .as_deref()
                .filter(|k| !k.is_empty())
                .ok_or("'key' is required for 'impact'")?;
            // v8: lowered default depth from 3 to 2. At depth=3, FileReference
            // compounds through hub files (cmd/deps.go, cmd/dispatcher.go in
            // a typical Go service; mod.rs / lib.rs in Rust) and the impact
            // set effectively becomes "the whole runtime". Depth 2 still
            // catches the dependency-of-a-dependency that matters for "what
            // breaks if I change this", without the third hop's noise. Power
            // users can still pass `limit: 3+` explicitly.
            let depth = p.limit.unwrap_or(2);
            // PR A2: validate `min_confidence` in `[0, 1]` before forwarding
            // so chat-tool callers get a clear error instead of silent zero
            // results.
            if let Some(c) = p.min_confidence
                && !(0.0..=1.0).contains(&c)
            {
                return Err(format!(
                    "invalid min_confidence {c}: must be in [0.0, 1.0]"
                ));
            }
            let impact = graph_ops
                .impact(
                    &ctx,
                    key,
                    depth,
                    p.group_by.as_deref(),
                    p.min_confidence,
                )
                .await?;
            serde_json::to_value(&impact).map_err(|e| format!("serialize error: {e}"))?
        }
        "search" => {
            let query = p
                .query
                .as_deref()
                .filter(|q| !q.is_empty())
                .ok_or("'query' is required for 'search'")?;
            let limit = p.limit.unwrap_or(20);
            // PR B4: dispatch on `mode`. The default lives in
            // `DJINN_CODE_GRAPH_SEARCH_DEFAULT_MODE` (env var), which
            // ships as `"name"` until the hybrid soak window closes.
            let mode = match p.mode.as_deref() {
                Some(value) => value.to_string(),
                None => std::env::var("DJINN_CODE_GRAPH_SEARCH_DEFAULT_MODE")
                    .unwrap_or_else(|_| "name".to_string()),
            };
            let hits = match mode.as_str() {
                "name" => {
                    graph_ops
                        .search(&ctx, query, p.kind_filter.as_deref(), limit)
                        .await?
                }
                "hybrid" => {
                    graph_ops
                        .hybrid_search(&ctx, query, p.kind_filter.as_deref(), limit)
                        .await?
                }
                other => {
                    return Err(format!(
                        "invalid search mode '{other}': expected 'name' or 'hybrid'"
                    ));
                }
            };
            // v8: when hybrid returns nothing, wrap with a diagnostic
            // payload explaining WHY (semantic index unavailable, etc.)
            // instead of an opaque `[]`. Empty `name` results don't get
            // wrapped because the failure mode is just "no name match"
            // — clients understand that. The UI's `unwrapList(value,
            // 'hits')` handles both the array shape and the wrapped
            // `{ hits: [...] }` shape, so this is non-breaking.
            if hits.is_empty() && mode == "hybrid" {
                serde_json::json!({
                    "hits": [],
                    "diagnostic": hybrid_search_diagnostic(query),
                })
            } else {
                serde_json::to_value(&hits).map_err(|e| format!("serialize error: {e}"))?
            }
        }
        "cycles" => {
            let min_size = p.min_size.unwrap_or(2);
            // v8: default kind_filter to "symbol" when unspecified.
            // The raw graph always contains tautological file↔symbol
            // 2-cycles (every symbol forms one with its containing file
            // via ContainsDefinition + DeclaredInFile), which drown
            // out real dependency cycles. Power users can pass
            // kind_filter="file" for file-level import cycles, or
            // kind_filter=null explicitly via the underlying bridge
            // for the mixed view.
            let kind_filter = p.kind_filter.as_deref().or(Some("symbol"));
            let cycles = graph_ops
                .cycles(&ctx, kind_filter, min_size)
                .await?;
            serde_json::to_value(&cycles).map_err(|e| format!("serialize error: {e}"))?
        }
        "orphans" => {
            let limit = p.limit.unwrap_or(50);
            let orphans = graph_ops
                .orphans(
                    &ctx,
                    p.kind_filter.as_deref(),
                    p.visibility.as_deref(),
                    limit,
                )
                .await?;
            serde_json::to_value(&orphans).map_err(|e| format!("serialize error: {e}"))?
        }
        "path" => {
            let from = p
                .from
                .as_deref()
                .filter(|s| !s.is_empty())
                .ok_or("'from' is required for 'path'")?;
            let to =
                p.to.as_deref()
                    .filter(|s| !s.is_empty())
                    .ok_or("'to' is required for 'path'")?;
            let path = graph_ops
                .path(&ctx, from, to, p.max_depth)
                .await?;
            serde_json::to_value(&path).map_err(|e| format!("serialize error: {e}"))?
        }
        "edges" => {
            let from_glob = p
                .from_glob
                .as_deref()
                .filter(|s| !s.is_empty())
                .ok_or("'from_glob' is required for 'edges'")?;
            let to_glob = p
                .to_glob
                .as_deref()
                .filter(|s| !s.is_empty())
                .ok_or("'to_glob' is required for 'edges'")?;
            let limit = p.limit.unwrap_or(100);
            let edges = graph_ops
                .edges(
                    &ctx,
                    from_glob,
                    to_glob,
                    p.edge_kind.as_deref(),
                    limit,
                )
                .await?;
            serde_json::to_value(&edges).map_err(|e| format!("serialize error: {e}"))?
        }
        "describe" => {
            let key = p
                .key
                .as_deref()
                .filter(|k| !k.is_empty())
                .ok_or("'key' is required for 'describe'")?;
            let description = graph_ops.describe(&ctx, key).await?;
            serde_json::to_value(&description).map_err(|e| format!("serialize error: {e}"))?
        }
        "context" => {
            // PR C1: 360° symbol view. Default include_content=false to
            // keep wire size bounded — chat callers rarely need the body
            // on the first hop.
            let key = p
                .key
                .as_deref()
                .filter(|k| !k.is_empty())
                .ok_or("'key' is required for 'context'")?;
            let include_content = p.include_content.unwrap_or(false);
            match graph_ops.context(&ctx, key, include_content).await? {
                Some(symbol_context) => {
                    // Wrap in the same `symbol_context` discriminator the
                    // MCP dispatcher emits so downstream parsers stay
                    // consistent.
                    serde_json::json!({ "symbol_context": symbol_context })
                }
                None => serde_json::json!({
                    "not_found": { "query": key, "kind_hint": p.kind_hint }
                }),
            }
        }
        "api_surface" => {
            // v8: list public symbols with fan-in / fan-out + a
            // used-outside-crate signal. Trait method already exists;
            // this is just dispatch wiring.
            let limit = p.limit.unwrap_or(50);
            let result = graph_ops
                .api_surface(
                    &ctx,
                    p.from_glob.as_deref(),
                    p.visibility.as_deref(),
                    limit,
                )
                .await?;
            serde_json::to_value(&result).map_err(|e| format!("serialize error: {e}"))?
        }
        "metrics_at" => {
            // v8: scalar graph snapshot — node/edge counts, cycles,
            // god-object floor, orphan count, public-API and
            // documentation coverage. Cheap enough to call any time;
            // no graph load if cached.
            let result = graph_ops.metrics_at(&ctx).await?;
            serde_json::to_value(&result).map_err(|e| format!("serialize error: {e}"))?
        }
        "dead_symbols" => {
            // v8: stricter sibling of `orphans`. Tiered by caller-
            // confidence (`high`/`med`/`low`); high = no incoming
            // refs from any entry-point reachable scope.
            let confidence = p.kind_filter.as_deref().unwrap_or("high");
            let limit = p.limit.unwrap_or(50);
            let result = graph_ops.dead_symbols(&ctx, confidence, limit).await?;
            serde_json::to_value(&result).map_err(|e| format!("serialize error: {e}"))?
        }
        "deprecated_callers" => {
            // v8: surface symbols whose signature/documentation
            // carries `#[deprecated]` / `@deprecated`, plus their
            // callers — actionable removal target list.
            let limit = p.limit.unwrap_or(50);
            let result = graph_ops.deprecated_callers(&ctx, limit).await?;
            serde_json::to_value(&result).map_err(|e| format!("serialize error: {e}"))?
        }
        "touches_hot_path" => {
            // v8: given entries (e.g. `[fn main]`) + sinks (e.g. db
            // writes) + queried symbols, returns which queried
            // symbols sit on any shortest path entry → sink. Useful
            // for "does my refactor touch the hot request path?"
            let entries: Vec<String> = p
                .from_glob
                .as_deref()
                .map(|s| s.split(',').map(str::trim).filter(|s| !s.is_empty()).map(str::to_string).collect())
                .unwrap_or_default();
            let sinks: Vec<String> = p
                .to_glob
                .as_deref()
                .map(|s| s.split(',').map(str::trim).filter(|s| !s.is_empty()).map(str::to_string).collect())
                .unwrap_or_default();
            let queried: Vec<String> = p
                .query
                .as_deref()
                .map(|s| s.split(',').map(str::trim).filter(|s| !s.is_empty()).map(str::to_string).collect())
                .unwrap_or_default();
            if entries.is_empty() || sinks.is_empty() || queried.is_empty() {
                return Err(
                    "touches_hot_path requires from_glob (entries, comma-sep), \
                     to_glob (sinks, comma-sep), and query (symbols, comma-sep)"
                        .to_string(),
                );
            }
            let result = graph_ops
                .touches_hot_path(&ctx, &entries, &sinks, &queried)
                .await?;
            serde_json::to_value(&result).map_err(|e| format!("serialize error: {e}"))?
        }
        "coupling_hubs" => {
            // v8: top files by cumulative coupling across all
            // partners (sum of co_edits). High value = touching this
            // file is more likely to require touching many others.
            let limit = p.limit.unwrap_or(20);
            let since_days = p.query.as_deref().and_then(|s| s.parse::<u32>().ok());
            let result = graph_ops.coupling_hubs(&ctx, limit, since_days, 15).await?;
            serde_json::to_value(&result).map_err(|e| format!("serialize error: {e}"))?
        }
        "capabilities" => {
            // v8: introspection. Lets clients plan workflows without
            // trial-and-error against a deployment whose set of wired
            // ops, env-gated features, and indexed languages they
            // can't otherwise discover. Cheap — no graph load.
            code_graph_capabilities()
        }
        "cochange" => {
            // v8: files that change together. Routes through the
            // existing RepoGraphOps::coupling / coupling_hotspots
            // methods so the agent dispatch returns the same rich
            // shape the MCP server already exposes (with
            // supporting_commit_samples per coupled file).
            // - With `key`: top files co-edited with that one file.
            // - Without `key`: project-wide top coupled pairs.
            let limit = p.limit.unwrap_or(20);
            if let Some(key) = p.key.as_deref().filter(|k| !k.is_empty()) {
                let file_path = key.trim_start_matches("file:");
                let coupled = graph_ops.coupling(&ctx, file_path, limit).await?;
                serde_json::json!({
                    "target": file_path,
                    "coupled": coupled,
                })
            } else {
                let pairs = graph_ops
                    .coupling_hotspots(&ctx, limit, None, 15)
                    .await?;
                serde_json::json!({ "pairs": pairs })
            }
        }
        "churn" => {
            // v8: top files by distinct-commit count.
            // `query` carries an optional `since_days` integer (parsed
            // back here) so we don't add yet another field to
            // CodeGraphParams.
            let limit = p.limit.unwrap_or(20);
            let since_days = p
                .query
                .as_deref()
                .and_then(|s| s.parse::<u32>().ok());
            let rows = graph_ops.churn(&ctx, limit, since_days).await?;
            serde_json::json!({
                "since_days": since_days,
                "files": rows,
            })
        }
        "hotspots" => {
            // v8: churn × centrality, via the trait's existing
            // hotspots method. Returns HotspotEntry with `top_symbols`
            // (highest-pagerank symbol display names per file) so the
            // user gets actionable "what symbols would I touch in
            // this hotspot" without a second round-trip.
            let limit = p.limit.unwrap_or(20);
            let window_days = p
                .query
                .as_deref()
                .and_then(|s| s.parse::<u32>().ok())
                .unwrap_or(90);
            let hotspots = graph_ops
                .hotspots(&ctx, window_days, None, limit)
                .await?;
            serde_json::json!({
                "window_days": window_days,
                "hotspots": hotspots,
                "scoring": "composite_score = churn × centrality (sum of pagerank over file's symbols)",
                "next_steps": [
                    "review top hotspots for refactoring candidates",
                    "high churn + high centrality = highest blast radius if it breaks",
                ],
            })
        }
        "boundary_check" => {
            // v8: enforce architectural layering rules. Routes through
            // `RepoGraphOps::boundary_check` (single graph walk over
            // all rules, vs. one per rule the iter-11 implementation
            // did). The trait's BoundaryRule is {from_glob, to_glob}
            // — we explode each user-supplied rule's `forbid_to` list
            // into multiple BoundaryRule entries + track a mapping
            // back to the original rule for output grouping.
            use djinn_control_plane::bridge::BoundaryRule as TraitRule;
            let rules = p.rules.as_deref().unwrap_or(&[]);
            if rules.is_empty() {
                return Err(
                    "'rules' is required for 'boundary_check': pass [{name, from_glob, \
                     forbid_to: [...]}]"
                        .to_string(),
                );
            }
            // Build the flat trait-rule list + the index → user-rule
            // mapping so we can regroup violations afterwards.
            let mut flat: Vec<TraitRule> = Vec::new();
            // Maps trait-rule index → (user-rule index, matched forbid_to glob).
            let mut origin: Vec<(usize, String)> = Vec::new();
            for (rule_i, rule) in rules.iter().enumerate() {
                for forbid in &rule.forbid_to {
                    flat.push(TraitRule {
                        from_glob: rule.from_glob.clone(),
                        to_glob: forbid.clone(),
                    });
                    origin.push((rule_i, forbid.clone()));
                }
            }
            let violations = graph_ops.boundary_check(&ctx, &flat).await?;
            // Regroup violations by original user rule.
            const PER_RULE_LIMIT: usize = 100;
            let mut by_user_rule: Vec<(usize, bool, Vec<serde_json::Value>)> =
                rules.iter().map(|_| (0, false, Vec::new())).collect();
            for v in &violations {
                let (rule_i, ref forbid_glob) = origin[v.rule_index];
                let entry = &mut by_user_rule[rule_i];
                entry.0 += 1; // total count
                if entry.2.len() >= PER_RULE_LIMIT {
                    entry.1 = true; // truncated
                    continue;
                }
                entry.2.push(serde_json::json!({
                    "from": v.from_key,
                    "to": v.to_key,
                    "matched_forbid_glob": forbid_glob,
                    "edge_kind": v.edge_kind,
                    "from_file": v.from_file,
                    "to_file": v.to_file,
                }));
            }
            let mut total_violations: usize = 0;
            let report_rules: Vec<serde_json::Value> = rules
                .iter()
                .zip(by_user_rule.iter())
                .map(|(rule, (count, truncated, vs))| {
                    total_violations += count;
                    serde_json::json!({
                        "name": rule.name,
                        "from_glob": rule.from_glob,
                        "forbid_to": rule.forbid_to,
                        "violation_count": count,
                        "violations": vs,
                        "truncated": truncated,
                        "passed": *count == 0,
                    })
                })
                .collect();
            serde_json::json!({
                "rules_evaluated": rules.len(),
                "total_violations": total_violations,
                "passed": total_violations == 0,
                "rules": report_rules,
                "next_steps": if total_violations == 0 {
                    serde_json::json!(["all rules passed — no architectural violations detected"])
                } else {
                    serde_json::json!([
                        "for each violation, decide: refactor the dependency, or relax the rule",
                        "wire `boundary_check` into CI to fail on regressions",
                    ])
                },
            })
        }
        "blast_radius" => {
            // v8: first-class "what breaks if I change this" op.
            // Bundles `neighbors(incoming, group_by=file)` for direct
            // dependents and `impact(group_by=file)` for transitive,
            // then categorises each file path into runtime / test /
            // e2e buckets. Defaults: depth=2 (matches the impact
            // default; depth-3 compounds through hub files into the
            // whole runtime), no kind/edge filters.
            let key = p
                .key
                .as_deref()
                .filter(|k| !k.is_empty())
                .ok_or("'key' is required for 'blast_radius'")?;
            let depth = p.max_depth.unwrap_or(2);

            let (direct_result, transitive_result) = tokio::join!(
                graph_ops.neighbors(&ctx, key, Some("incoming"), Some("file"), None),
                graph_ops.impact(&ctx, key, depth, Some("file"), None),
            );
            let direct_groups = match direct_result? {
                djinn_control_plane::bridge::NeighborsResult::Grouped(g) => g,
                djinn_control_plane::bridge::NeighborsResult::Detailed(_) => {
                    // Unreachable: we passed group_by=file. Defensive
                    // fallback so a contract change doesn't panic.
                    Vec::new()
                }
            };
            let transitive_groups = match transitive_result? {
                djinn_control_plane::bridge::ImpactResult::Grouped(g) => g,
                djinn_control_plane::bridge::ImpactResult::Detailed(_) => Vec::new(),
            };

            // Hide the queried target itself from the transitive set —
            // depth=0 is the source node and shouldn't show up as its
            // own dependent. Also hide it from direct (defensive).
            let target_key_norm = key.trim_start_matches("file:").to_string();
            let direct_filtered: Vec<_> = direct_groups
                .into_iter()
                .filter(|g| g.file != target_key_norm)
                .collect();
            // Subtract direct dependents from transitive so the second
            // section is genuinely "deeper than depth-1".
            let direct_files: std::collections::HashSet<String> =
                direct_filtered.iter().map(|g| g.file.clone()).collect();
            let transitive_filtered: Vec<_> = transitive_groups
                .into_iter()
                .filter(|g| g.file != target_key_norm && !direct_files.contains(&g.file))
                .collect();

            serde_json::json!({
                "target": key,
                "direct": categorize_blast_groups(direct_filtered),
                "transitive": categorize_blast_groups(transitive_filtered),
                "depth": depth,
                "next_steps": [
                    "run the tests listed in `direct.tests` and `direct.e2e_tests`",
                    "review `direct.runtime` for behavioural compatibility",
                    "treat `transitive.runtime` as a deeper-review hint, not a hard breakage list",
                ],
            })
        }
        other => {
            return Err(format!(
                "unknown code_graph operation '{other}': expected one of \
                 'neighbors', 'ranked', 'impact', 'implementations', \
                 'search', 'cycles', 'orphans', 'path', 'edges', \
                 'describe', 'context', 'capabilities', 'blast_radius', \
                 'boundary_check', 'cochange', 'churn', 'hotspots', \
                 'api_surface', 'metrics_at', 'dead_symbols', \
                 'deprecated_callers', 'touches_hot_path', 'coupling_hubs'"
            ));
        }
    };
    Ok(result)
}

/// v8: classify a [`djinn_control_plane::bridge::FileGroupEntry`] list
/// into runtime / test / e2e buckets for the `blast_radius` op.
///
/// Heuristics (file-path conventional, language-aware):
/// - **e2e_test**: file is a test (see below) AND path contains
///   `/e2e/`, `/integration/`, `/system/`, or matches `tests/e2e/**` /
///   `tests/integration/**`. Run e2e separately because they're slow.
/// - **test**: basename matches `*_test.{go,rs,py,kt,scala}` OR
///   `*.test.{ts,tsx,js,jsx}` OR `*_spec.{rb,ts,tsx,js,jsx}` OR path
///   contains `/tests/` segment OR `/test/` segment OR Rust
///   `#[cfg(test)] mod tests` symbols (already filtered to file
///   `tests.rs` here). Run before merge.
/// - **runtime**: everything else. Behavioural-compatibility review
///   target.
///
/// Returns a JSON object with three keys (`runtime`, `tests`,
/// `e2e_tests`) each holding an array of `{file, occurrence_count,
/// max_depth, sample_keys}`. Order within each bucket follows input
/// order — typically pagerank-ish from the upstream impact/neighbor
/// ranking.
fn categorize_blast_groups(
    groups: Vec<djinn_control_plane::bridge::FileGroupEntry>,
) -> serde_json::Value {
    let mut runtime = Vec::new();
    let mut tests = Vec::new();
    let mut e2e_tests = Vec::new();

    for g in groups {
        let path = g.file.as_str();
        let entry = serde_json::json!({
            "file": g.file,
            "occurrence_count": g.occurrence_count,
            "max_depth": g.max_depth,
            "sample_keys": g.sample_keys,
        });
        if is_e2e_test_path(path) {
            e2e_tests.push(entry);
        } else if is_test_path(path) {
            tests.push(entry);
        } else {
            runtime.push(entry);
        }
    }

    serde_json::json!({
        "runtime": runtime,
        "tests": tests,
        "e2e_tests": e2e_tests,
        "totals": {
            "runtime": runtime.len(),
            "tests": tests.len(),
            "e2e_tests": e2e_tests.len(),
        },
    })
}

/// True for test files by file-naming and directory convention.
/// Conservative — when unsure, return false so the file falls into
/// `runtime` (which is the user's review-required bucket).
fn is_test_path(path: &str) -> bool {
    let basename = path.rsplit('/').next().unwrap_or(path);
    // Basename suffixes per language convention.
    if basename.ends_with("_test.go")
        || basename.ends_with("_test.rs")
        || basename.ends_with("_test.py")
        || basename.ends_with("_test.kt")
        || basename.ends_with("_test.scala")
        || basename.ends_with(".test.ts")
        || basename.ends_with(".test.tsx")
        || basename.ends_with(".test.js")
        || basename.ends_with(".test.jsx")
        || basename.ends_with(".test.mjs")
        || basename.ends_with("_spec.rb")
        || basename.ends_with(".spec.ts")
        || basename.ends_with(".spec.tsx")
        || basename.ends_with(".spec.js")
        || basename.ends_with(".spec.jsx")
        || basename == "tests.rs"
    {
        return true;
    }
    // Conventional dir segments. Anchored on slash so a file
    // legitimately named `protests.rs` outside such a dir passes.
    if path.contains("/tests/")
        || path.starts_with("tests/")
        || path.contains("/test/")
        || path.starts_with("test/")
        || path.contains("/__tests__/")
    {
        return true;
    }
    false
}

/// True for tests likely to be slow / require external services. e2e
/// usually runs separately from unit; surfacing it as its own bucket
/// helps reviewers plan their verification (run unit first, e2e on a
/// real env). Always also passes [`is_test_path`].
fn is_e2e_test_path(path: &str) -> bool {
    if !is_test_path(path) {
        return false;
    }
    path.contains("/e2e/")
        || path.starts_with("e2e/")
        || path.contains("/integration/")
        || path.starts_with("integration/")
        || path.contains("/system/")
        || path.contains("tests/integration/")
        || path.contains("tests/e2e/")
}

/// v8: explain why a hybrid search returned no hits. Hybrid fans out
/// to lexical (Dolt LIKE), semantic (Qdrant vector cosine), and
/// structural (canonical-graph name index) signals — when all three
/// are empty, the user otherwise sees `[]` with no signal about
/// whether the codebase is mis-indexed, the query is just unmatched,
/// or a backend (typically Qdrant) is unreachable.
///
/// We can't easily distinguish the failure modes from this layer
/// without re-running the signals, so the diagnostic is a structured
/// hint rather than a definitive cause. Surfaces:
/// - the resolved query string
/// - the hybrid-mode fan-out the search uses
/// - the most common reasons each signal returns nothing
/// - actionable next steps
fn hybrid_search_diagnostic(query: &str) -> serde_json::Value {
    serde_json::json!({
        "reason": "no hits across lexical + semantic + structural signals",
        "query": query,
        "fan_out": ["lexical (LIKE on code_chunks)", "semantic (Qdrant cosine)", "structural (canonical-graph name index)"],
        "common_causes": [
            "semantic index not built — code_chunk_embeddings warm pass hasn't run for this project",
            "Qdrant unreachable or empty for this project",
            "embedding service degraded — query embedding failed",
            "canonical graph not warmed for this project (call code_graph status to check)",
            "query genuinely has no matches",
        ],
        "next_steps": [
            "fall back to mode=name with the same query",
            "check code_graph status for warmed=true",
            "broaden the query (single keyword instead of natural language)",
        ],
    })
}

/// v8 capability-introspection payload. Returns JSON describing what
/// THIS binary actually supports — distinct from what the tool schema
/// might advertise. Cheap (no DB / graph load); safe to call from any
/// agent at any time.
///
/// Fields:
/// - `operations`: list of `operation` strings the dispatcher accepts.
/// - `default_search_mode`: the `mode` that bare `search` calls use.
/// - `available_search_modes`: every `mode` value the dispatcher routes.
/// - `env_features`: env-flag-controlled passes and their on/off state.
/// - `access_classifier_languages`: tree-sitter languages the read/write
///   classifier (v8 PR) can resolve when SCIP roles are absent.
/// - `repo_graph_artifact_version`: bincode schema stamp; mismatches
///   force a re-warm.
fn code_graph_capabilities() -> serde_json::Value {
    // env-flag readers — kept inline so this crate doesn't take a
    // dep on djinn-graph just for capability introspection.
    fn env_on(var: &str, default: bool) -> bool {
        match std::env::var(var) {
            Ok(v) => !matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "0" | "false" | "no" | "off"
            ),
            Err(_) => default,
        }
    }
    fn env_opt_in(var: &str) -> bool {
        matches!(
            std::env::var(var).ok().as_deref().map(str::trim).map(str::to_ascii_lowercase),
            Some(ref v) if matches!(v.as_str(), "1" | "true" | "on" | "yes")
        )
    }

    serde_json::json!({
        "operations": [
            "neighbors", "ranked", "impact", "implementations",
            "search", "cycles", "orphans", "path", "edges",
            "describe", "context", "capabilities", "blast_radius",
            "boundary_check", "cochange", "churn", "hotspots",
            "api_surface", "metrics_at", "dead_symbols",
            "deprecated_callers", "touches_hot_path", "coupling_hubs",
        ],
        "default_search_mode": std::env::var("DJINN_CODE_GRAPH_SEARCH_DEFAULT_MODE")
            .unwrap_or_else(|_| "name".to_string()),
        "available_search_modes": ["name", "hybrid"],
        "env_features": {
            // Defaults match the on-by-default behavior in djinn-graph.
            "entry_point_detection": env_on("DJINN_ENTRY_POINT_DETECTION", true),
            "process_detection": env_on("DJINN_PROCESS_DETECTION", true),
            "community_detection": env_on("DJINN_COMMUNITY_DETECTION", true),
            // Opt-in by design.
            "db_access_detection": env_opt_in("DJINN_DB_ACCESS_DETECTION"),
        },
        "access_classifier_languages": ["rust", "go", "python", "typescript", "javascript"],
        "repo_graph_artifact_version": 8,
        "filter_tiers": {
            "tier_1_module_artifacts": "always-on; SCIP module-tree synthetic nodes (`crate/`, `…/MODULE.`)",
            "tier_1_5_generated_and_mocks": "always-on; *.pb.go, *.gen.*, *_mock.go, mock_*.go, **/__mocks__/**, *.snap",
            "tier_2_project_globs": "from project config: graph_excluded_paths + graph_orphan_ignore",
        },
        "default_filters": {
            "ranked_excludes_externals": true,
            "neighbors_excludes_externals": true,
            "implementations_excludes_externals": true,
            "context_excludes_externals": true,
            "snapshot_excludes_externals": true,
            "impact_excludes_externals": true,
            "impact_default_max_depth": 2,
            "impact_default_min_confidence": 0.85,
            "impact_behavioral_edge_whitelist": [
                "Reads", "Writes", "SymbolReference", "FileReference",
                "Implements", "Extends", "TypeDefines", "Defines"
            ],
            "cycles_default_kind_filter": "symbol",
            "ranked_default_sort_by": "fused"
        },
    })
}

// ---------------------------------------------------------------------------
// github_search — search GitHub code via the GitHub Code Search API
// ---------------------------------------------------------------------------

pub(crate) async fn call_github_search(
    state: &AgentContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
    project_id: Option<&str>,
) -> Result<serde_json::Value, String> {
    let params: GithubSearchParams = parse_args(arguments)?;
    let installation_id = resolve_installation_id(state, project_id).await?;
    github_search::search(
        installation_id,
        &params.query,
        params.language.as_deref(),
        params.repo.as_deref(),
        params.path.as_deref(),
    )
    .await
}

// ---------------------------------------------------------------------------
// github_fetch_file — fetch a file from a GitHub repository
// ---------------------------------------------------------------------------

pub(crate) async fn call_github_fetch_file(
    state: &AgentContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
    project_id: Option<&str>,
) -> Result<serde_json::Value, String> {
    let params: GithubFetchFileParams = parse_args(arguments)?;
    let installation_id = resolve_installation_id(state, project_id).await?;
    github_search::fetch_file(
        installation_id,
        &params.repo,
        &params.path,
        params.git_ref.as_deref(),
        params.start_line,
        params.end_line,
    )
    .await
}

/// Resolve a GitHub App installation id for an agent-dispatched GitHub tool.
///
/// Worker sessions run outside the MCP request scope, so we cannot read the
/// session-user token-local. The project-scoped installation is the only
/// credential available; fail closed with an actionable error when the
/// project lacks one.
async fn resolve_installation_id(
    state: &AgentContext,
    project_id: Option<&str>,
) -> Result<u64, String> {
    let project_id = project_id.ok_or(
        "github_* tools require an active project context; dispatcher could not resolve project_id",
    )?;
    let project_repo = djinn_db::ProjectRepository::new(state.db.clone(), state.event_bus.clone());
    match project_repo.get_installation_id(project_id).await {
        Ok(Some(id)) => Ok(id),
        Ok(None) => Err(format!(
            "project {project_id} has no GitHub App installation; \
             re-register the project via the GitHub App flow to enable background GitHub tools"
        )),
        Err(e) => Err(format!(
            "failed to read installation_id for project {project_id}: {e}"
        )),
    }
}

#[cfg(test)]
mod blast_radius_categorize_tests {
    use super::*;

    #[test]
    fn classifies_unit_tests_per_language_convention() {
        // Go.
        assert!(is_test_path("internal/worker/page_worker_test.go"));
        // Rust.
        assert!(is_test_path("crates/foo/src/lib_test.rs"));
        assert!(is_test_path("crates/foo/src/tests.rs"));
        // Python.
        assert!(is_test_path("backend/handlers_test.py"));
        // Kotlin / Scala.
        assert!(is_test_path("src/main/kotlin/foo_test.kt"));
        // TS / JS.
        assert!(is_test_path("ui/src/components/Button.test.tsx"));
        assert!(is_test_path("ui/src/utils/parser.test.ts"));
        assert!(is_test_path("ui/src/utils/parser.spec.tsx"));
        assert!(is_test_path("scripts/util.test.mjs"));
        // Ruby.
        assert!(is_test_path("app/models/user_spec.rb"));
        // Conventional test dirs.
        assert!(is_test_path("tests/integration/foo.go"));
        assert!(is_test_path("crates/foo/tests/integration.rs"));
        assert!(is_test_path("test/unit/foo.py"));
        assert!(is_test_path("ui/src/__tests__/parser.ts"));
    }

    #[test]
    fn does_not_misclassify_legitimate_source_as_tests() {
        // Words containing "test" that aren't tests.
        assert!(!is_test_path("internal/handler/protests_handler.go"));
        assert!(!is_test_path("crates/contest/src/lib.rs"));
        assert!(!is_test_path("internal/util/testify_helper.go"));
        // Files literally named like a test pattern but in a non-test dir.
        // (tests.rs is a Rust convention, so it IS a test — covered above.)
        assert!(!is_test_path("internal/handler/handler.go"));
        assert!(!is_test_path("crates/foo/src/lib.rs"));
    }

    #[test]
    fn separates_e2e_from_unit_tests() {
        // E2E directory variants.
        assert!(is_e2e_test_path("tests/integration/e2e/cw_polling_e2e_test.go"));
        assert!(is_e2e_test_path("tests/e2e/auth_flow_test.go"));
        assert!(is_e2e_test_path("e2e/page_lifecycle_test.go"));
        assert!(is_e2e_test_path("integration/billing_test.go"));
        assert!(is_e2e_test_path("backend/system/system_test.py"));
        // A test that's NOT in e2e dir → not e2e (would be `test`).
        assert!(!is_e2e_test_path("internal/worker/page_worker_test.go"));
        assert!(!is_e2e_test_path("ui/src/components/Button.test.tsx"));
        // Non-test files outside any tests/ dir → not e2e (would be runtime).
        assert!(!is_e2e_test_path("internal/worker/page_worker.go"));
        assert!(!is_e2e_test_path("cmd/dispatcher.go"));
        // Note: a fixture file like `tests/e2e/fixtures.go` IS classified
        // as e2e here — it lives under `tests/` so is_test_path fires,
        // and it lives under `/e2e/` so is_e2e_test_path fires too. That's
        // the intentional behavior: fixtures shipped alongside e2e tests
        // should be in the same bucket as the e2e tests for "what should
        // I re-run" purposes.
    }

    #[test]
    fn categorize_buckets_each_path_correctly() {
        use djinn_control_plane::bridge::FileGroupEntry;
        let groups = vec![
            FileGroupEntry {
                file: "cmd/worker.go".to_string(),
                occurrence_count: 3,
                max_depth: 1,
                sample_keys: vec!["scip-go . . . StartPageWorker().".to_string()],
            },
            FileGroupEntry {
                file: "internal/worker/page_worker_test.go".to_string(),
                occurrence_count: 5,
                max_depth: 1,
                sample_keys: vec![],
            },
            FileGroupEntry {
                file: "tests/integration/e2e/cw_polling_e2e_test.go".to_string(),
                occurrence_count: 2,
                max_depth: 2,
                sample_keys: vec![],
            },
        ];
        let result = categorize_blast_groups(groups);
        assert_eq!(result["totals"]["runtime"], 1);
        assert_eq!(result["totals"]["tests"], 1);
        assert_eq!(result["totals"]["e2e_tests"], 1);
        assert_eq!(
            result["runtime"][0]["file"].as_str().unwrap(),
            "cmd/worker.go"
        );
        assert_eq!(
            result["tests"][0]["file"].as_str().unwrap(),
            "internal/worker/page_worker_test.go"
        );
        assert_eq!(
            result["e2e_tests"][0]["file"].as_str().unwrap(),
            "tests/integration/e2e/cw_polling_e2e_test.go"
        );
    }
}
