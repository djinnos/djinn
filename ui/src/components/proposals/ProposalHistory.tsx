import { useMemo, useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { HugeiconsIcon } from "@hugeicons/react";
import { ArrowDown01Icon } from "@hugeicons/core-free-icons";
import { usersQueryOptions } from "@/api/queryOptions";
import type { ProposalLintResult } from "@/api/types";
import { userDisplayName, type OrgUser } from "@/api/users";
import { UserAvatar } from "@/components/UserAvatar";
import { DiffView } from "@/components/proposals/DiffView";
import { relativeTime } from "@/components/memory/memoryUtils";
import { Badge } from "@/components/ui/badge";
import { Label } from "@/components/ui/label";
import { cn } from "@/lib/utils";
import type {
  ProposalDetail,
  ProposalHistoryEntry,
} from "@/lib/proposalQueries";

type SkippedTierDetail = ProposalLintResult["skipped_tiers"][number] & {
  message?: string | null;
};

function revisionLint(revision: ProposalHistoryEntry): ProposalLintResult | null {
  return revision.lint ?? null;
}

/** Render only the immutable lint result carried by this exact revision. */
function LintBadges({ lint }: { lint: ProposalLintResult | null }) {
  if (!lint) return null;

  const errors = lint.errors?.length ?? 0;
  const warnings = lint.warnings?.length ?? 0;
  const skipped = lint.skipped_tiers?.length ?? 0;
  if (errors === 0 && warnings === 0 && skipped === 0) return null;

  return (
    <>
      {errors > 0 && (
        <Badge variant="destructive">
          {errors} error{errors === 1 ? "" : "s"}
        </Badge>
      )}
      {warnings > 0 && (
        <Badge variant="secondary">
          {warnings} warning{warnings === 1 ? "" : "s"}
        </Badge>
      )}
      {skipped > 0 && <Badge variant="outline">{skipped} skipped</Badge>}
    </>
  );
}

/** Server diagnostics are deliberately rendered verbatim, including unknown codes. */
function LintDetails({ lint }: { lint: ProposalLintResult | null }) {
  if (!lint) return null;
  const violations = [...(lint.errors ?? []), ...(lint.warnings ?? [])];
  const skippedTiers = lint.skipped_tiers ?? [];
  if (violations.length === 0 && skippedTiers.length === 0) return null;

  return (
    <section
      aria-label="Lint diagnostics"
      className="space-y-2 rounded-md border border-border/60 p-2 text-xs"
    >
      <p className="font-medium">Lint diagnostics</p>
      {violations.length > 0 && (
        <ul className="space-y-1">
          {violations.map((violation, index) => (
            <li
              key={`${violation.severity}-${violation.code}-${index}`}
              className="rounded border border-border/60 p-2"
            >
              <span className="font-medium">{violation.severity}</span>{" · "}
              <span className="font-mono">{violation.code}</span>{": "}
              <span>{violation.message}</span>
              <span className="block text-muted-foreground">
                bytes [{violation.span.start}, {violation.span.end})
              </span>
            </li>
          ))}
        </ul>
      )}
      {skippedTiers.length > 0 && (
        <ul className="space-y-1">
          {(skippedTiers as SkippedTierDetail[]).map((tier, index) => (
            <li
              key={`${tier.tier}-${tier.reason}-${index}`}
              className="rounded border border-border/60 p-2"
            >
              <span className="font-medium">Skipped tier</span>{": "}
              <span className="font-mono">{tier.tier}</span>{" · "}
              <span>{tier.reason}</span>
              {tier.message && <span>{": "}{tier.message}</span>}
            </li>
          ))}
        </ul>
      )}
    </section>
  );
}

function revisionBodyFormat(revision: ProposalHistoryEntry): string {
  const bodyFormat = (revision as { body_format?: unknown }).body_format;
  return typeof bodyFormat === "string" && bodyFormat.trim()
    ? bodyFormat.trim().toLowerCase()
    : "markdown";
}

function truncatePreview(text: string, maxLength = 140): string {
  const normalized = text.replace(/\s+/g, " ").trim();
  if (normalized.length <= maxLength) return normalized;

  const clipped = normalized.slice(0, maxLength - 1);
  const lastBoundary = Math.max(
    clipped.lastIndexOf(" "),
    clipped.lastIndexOf("."),
    clipped.lastIndexOf(","),
  );
  const safeClip = lastBoundary >= 60 ? clipped.slice(0, lastBoundary) : clipped;
  return `${safeClip.trimEnd()}…`;
}

function revisionBodyPreview(revision: ProposalHistoryEntry): string {
  const body = (revision.body ?? "").trim();
  if (!body) return "No body.";

  const format = revisionBodyFormat(revision);
  if (format !== "mdx") return truncatePreview(body);

  const blockTags = Array.from(
    body.matchAll(/<([A-Z][A-Za-z0-9]*)(?:\s[^>]*)?>/g),
    (match) => match[1],
  );
  const uniqueBlockTags = [...new Set(blockTags)];
  const textPreview = truncatePreview(
    body.replace(/<\/?[A-Z][A-Za-z0-9]*(?:\s[^>]*)?>/g, " "),
  );

  if (uniqueBlockTags.length === 0) return truncatePreview(body);

  const blockSummary = `MDX blocks: ${uniqueBlockTags.join(", ")}`;
  return textPreview ? `${blockSummary} · ${textPreview}` : blockSummary;
}

/** One entry in an AC-amendment audit list (from `proposal_ac_amend`). */
type AmendmentEntry = {
  operation?: string;
  index?: number;
  old_criterion?: unknown;
  new_criterion?: unknown;
};

/** Parsed `event_metadata` (it arrives as a JSON string on the wire). */
function parsedMeta(r: ProposalHistoryEntry): {
  source?: string;
  round?: number;
  kind?: string;
  reason?: string;
  amendments?: AmendmentEntry[];
} | null {
  const meta = (r as { event_metadata?: unknown }).event_metadata;
  if (!meta) return null;
  try {
    return typeof meta === "string" ? JSON.parse(meta) : (meta as object);
  } catch {
    return null;
  }
}

/** A spec revision produced by the autonomous refinement tribunal. */
function isRefinementRevision(r: ProposalHistoryEntry): boolean {
  return r.event_kind === "spec_revision" && parsedMeta(r)?.source === "refinement_loop";
}

/** A spec revision that carries an acceptance-criteria amendment audit. */
function isAcAmendmentRevision(r: ProposalHistoryEntry): boolean {
  return r.event_kind === "spec_revision" && parsedMeta(r)?.kind === "ac_amendment";
}

/** Human-readable criterion text from a criterion value (string or object). */
function criterionText(value: unknown): string {
  if (typeof value === "string") return value;
  if (value && typeof value === "object") {
    const criterion = (value as Record<string, unknown>).criterion;
    if (typeof criterion === "string") return criterion;
  }
  return "";
}

/** Short verb for an amendment operation ("rewritten" / "waived" / "dropped"). */
function amendmentVerb(operation: string | undefined): string {
  switch (operation) {
    case "rewrite":
      return "rewritten";
    case "waive":
      return "waived";
    case "drop":
      return "dropped";
    default:
      return operation ?? "changed";
  }
}

/**
 * One row in the rendered timeline. A whole tribunal run (its contiguous
 * `refinement_loop` revisions) collapses into a single `refinement` row whose
 * diff is the pre-refinement snapshot → the converged head.
 */
type HistoryRow =
  | { type: "status"; entry: ProposalHistoryEntry; at: string }
  | { type: "rev"; entry: ProposalHistoryEntry; at: string }
  | {
      type: "refinement";
      head: ProposalHistoryEntry;
      before?: ProposalHistoryEntry;
      fromSeq: number;
      rounds: number;
      at: string;
    };

/**
 * Revision history for a proposal's spec. Every material edit (title/body/AC)
 * appends a full snapshot to `proposal_revisions`; this lists them newest-first
 * with the editor + timestamp, and expands to a diff against the prior revision.
 * A tribunal refinement run is collapsed into one "Refined via tribunal" entry.
 */
export function ProposalHistory({ detail }: { detail: ProposalDetail }) {
  const proposal = detail.proposal!;
  const usersQuery = useQuery(usersQueryOptions());
  const userFor = (id?: string | null) =>
    id ? (usersQuery.data ?? []).find((u: OrgUser) => u.id === id) : undefined;

  const history = useMemo(
    () =>
      [...detail.revisions].sort(
        (a, b) =>
          b.created_at.localeCompare(a.created_at) ||
          b.seq - a.seq ||
          b.id.localeCompare(a.id),
      ),
    [detail.revisions],
  );
  const specRevisions = useMemo(
    () => detail.revisions.filter((r) => r.event_kind === "spec_revision"),
    [detail.revisions],
  );
  // Each spec revision diffs against the one immediately before it (seq − 1).
  const bySeq = useMemo(() => {
    const m = new Map<number, (typeof specRevisions)[number]>();
    specRevisions.forEach((r) => m.set(r.seq, r));
    return m;
  }, [specRevisions]);

  const [open, setOpen] = useState<string[]>([]);
  const toggle = (id: string) =>
    setOpen((cur) =>
      cur.includes(id) ? cur.filter((s) => s !== id) : [...cur, id],
    );

  // Only genuine status transitions count as history rows (refinement
  // lifecycle events carry no transition and are dropped at render time).
  const statusEvents = history.filter(
    (r) => r.event_kind !== "spec_revision" && (r.status_from || r.status_to),
  );

  // Build the rendered timeline, collapsing each contiguous run of tribunal
  // `refinement_loop` revisions into one "Refined via tribunal" row.
  const rows = useMemo<HistoryRow[]>(() => {
    const asc = [...specRevisions].sort((a, b) => a.seq - b.seq);
    const out: HistoryRow[] = [];
    let i = 0;
    while (i < asc.length) {
      if (isRefinementRevision(asc[i])) {
        let j = i;
        while (j < asc.length && isRefinementRevision(asc[j])) j++;
        const group = asc.slice(i, j);
        const head = group[group.length - 1];
        const fromSeq = group[0].seq;
        const rounds =
          new Set(
            group
              .map((g) => parsedMeta(g)?.round)
              .filter((x): x is number => typeof x === "number"),
          ).size || group.length;
        out.push({
          type: "refinement",
          head,
          before: bySeq.get(fromSeq - 1),
          fromSeq,
          rounds,
          at: head.created_at,
        });
        i = j;
      } else {
        out.push({ type: "rev", entry: asc[i], at: asc[i].created_at });
        i++;
      }
    }
    statusEvents.forEach((e) => out.push({ type: "status", entry: e, at: e.created_at }));
    out.sort(
      (a, b) => b.at.localeCompare(a.at) || (b.type === "status" ? -1 : 1),
    );
    return out;
  }, [specRevisions, statusEvents, bySeq]);

  // A lone seed revision has nothing to compare against yet, but status audit
  // events still need to be visible even when no material spec edit exists.
  if (specRevisions.length <= 1 && statusEvents.length === 0) return null;

  return (
    <div className="space-y-2">
      <Label className="text-xs uppercase text-muted-foreground">
        Revision history
      </Label>
      <ul className="divide-y rounded-md border">
        {rows.map((row) => {
          // ── Collapsed tribunal run ──────────────────────────────────────
          if (row.type === "refinement") {
            const { head, before, fromSeq, rounds } = row;
            const isHead = head.seq === proposal.latest_revision_seq;
            const isOpen = open.includes(head.id);
            const rangeLabel =
              head.seq === fromSeq ? `rev ${head.seq}` : `rev ${fromSeq}–${head.seq}`;
            return (
              <li key={`refine-${head.id}`}>
                <button
                  onClick={() => toggle(head.id)}
                  className="flex w-full items-center gap-2 px-3 py-2 text-left text-sm hover:bg-muted/40"
                >
                  <HugeiconsIcon
                    icon={ArrowDown01Icon}
                    size={14}
                    className={cn(
                      "shrink-0 text-muted-foreground transition-transform",
                      isOpen && "rotate-180",
                    )}
                  />
                  <Badge variant={isHead ? "default" : "outline"} className="font-mono">
                    {rangeLabel}
                  </Badge>
                  <Badge variant="secondary">tribunal</Badge>
                  <LintBadges lint={revisionLint(head)} />
                  <span className="font-medium">
                    Refined via tribunal ({rounds} {rounds === 1 ? "round" : "rounds"})
                  </span>
                  {isHead && (
                    <span className="text-xs text-muted-foreground">current</span>
                  )}
                  <span className="min-w-0 flex-1 truncate text-xs text-muted-foreground">
                    {revisionBodyPreview(head)}
                  </span>
                  <time
                    dateTime={head.created_at}
                    title={head.created_at}
                    className="ml-auto shrink-0 text-xs text-muted-foreground"
                  >
                    {relativeTime(head.created_at)}
                  </time>
                </button>
                {isOpen && (
                  <div className="space-y-2 px-3 pb-3">
                    <p className="text-xs text-muted-foreground">
                      The autonomous Adversary → Advocate → Judge tribunal revised
                      the spec over {rounds} {rounds === 1 ? "round" : "rounds"};
                      diff is your original → the converged result.
                    </p>
                    <DiffView before={before?.body ?? ""} after={head.body ?? ""} />
                    <LintDetails lint={revisionLint(head)} />
                  </div>
                )}
              </li>
            );
          }

          const r = row.entry;
          const prev = bySeq.get(r.seq - 1);
          const editor = userFor(r.edited_by_user_id);
          const isHead = r.seq === proposal.latest_revision_seq;
          const isOpen = open.includes(r.id);
          const isSpecRevision = r.event_kind === "spec_revision";
          const bodyFormat = revisionBodyFormat(r);
          const titleChanged = !!prev && prev.title !== r.title;

          // ── Acceptance-criteria amendment ───────────────────────────────
          // A spec revision whose only change is an AC amendment (via
          // `proposal_ac_amend`). Its body is unchanged, so render the reason
          // and a human-readable list of operations instead of a body diff.
          if (isAcAmendmentRevision(r)) {
            const meta = parsedMeta(r);
            const reason = meta?.reason?.trim() ?? "";
            const amendments = meta?.amendments ?? [];
            const summary =
              amendments
                .map((a) => `criterion ${(a.index ?? 0) + 1} ${amendmentVerb(a.operation)}`)
                .join(", ") || "acceptance criteria amended";
            return (
              <li key={r.id}>
                <button
                  onClick={() => toggle(r.id)}
                  className="flex w-full items-center gap-2 px-3 py-2 text-left text-sm hover:bg-muted/40"
                >
                  <HugeiconsIcon
                    icon={ArrowDown01Icon}
                    size={14}
                    className={cn(
                      "shrink-0 text-muted-foreground transition-transform",
                      isOpen && "rotate-180",
                    )}
                  />
                  <Badge
                    variant={isHead ? "default" : "outline"}
                    className="font-mono"
                  >
                    rev {r.seq}
                  </Badge>
                  <Badge variant="secondary">AC amendment</Badge>
                  <LintBadges lint={revisionLint(r)} />
                  {isHead && (
                    <span className="text-xs text-muted-foreground">current</span>
                  )}
                  <span className="flex min-w-0 items-center gap-1.5">
                    <UserAvatar user={editor} className="size-4" />
                    <span className="truncate">
                      {editor
                        ? userDisplayName(editor)
                        : r.edited_by_user_id
                          ? "unknown"
                          : "—"}
                    </span>
                  </span>
                  <span className="min-w-0 flex-1 truncate text-xs text-muted-foreground">
                    {summary}
                  </span>
                  <time
                    dateTime={r.created_at}
                    title={r.created_at}
                    className="ml-auto shrink-0 text-xs text-muted-foreground"
                  >
                    {relativeTime(r.created_at)}
                  </time>
                </button>
                {isOpen && (
                  <div className="space-y-2 px-3 pb-3">
                    {reason && (
                      <p className="text-xs text-muted-foreground">
                        Reason:{" "}
                        <span className="text-foreground">{reason}</span>
                      </p>
                    )}
                    {amendments.length === 0 ? (
                      <p className="text-xs text-muted-foreground">
                        Acceptance criteria amended.
                      </p>
                    ) : (
                      <ul className="space-y-1 text-xs">
                        {amendments.map((a, i) => {
                          const oldText = criterionText(a.old_criterion);
                          const newText = criterionText(a.new_criterion);
                          const showNew =
                            a.operation === "rewrite" && !!newText && newText !== oldText;
                          return (
                            <li
                              key={i}
                              className="rounded-md border border-border/60 px-2 py-1.5"
                            >
                              <span className="font-medium">
                                Criterion {(a.index ?? 0) + 1} {amendmentVerb(a.operation)}
                              </span>
                              {oldText && (
                                <span className="block text-muted-foreground">
                                  <span
                                    className={cn(showNew && "line-through")}
                                  >
                                    {oldText}
                                  </span>
                                  {showNew && (
                                    <>
                                      {" → "}
                                      <span className="text-foreground">
                                        {newText}
                                      </span>
                                    </>
                                  )}
                                </span>
                              )}
                            </li>
                          );
                        })}
                      </ul>
                    )}
                    <LintDetails lint={revisionLint(r)} />
                  </div>
                )}
              </li>
            );
          }

          if (!isSpecRevision) {
            // Only genuine status transitions belong in the spec revision
            // history. Refinement lifecycle events (refinement_start/stop) are
            // a separate concern shown in the Refinement panel, and checkpoint
            // reverts carry no transition — drop them all so this section stays
            // about the spec itself.
            if (!(r.status_from || r.status_to)) return null;
            const statusLabel =
              r.status_to === "done"
                ? "Marked done (implemented externally)"
                : "Status changed";
            return (
              <li key={r.id}>
                <div className="flex w-full items-center gap-2 px-3 py-2 text-sm">
                  <Badge variant="secondary" className="font-mono">
                    status
                  </Badge>
                  <span className="font-medium">{statusLabel}</span>
                  <span className="text-muted-foreground">(</span>
                  <span className="font-medium">{r.status_from ?? "—"}</span>
                  <span className="text-muted-foreground">→</span>
                  <span className="font-medium">{r.status_to ?? "—"}</span>
                  <span className="text-muted-foreground">)</span>
                  <span className="flex min-w-0 items-center gap-1.5">
                    <UserAvatar user={editor} className="size-4" />
                    <span className="truncate">
                      {editor
                        ? userDisplayName(editor)
                        : r.edited_by_user_id
                          ? "unknown"
                          : "—"}
                    </span>
                  </span>
                  <time
                    dateTime={r.created_at}
                    title={r.created_at}
                    className="ml-auto shrink-0 text-xs text-muted-foreground"
                  >
                    {relativeTime(r.created_at)}
                  </time>
                </div>
              </li>
            );
          }
          return (
            <li key={r.id}>
              <button
                onClick={() => toggle(r.id)}
                className="flex w-full items-center gap-2 px-3 py-2 text-left text-sm hover:bg-muted/40"
              >
                <HugeiconsIcon
                  icon={ArrowDown01Icon}
                  size={14}
                  className={cn(
                    "shrink-0 text-muted-foreground transition-transform",
                    isOpen && "rotate-180",
                  )}
                />
                <Badge
                  variant={isHead ? "default" : "outline"}
                  className="font-mono"
                >
                  rev {r.seq}
                </Badge>
                <Badge variant="secondary" className="font-mono">
                  {r.event_kind}
                </Badge>
                <LintBadges lint={revisionLint(r)} />
                {bodyFormat !== "markdown" && (
                  <Badge variant="outline" className="font-mono uppercase">
                    {bodyFormat.toUpperCase()}
                  </Badge>
                )}
                {isHead && (
                  <span className="text-xs text-muted-foreground">current</span>
                )}
                <span className="flex min-w-0 items-center gap-1.5 self-start pt-0.5">
                  <UserAvatar user={editor} className="size-4" />
                  <span className="truncate">
                    {editor
                      ? userDisplayName(editor)
                      : r.edited_by_user_id
                        ? "unknown"
                        : "—"}
                  </span>
                </span>
                <span className="min-w-0 flex-1 truncate text-xs text-muted-foreground">
                  {revisionBodyPreview(r)}
                </span>
                <time
                  dateTime={r.created_at}
                  title={r.created_at}
                  className="ml-auto shrink-0 text-xs text-muted-foreground"
                >
                  {relativeTime(r.created_at)}
                </time>
              </button>
              {isOpen && (
                <div className="space-y-2 px-3 pb-3">
                  {titleChanged && (
                    <p className="text-xs text-muted-foreground">
                      Title:{" "}
                      <span className="line-through">{prev.title}</span>{" "}
                      → <span className="text-foreground">{r.title}</span>
                    </p>
                  )}
                  <DiffView before={prev?.body ?? ""} after={r.body ?? ""} />
                  <LintDetails lint={revisionLint(r)} />
                  {!prev && (
                    <p className="text-xs text-muted-foreground">
                      Initial spec.
                    </p>
                  )}
                </div>
              )}
            </li>
          );
        })}
      </ul>
    </div>
  );
}
