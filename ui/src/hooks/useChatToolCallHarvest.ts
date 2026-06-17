/**
 * useChatToolCallHarvest — D5 producer for the `codeGraphStore.citationIds`
 * highlight layer.
 *
 * Subscribes to the active chat session's message list, diffs by last-
 * processed-message id so streaming deltas don't re-fire, and for each
 * new assistant message that contains a `code_graph` tool call
 * (`impact` / `neighbors` / `search`, or any name that starts with
 * `code_graph`), parses the structured JSON result payload, extracts
 * the referenced symbol ids, fuzzy-resolves them against the current
 * snapshot's node-id allowlist (exact match first, then suffix
 * match), and writes the deduped union into `codeGraphStore.citationIds`
 * via `setCitations`.
 *
 * The hook is mounted at the ChatView level (not inside individual
 * ChatMessageBubble components) so it survives message-list re-renders.
 *
 * Degradation contract (per task AC):
 *   - No active session, no project, or empty snapshot → no-op.
 *   - Tool result not parseable JSON or missing expected fields →
 *     skip that call, process the rest.
 *   - `success: false` result → skip that call.
 *   - Resolved-id set is empty → no `setCitations` call.
 *
 * Out of scope (per task): prose marker protocol, citationLink-style
 * click navigation, anything server-side.
 */

import { useEffect, useRef } from "react";

import { fetchSnapshot } from "@/api/codeGraph";
import { parseSnapshotResponse } from "@/lib/codeGraphAdapter";
import { useChatStore, type ChatMessage } from "@/stores/chatStore";
import { useCodeGraphStore } from "@/stores/codeGraphStore";

// Same default the canvas uses for `fetchSnapshot`. Kept local so the
// hook doesn't pull a transitive dep on the canvas component.
const DEFAULT_SNAPSHOT_CAP = 10_000;

// ── Harvesting helpers (exported for unit testing) ───────────────────────────

/**
 * Names that count as a `code_graph` op for harvest purposes. We
 * match the bare op names the agent dispatches (`impact`,
 * `neighbors`, `search`) and also the tool's full id (`code_graph`)
 * in case the upstream surfaces the MCP tool name verbatim. Anything
 * matching the `code_graph*` prefix is folded in for forward
 * compatibility with future ops (e.g. `code_graph_search`).
 */
const CODE_GRAPH_OP_NAMES = new Set<string>(["impact", "neighbors", "search"]);

export function isCodeGraphToolCallName(name: string | undefined | null): boolean {
  if (!name) return false;
  if (CODE_GRAPH_OP_NAMES.has(name)) return true;
  if (name === "code_graph") return true;
  return name.startsWith("code_graph");
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function asString(value: unknown): string | undefined {
  return typeof value === "string" && value.length > 0 ? value : undefined;
}

function pushIfString(set: Set<string>, value: unknown): void {
  const s = asString(value);
  if (s) set.add(s);
}

/**
 * Extract id-like strings from a single tool-call result payload.
 *
 * Different ops emit slightly different wire shapes; we accept any of
 * the known ones and skip the rest:
 *
 *   - `impact`   — either `{key, impact: [{key, ...}]}` (detailed;
 *                  `parseImpactDetailed`) or `{key, file_groups:
 *                  [{sample_keys: [...]}]}`. We also seed with the
 *                  outer `key` so the symbol the agent queried lights
 *                  up alongside its blast radius.
 *   - `neighbors` — `{neighbors: [{key, ...}]}` (`parseNeighbors`).
 *   - `search`    — `{hits: [{key, ...}]}` (`parseSearchHits`).
 *
 * Field aliases: the task spec says `uid` and `key` are the canonical
 * id fields. The harvesters below prefer `uid` (matches `Candidate`
 * and `RelatedSymbol`) and fall back to `key` (matches the
 * `parse*Detailed` shape), then to any string field the record
 * exposes via the first non-empty match.
 *
 * Returns `null` when the payload is unparseable or when nothing
 * usable was extracted (caller skips the call).
 */
export function extractIdsFromToolResult(
  toolName: string,
  rawOutput: string,
): Set<string> | null {
  if (!isCodeGraphToolCallName(toolName)) return null;

  let parsed: unknown;
  try {
    parsed = JSON.parse(rawOutput);
  } catch {
    return null;
  }

  if (!isRecord(parsed)) return null;

  // Bail on explicit success:false envelopes — matches the task's
  // "`success: false` result → skip that call" requirement.
  if (parsed.success === false) return null;

  const ids = new Set<string>();

  if (toolName === "impact" || toolName === "code_graph_impact") {
    // Detailed form: { key, impact: [{key, depth, ...}], risk?, summary? }
    // Alias: some callers/legacy variants use `uid` instead of `key`.
    pushIfString(ids, parsed.key);
    pushIfString(ids, parsed.uid);
    const entries = Array.isArray(parsed.impact) ? parsed.impact : [];
    for (const entry of entries) {
      if (!isRecord(entry)) continue;
      pushIfString(ids, entry.uid);
      pushIfString(ids, entry.key);
    }
    // file_groups form (group_by=file): { file_groups: [{sample_keys: [...]}] }
    const fileGroups = Array.isArray(parsed.file_groups)
      ? parsed.file_groups
      : [];
    for (const group of fileGroups) {
      if (!isRecord(group)) continue;
      const sampleKeys = Array.isArray(group.sample_keys)
        ? group.sample_keys
        : [];
      for (const k of sampleKeys) pushIfString(ids, k);
    }
  } else if (toolName === "neighbors" || toolName === "code_graph_neighbors") {
    const neighbors = Array.isArray(parsed.neighbors) ? parsed.neighbors : [];
    for (const n of neighbors) {
      if (!isRecord(n)) continue;
      pushIfString(ids, n.uid);
      pushIfString(ids, n.key);
    }
  } else if (toolName === "search" || toolName === "code_graph_search") {
    const hits = Array.isArray(parsed.hits) ? parsed.hits : [];
    for (const hit of hits) {
      if (!isRecord(hit)) continue;
      pushIfString(ids, hit.uid);
      pushIfString(ids, hit.key);
    }
  } else {
    // Generic `code_graph*` op — best-effort sweep of the common list
    // field names. Unknown ops are tolerated rather than blocked so
    // new ops ship without a hook update.
    const candidates = [
      parsed.neighbors,
      parsed.hits,
      parsed.impact,
      parsed.entries,
      parsed.results,
      parsed.nodes,
      parsed.candidates,
    ];
    for (const list of candidates) {
      if (!Array.isArray(list)) continue;
      for (const entry of list) {
        if (!isRecord(entry)) continue;
        pushIfString(ids, entry.uid);
        pushIfString(ids, entry.key);
      }
    }
  }

  return ids.size > 0 ? ids : null;
}

/**
 * Resolve candidate ids against the live snapshot node-id allowlist.
 *
 * Two-stage match:
 *   1. Exact `validIds.has(candidate)`.
 *   2. For anything still unresolved, accept the first snapshot id
 *      whose `validIds` entry ends with the candidate suffix. This
 *      catches the common case of the model returning a short symbol
 *      key like `MyClass#` while the snapshot carries the full
 *      SCIP descriptor `scip-typescript npm my-pkg 1.0.0 src/. MyClass#`.
 *
 * Drop silently on no match (per task AC). Mutates neither input set.
 */
export function fuzzyResolveIds(
  candidates: Iterable<string>,
  validIds: ReadonlySet<string>,
): Set<string> {
  const resolved = new Set<string>();
  if (validIds.size === 0) return resolved;

  const validArray = Array.from(validIds);

  for (const rawCandidate of candidates) {
    if (!rawCandidate || !asString(rawCandidate)) continue;

    // Stage 1 — exact match.
    if (validIds.has(rawCandidate)) {
      resolved.add(rawCandidate);
      continue;
    }

    // Stage 2 — suffix match. Pick the FIRST snapshot id whose
    // tail equals the candidate; deterministic because `validArray`
    // preserves snapshot order. If multiple matches exist we keep the
    // first — the snapshot is already an allowlist so any member is a
    // reasonable proxy.
    const matched = validArray.find((id) => id.endsWith(rawCandidate));
    if (matched !== undefined) {
      resolved.add(matched);
    }
  }

  return resolved;
}

/**
 * Walk a ChatMessage's `toolCalls`, returning the union of every
 * parseable `code_graph` payload's id set. Skips calls whose result
 * is missing, unparseable, or whose `success === false`.
 */
export function harvestMessageToolCalls(message: ChatMessage): Set<string> {
  const union = new Set<string>();
  const calls = message.toolCalls;
  if (!calls || calls.length === 0) return union;

  for (const call of calls) {
    if (call.success === false) continue;
    const result = call.result;
    if (!result || typeof result.output !== "string") continue;
    const ids = extractIdsFromToolResult(call.name, result.output);
    if (!ids) continue;
    for (const id of ids) union.add(id);
  }

  return union;
}

// ── Hook ─────────────────────────────────────────────────────────────────────

export interface UseChatToolCallHarvestOptions {
  /**
   * The currently-selected project slug (`owner/repo`) or `null` when
   * the user has no project selected. When `null` the hook is a
   * no-op (no snapshot fetch, no citations written).
   */
  projectSlug: string | null;
  /**
   * Override the snapshot node cap. Defaults to 10_000 to match the
   * canvas. Pass a smaller number for tests to keep payloads small.
   */
  snapshotCap?: number;
}

/**
 * Side-effect-only hook. Mount at the ChatView level so it survives
 * message-list re-renders (per task AC).
 *
 * Algorithm:
 *   1. On mount, read `activeSessionId` once.
 *   2. Subscribe to `messagesBySession[activeSessionId]` via
 *      `useChatStore.subscribe`. Track the last-processed message id
 *      in a ref so streaming deltas don't re-fire on every token.
 *   3. When new messages arrive, harvest `code_graph` tool results,
 *      fetch the snapshot, fuzzy-resolve, and `setCitations`.
 */
export function useChatToolCallHarvest(
  options: UseChatToolCallHarvestOptions,
): void {
  const { projectSlug, snapshotCap = DEFAULT_SNAPSHOT_CAP } = options;

  // Refs that survive across renders. We deliberately avoid
  // `useChatStore((s) => s.activeSessionId)` as a render dep so the
  // hook's own re-render doesn't replay the entire history when the
  // session swaps — the spec calls for a single read on mount.
  const activeSessionIdRef = useRef<string | null>(
    useChatStore.getState().activeSessionId,
  );
  const lastProcessedMessageIdRef = useRef<string | null>(null);

  // In-flight snapshot fetch token — when projectSlug changes we
  // discard any stale resolution so a fast project switch can't
  // overwrite the fresh project's citations with the previous one's.
  const requestTokenRef = useRef(0);

  useEffect(() => {
    activeSessionIdRef.current = useChatStore.getState().activeSessionId;
    // Reset the last-processed cursor so swapping into a fresh
    // session doesn't skip messages that arrive before the next
    // subscribe tick.
    lastProcessedMessageIdRef.current = null;
  }, []);

  useEffect(() => {
    // No project → nothing to resolve against. Stay subscribed so a
    // later project selection starts harvesting immediately.
    if (!projectSlug) return;

    let cancelled = false;

    const process = async (): Promise<void> => {
      const sessionId = activeSessionIdRef.current;
      if (!sessionId) return;

      const state = useChatStore.getState();
      const messages = state.messagesBySession[sessionId] ?? [];
      if (messages.length === 0) return;

      // Use the last-processed message id as a re-fire gate: if the
      // trailing message id is unchanged there's no new content to
      // harvest, so skip the snapshot fetch entirely.
      const lastId = lastProcessedMessageIdRef.current;
      const trailingId = messages[messages.length - 1]?.id ?? null;
      if (lastId !== null && lastId === trailingId) return;

      // When there IS new content, harvest across the entire session
      // so `citationIds` reflects the accumulated union of all
      // code_graph tool results, not just the latest message.
      const harvested = new Set<string>();
      for (const message of messages) {
        // We only act on assistant turns — user messages never carry
        // tool calls.
        if (message.role !== "assistant") continue;
        const ids = harvestMessageToolCalls(message);
        for (const id of ids) harvested.add(id);
      }

      // Advance the cursor regardless of harvest outcome so a
      // no-op-call message still doesn't trigger a re-fire.
      lastProcessedMessageIdRef.current =
        messages[messages.length - 1]?.id ?? lastId;

      if (harvested.size === 0) return;

      // Token-tag the request so a stale snapshot fetch can't
      // overwrite a fresher one's result.
      const token = ++requestTokenRef.current;
      let snapshotJson: unknown;
      try {
        snapshotJson = await fetchSnapshot(projectSlug, snapshotCap);
      } catch {
        // Snapshot fetch failed — leave citations untouched. The
        // canvas will already be showing an error state.
        return;
      }
      if (cancelled || token !== requestTokenRef.current) return;

      const snapshot = parseSnapshotResponse(snapshotJson);
      if (!snapshot || snapshot.nodes.length === 0) return;

      const validIds = new Set<string>();
      for (const node of snapshot.nodes) validIds.add(node.id);

      const resolved = fuzzyResolveIds(harvested, validIds);
      if (resolved.size === 0) return;

      useCodeGraphStore.getState().setCitations(resolved);
    };

    // Subscribe to the *specific session slice* so we don't fire on
    // unrelated chat updates (other sessions, drafts, etc.). Zustand
    // lets us read state inside the listener via `get()`.
    const unsubscribe = useChatStore.subscribe((state) => {
      const sessionId = activeSessionIdRef.current;
      if (!sessionId) return;
      void state.messagesBySession[sessionId];
      // We don't gate on `state.messagesBySession[sessionId]` reference
      // equality — the listener fires for any change, then we diff
      // by last-processed id inside `process()`.
      void process();
    });

    // Kick once on mount in case the active session already has
    // finished messages.
    void process();

    return () => {
      cancelled = true;
      unsubscribe();
    };
  }, [projectSlug, snapshotCap]);
}
