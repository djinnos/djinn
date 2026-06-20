import { useNavigate } from "react-router-dom";
import { HugeiconsIcon } from "@hugeicons/react";
import { ArrowRight01Icon, Comment01Icon } from "@hugeicons/core-free-icons";
import { Badge } from "@/components/ui/badge";
import type { ProposalChatScope } from "@/stores/chatStore";

/**
 * Header card shown at the top of a proposal-scoped chat ("Address with djinn").
 * Anchors the conversation to the proposal + the feedback being addressed and
 * links back to the proposal.
 */
export function ProposalChatContext({ scope }: { scope: ProposalChatScope }) {
  const navigate = useNavigate();
  return (
    <div className="mb-3 rounded-lg border bg-muted/40 p-3">
      <button
        type="button"
        onClick={() => navigate(`/proposals/${scope.proposalId}`)}
        className="group flex w-full items-center gap-2 text-left"
      >
        <Badge variant="outline" className="shrink-0 uppercase">
          Proposal
        </Badge>
        <span className="min-w-0 flex-1 truncate text-sm font-medium">
          {scope.proposalTitle}
        </span>
        <span className="shrink-0 font-mono text-xs text-muted-foreground">
          {scope.proposalShortId}
        </span>
        <HugeiconsIcon
          icon={ArrowRight01Icon}
          size={16}
          className="shrink-0 text-muted-foreground transition-transform group-hover:translate-x-0.5"
        />
      </button>
      {scope.targetSection && (
        <div className="mt-2 text-xs text-muted-foreground">
          Block target: <span className="font-mono text-foreground">{scope.targetSection}</span>
        </div>
      )}
      {scope.feedbackBody && (
        <div className="mt-2 rounded-md border bg-background/60 p-2">
          <div className="mb-1 flex items-center gap-1.5 text-xs text-muted-foreground">
            <HugeiconsIcon icon={Comment01Icon} size={13} />
            <span>Feedback from {scope.feedbackAuthor ?? "reviewer"}</span>
          </div>
          <p className="line-clamp-4 whitespace-pre-wrap text-xs text-foreground/80">
            {scope.feedbackBody}
          </p>
        </div>
      )}
    </div>
  );
}
