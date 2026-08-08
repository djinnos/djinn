import { useQuery } from "@tanstack/react-query";
import { usersQueryOptions } from "@/api/queryOptions";
import { userDisplayName, type OrgUser } from "@/api/users";
import { UserAvatar } from "@/components/UserAvatar";
import { Badge } from "@/components/ui/badge";
import { relativeTime } from "@/components/memory/memoryUtils";
import type {
  ProposalFeedbackRefinement,
  ProposalFeedbackSourceRow,
} from "@/api/types";

const stateCopy: Record<string, string> = {
  queued: "Queued for refinement",
  injected: "Under review",
  accepted: "Fixed by revision",
  wont_fix: "Won't fix",
  withdrawn_by_author: "Withdrawn by author",
};

function feedbackAuthor(
  row: ProposalFeedbackSourceRow,
  userFor: (id?: string | null) => OrgUser | undefined,
) {
  if (row.author_kind === "ai") return row.author_model ?? "AI reviewer";
  const user = userFor(row.author_user_id);
  return user ? userDisplayName(user) : "reviewer";
}

/** Canonical server-projected feedback refinement generations. */
export function FeedbackRefinementCards({
  refinements,
}: {
  refinements: ProposalFeedbackRefinement[];
}) {
  const usersQuery = useQuery(usersQueryOptions());
  const userFor = (id?: string | null) =>
    id ? (usersQuery.data ?? []).find((user: OrgUser) => user.id === id) : undefined;

  if (refinements.length === 0) return null;

  return (
    <div className="space-y-3" aria-label="Feedback refinement lifecycle">
      {refinements.map((generation) => {
        const sourceRows = generation.source_rows ?? [];
        const isBlocking = sourceRows.some((row) => row.severity === "blocking");
        const state = stateCopy[generation.state] ?? generation.state;
        return (
          <section
            key={`${generation.root_feedback_id}-${generation.generation}`}
            className="rounded-md border bg-muted/30 p-3"
            data-testid="feedback-refinement-generation"
          >
            <div className="flex flex-wrap items-center gap-2">
              <span className="text-sm font-medium">Feedback refinement</span>
              <Badge variant="outline">generation {generation.generation}</Badge>
              {isBlocking ? (
                <Badge variant="destructive">blocking generation</Badge>
              ) : (
                <Badge variant="secondary">advisory</Badge>
              )}
              <Badge variant={generation.state === "wont_fix" ? "outline" : "secondary"}>
                {state}
              </Badge>
              {generation.debate_entry_id && (
                <a
                  href={`#proposal-debate-entry-${generation.debate_entry_id}`}
                  className="ml-auto text-xs text-muted-foreground underline-offset-2 hover:text-foreground hover:underline"
                >
                  View source debate entry
                </a>
              )}
              {generation.accepted_revision_seq != null && (
                <a
                  href={`#proposal-revision-${generation.accepted_revision_seq}`}
                  className="text-xs text-muted-foreground underline-offset-2 hover:text-foreground hover:underline"
                >
                  View accepted revision {generation.accepted_revision_seq}
                </a>
              )}
            </div>
            {generation.accepted_reason && (
              <p className="mt-2 text-sm">
                <span className="font-medium">Reason: </span>
                {generation.accepted_reason}
              </p>
            )}
            {generation.state === "withdrawn_by_author" && (
              <p className="mt-2 text-sm text-muted-foreground">
                Withdrawn by the original author
                {generation.withdrawn_at ? ` ${relativeTime(generation.withdrawn_at)}` : ""}.
              </p>
            )}
            <ul className="mt-3 space-y-2">
              {sourceRows.map((row) => {
                const user = userFor(row.author_user_id);
                return (
                  <li
                    key={`${row.source_feedback_id}-${row.source_ordinal}`}
                    className="rounded border bg-background/60 p-2"
                  >
                    <div className="flex flex-wrap items-center gap-2 text-xs text-muted-foreground">
                      <Badge variant={row.severity === "blocking" ? "destructive" : "secondary"}>
                        {row.severity}
                      </Badge>
                      {row.author_kind === "user" && <UserAvatar user={user} className="size-4" />}
                      <span className="font-medium text-foreground">
                        {feedbackAuthor(row, userFor)}
                      </span>
                      <span>· {relativeTime(row.created_at)}</span>
                    </div>
                    <p className="mt-1 text-sm">{row.body}</p>
                  </li>
                );
              })}
            </ul>
          </section>
        );
      })}
    </div>
  );
}
