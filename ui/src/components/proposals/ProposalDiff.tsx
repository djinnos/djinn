import {
  forwardRef,
  useCallback,
  useId,
  useImperativeHandle,
  useMemo,
  useRef,
  useState,
} from "react";
import { DiffView } from "@/components/proposals/DiffView";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import type { ProposalRevision } from "@/api/types";
import { cn } from "@/lib/utils";

export interface ProposalDiffHandle {
  open: () => void;
  close: () => void;
  toggle: () => void;
  focus: () => void;
}

export interface ProposalDiffProps {
  revisions: ProposalRevision[];
  baseSeq: number | null | undefined;
  headSeq: number | null | undefined;
  open?: boolean;
  defaultOpen?: boolean;
  onOpenChange?: (open: boolean) => void;
  className?: string;
}

function acceptanceCriterionLine(
  item: ProposalRevision["acceptance_criteria"][number],
): string {
  if (typeof item === "string") return `- ${item}`;
  const checked = item.met ? "x" : " ";
  return `- [${checked}] ${item.criterion}`;
}

function revisionBodyFormat(revision: ProposalRevision): string {
  const bodyFormat = (revision as { body_format?: unknown }).body_format;
  return typeof bodyFormat === "string" && bodyFormat.trim()
    ? bodyFormat.trim().toLowerCase()
    : "markdown";
}

/**
 * Best-effort markdown snapshot for diff display, not a markdown renderer.
 * Keeping stable section headings makes title, body, and acceptance-criteria
 * drift show up as ordinary line-level changes in the shared DiffView.
 */
function proposalRevisionMarkdown(revision: ProposalRevision): string {
  const criteria = revision.acceptance_criteria.length
    ? revision.acceptance_criteria.map(acceptanceCriterionLine).join("\n")
    : "_No acceptance criteria._";
  const bodyFormat = revisionBodyFormat(revision);

  return [
    `# ${revision.title}`,
    "",
    `## Body (${bodyFormat})`,
    (revision.body ?? "").trimEnd() || "_No body._",
    "",
    "## Acceptance criteria",
    criteria,
  ].join("\n");
}

/**
 * Default-closed proposal revision drift diff. Parents can either control
 * `open`/`onOpenChange` or use the imperative ref to open it from a drift badge.
 */
export const ProposalDiff = forwardRef<ProposalDiffHandle, ProposalDiffProps>(
  function ProposalDiff(
    {
      revisions,
      baseSeq,
      headSeq,
      open,
      defaultOpen = false,
      onOpenChange,
      className,
    },
    ref,
  ) {
    const [uncontrolledOpen, setUncontrolledOpen] = useState(defaultOpen);
    const triggerRef = useRef<HTMLButtonElement>(null);
    const panelId = useId();
    const isOpen = open ?? uncontrolledOpen;

    const setIsOpen = useCallback((next: boolean) => {
      if (open === undefined) setUncontrolledOpen(next);
      onOpenChange?.(next);
    }, [onOpenChange, open]);

    useImperativeHandle(
      ref,
      () => ({
        open: () => setIsOpen(true),
        close: () => setIsOpen(false),
        toggle: () => setIsOpen(!isOpen),
        focus: () => triggerRef.current?.focus(),
      }),
      [isOpen, setIsOpen],
    );

    const { baseRevision, headRevision } = useMemo(() => {
      const bySeq = new Map(revisions.map((revision) => [revision.seq, revision]));
      return {
        baseRevision: baseSeq == null ? undefined : bySeq.get(baseSeq),
        headRevision: headSeq == null ? undefined : bySeq.get(headSeq),
      };
    }, [baseSeq, headSeq, revisions]);

    const missingMessage =
      baseSeq == null || headSeq == null
        ? "Select both a base and head revision to view proposal drift."
        : !baseRevision && !headRevision
          ? `Revisions ${baseSeq} and ${headSeq} are not available. The diff cannot be displayed.`
          : !baseRevision
            ? `Base revision ${baseSeq} is not available. The diff cannot be displayed.`
            : !headRevision
              ? `Head revision ${headSeq} is not available. The diff cannot be displayed.`
              : null;

    const before = baseRevision ? proposalRevisionMarkdown(baseRevision) : "";
    const after = headRevision ? proposalRevisionMarkdown(headRevision) : "";

    return (
      <section className={cn("rounded-lg border bg-card", className)}>
        <div className="flex items-center justify-between gap-3 px-3 py-2">
          <div className="min-w-0">
            <p className="text-sm font-medium">Proposal revision diff</p>
            <p className="text-xs text-muted-foreground">
              {baseSeq ?? "—"} → {headSeq ?? "—"}
            </p>
          </div>
          <div className="flex shrink-0 items-center gap-2">
            {baseSeq != null && headSeq != null && (
              <Badge variant="outline" className="font-mono">
                rev {baseSeq}…{headSeq}
              </Badge>
            )}
            <Button
              ref={triggerRef}
              type="button"
              variant="outline"
              size="sm"
              aria-expanded={isOpen}
              aria-controls={panelId}
              onClick={() => setIsOpen(!isOpen)}
            >
              {isOpen ? "Hide diff" : "Show diff"}
            </Button>
          </div>
        </div>
        {isOpen && (
          <div
            id={panelId}
            className="space-y-2 border-t px-3 py-3"
          >
            {missingMessage ? (
              <p className="text-sm text-muted-foreground">{missingMessage}</p>
            ) : (
              <DiffView before={before} after={after} />
            )}
          </div>
        )}
      </section>
    );
  },
);
