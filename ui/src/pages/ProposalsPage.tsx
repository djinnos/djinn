import { useMemo, useState } from "react";
import { useNavigate, useParams } from "react-router-dom";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import { ArrowLeft01Icon } from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";
import { callMcpTool } from "@/api/mcpClient";
import { AcceptanceChecklist } from "@/components/AcceptanceChecklist";
import { InlineError } from "@/components/InlineError";
import { relativeTime } from "@/components/memory/memoryUtils";
import {
  PROPOSAL_STATUS_KEYS,
  isArchivedLike,
  statusLabel,
  type ProposalStatus,
} from "@/components/proposals/proposalStatus";
import { StatusIcon } from "@/components/proposals/StatusIcon";
import { ProposalSignoffs } from "@/components/proposals/ProposalSignoffs";
import { ProposalKickoff } from "@/components/proposals/ProposalKickoff";
import { DiffView } from "@/components/proposals/DiffView";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Separator } from "@/components/ui/separator";
import { Skeleton } from "@/components/ui/skeleton";
import { Textarea } from "@/components/ui/textarea";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { showToast } from "@/lib/toast";
import { useAuthUser } from "@/components/AuthGate";
import { canEdit, capsFromUser } from "@/lib/proposalPermissions";
import { useProjects } from "@/stores/useProjectStore";
import {
  type ProposalDetail as ProposalDetailData,
  proposalDetailQueryOptions,
  proposalListQueryOptions,
} from "@/lib/proposalQueries";
import type { Project } from "@/api/server";
import type { Proposal, ProposalFeedback } from "@/api/types";

// Statuses a human sets directly. `approved` is reached via sign-offs;
// `building`/`done` via graduation.
const MANUAL_STATUSES: ProposalStatus[] = [
  "triage",
  "draft",
  "in_review",
  "rejected",
  "archived",
  "superseded",
];

const NEW_TEMPLATE = `## Problem
What's broken or missing, and who it hurts.

## Proposed change
What we want to do about it.

## Scope
Which projects/areas this touches.

## Open questions
Things still to decide.
`;

export function ProposalsPage() {
  const { id } = useParams<{ id?: string }>();
  if (id) return <ProposalDetailRoute id={id} />;
  return <ProposalsListView />;
}

// ── List ─────────────────────────────────────────────────────────────────────

function ProposalsListView() {
  const navigate = useNavigate();
  const [search, setSearch] = useState("");
  const [showArchived, setShowArchived] = useState(false);
  const [creating, setCreating] = useState(false);
  const projects = useProjects();
  const queryClient = useQueryClient();

  const listQuery = useQuery(
    proposalListQueryOptions({ text: search.trim() || undefined })
  );

  const groups = useMemo(() => {
    const visible = (listQuery.data ?? []).filter(
      (p) => showArchived || !isArchivedLike(p.status)
    );
    return PROPOSAL_STATUS_KEYS.map((status) => ({
      status,
      items: visible
        .filter((p) => p.status === status)
        .sort((a, b) => (a.updated_at < b.updated_at ? 1 : -1)),
    })).filter((g) => g.items.length > 0);
  }, [listQuery.data, showArchived]);

  if (creating) {
    return (
      <CreateProposal
        projects={projects}
        onCancel={() => setCreating(false)}
        onCreated={(newId) => {
          setCreating(false);
          queryClient.invalidateQueries({ queryKey: ["proposals"] });
          navigate(`/proposals/${newId}`);
        }}
      />
    );
  }

  return (
    <div className="flex h-full min-h-0 flex-col">
      <div className="flex items-center justify-between gap-4 border-b p-4">
        <h1 className="text-lg font-semibold">Proposals</h1>
        <div className="flex items-center gap-3">
          <Input
            placeholder="Search proposals…"
            value={search}
            onChange={(e) => setSearch(e.target.value)}
            className="w-64"
          />
          <label className="flex items-center gap-1.5 whitespace-nowrap text-sm text-muted-foreground">
            <input
              type="checkbox"
              checked={showArchived}
              onChange={(e) => setShowArchived(e.target.checked)}
            />
            Show archived
          </label>
          <Button size="sm" onClick={() => setCreating(true)}>
            New
          </Button>
        </div>
      </div>

      <div className="min-h-0 flex-1 overflow-y-auto">
        {listQuery.isLoading ? (
          <div className="space-y-2 p-4">
            {[0, 1, 2].map((i) => (
              <Skeleton key={i} className="h-10 w-full" />
            ))}
          </div>
        ) : listQuery.isError ? (
          <div className="p-4">
            <InlineError message={(listQuery.error as Error).message} />
          </div>
        ) : groups.length === 0 ? (
          <p className="p-10 text-center text-sm text-muted-foreground">
            No proposals yet. Create one to get a scope out.
          </p>
        ) : (
          <div className="pb-10">
            {groups.map((g) => (
              <section key={g.status}>
                <div className="flex items-center gap-2 bg-muted/40 px-4 py-1.5 text-xs font-medium text-muted-foreground">
                  <StatusIcon status={g.status} />
                  <span>{statusLabel(g.status)}</span>
                  <span className="text-muted-foreground/60">{g.items.length}</span>
                </div>
                <ul>
                  {g.items.map((p) => (
                    <li key={p.id}>
                      <button
                        onClick={() => navigate(`/proposals/${p.id}`)}
                        className="flex w-full items-center gap-3 border-b border-border/40 px-4 py-2.5 text-left hover:bg-muted/40"
                      >
                        <StatusIcon status={p.status} />
                        <span className="min-w-0 flex-1 truncate text-sm">{p.title}</span>
                        <span className="hidden shrink-0 font-mono text-xs text-muted-foreground sm:inline">
                          {p.short_id}
                        </span>
                        <span className="shrink-0 text-xs text-muted-foreground">
                          {relativeTime(p.updated_at)}
                        </span>
                      </button>
                    </li>
                  ))}
                </ul>
              </section>
            ))}
          </div>
        )}
      </div>
    </div>
  );
}

// ── Create ───────────────────────────────────────────────────────────────────

function CreateProposal({
  projects,
  onCancel,
  onCreated,
}: {
  projects: Project[];
  onCancel: () => void;
  onCreated: (id: string) => void;
}) {
  const me = useAuthUser();
  const caps = capsFromUser(me);
  // External proposers always land in triage; PM/engineer/admin pick their
  // entry state (and can jump straight to engineering review for a technical
  // change, skipping product scoping).
  const isProposer = !!caps && !caps.isAdmin && caps.role === "proposer";
  const [title, setTitle] = useState("");
  const [body, setBody] = useState(NEW_TEMPLATE);
  const [targets, setTargets] = useState<Set<string>>(new Set());
  const [status, setStatus] = useState<"draft" | "in_review">("draft");
  const [saving, setSaving] = useState(false);

  const toggle = (id: string) =>
    setTargets((prev) => {
      const next = new Set(prev);
      if (next.has(id)) next.delete(id);
      else next.add(id);
      return next;
    });

  const create = async () => {
    if (!title.trim()) {
      showToast.error("Title is required");
      return;
    }
    setSaving(true);
    try {
      const res = await callMcpTool("proposal_create", {
        title: title.trim(),
        body,
        target_projects: Array.from(targets),
        // Proposers are forced into triage server-side; omit status for them.
        status: isProposer ? undefined : status,
      });
      if (res.error || !res.id) throw new Error(res.error ?? "create failed");
      showToast.success("Proposal created");
      onCreated(res.id as string);
    } catch (e) {
      showToast.error("Failed to create proposal", { description: (e as Error).message });
    } finally {
      setSaving(false);
    }
  };

  return (
    <div className="h-full overflow-y-auto">
      <div className="mx-auto max-w-2xl space-y-5 p-6">
        <h2 className="text-lg font-semibold">New proposal</h2>
        <div className="space-y-2">
          <Label htmlFor="np-title">Title</Label>
          <Input
            id="np-title"
            value={title}
            onChange={(e) => setTitle(e.target.value)}
            placeholder="Short, action-oriented title"
          />
        </div>
        <div className="space-y-2">
          <Label htmlFor="np-body">Spec</Label>
          <Textarea
            id="np-body"
            value={body}
            onChange={(e) => setBody(e.target.value)}
            className="min-h-[260px] font-mono text-sm"
          />
        </div>
        <div className="space-y-2">
          <Label>Target projects</Label>
          <div className="flex flex-wrap gap-2">
            {projects.map((p) => (
              <button
                key={p.id}
                type="button"
                onClick={() => toggle(p.id)}
                className={`rounded-full border px-3 py-1 text-xs ${
                  targets.has(p.id)
                    ? "border-primary bg-primary/10 text-primary"
                    : "text-muted-foreground"
                }`}
              >
                {p.name}
              </button>
            ))}
          </div>
        </div>
        {isProposer ? (
          <p className="flex items-center gap-2 text-xs text-muted-foreground">
            <StatusIcon status="triage" />
            This proposal goes to triage for a PM or engineer to pick up.
          </p>
        ) : (
          <div className="space-y-2">
            <Label>Start as</Label>
            <Select value={status} onValueChange={(v) => (v === "draft" || v === "in_review") && setStatus(v)}>
              <SelectTrigger className="w-[200px]">
                <span className="flex items-center gap-2">
                  <StatusIcon status={status} />
                  {statusLabel(status)}
                </span>
              </SelectTrigger>
              <SelectContent>
                <SelectItem value="draft">
                  <span className="flex items-center gap-2">
                    <StatusIcon status="draft" /> Draft
                  </span>
                </SelectItem>
                <SelectItem value="in_review">
                  <span className="flex items-center gap-2">
                    <StatusIcon status="in_review" /> In Review (skip scoping)
                  </span>
                </SelectItem>
              </SelectContent>
            </Select>
          </div>
        )}
        <div className="flex gap-2">
          <Button onClick={create} disabled={saving}>
            {saving ? "Creating…" : "Create proposal"}
          </Button>
          <Button variant="ghost" onClick={onCancel}>
            Cancel
          </Button>
        </div>
      </div>
    </div>
  );
}

// ── Detail route ─────────────────────────────────────────────────────────────

function ProposalDetailRoute({ id }: { id: string }) {
  const navigate = useNavigate();
  const queryClient = useQueryClient();
  const projects = useProjects();
  const detailQuery = useQuery(proposalDetailQueryOptions(id));

  const invalidate = () => queryClient.invalidateQueries({ queryKey: ["proposals"] });

  return (
    <div className="flex h-full min-h-0 flex-col">
      <div className="flex items-center gap-2 border-b px-4 py-2">
        <Button variant="ghost" size="sm" onClick={() => navigate("/proposals")} className="gap-1.5">
          <HugeiconsIcon icon={ArrowLeft01Icon} size={16} />
          Proposals
        </Button>
      </div>
      {detailQuery.isLoading ? (
        <div className="space-y-4 p-6">
          <Skeleton className="h-8 w-2/3" />
          <Skeleton className="h-40 w-full" />
        </div>
      ) : detailQuery.isError ? (
        <div className="p-6">
          <InlineError message={(detailQuery.error as Error).message} />
        </div>
      ) : detailQuery.data?.proposal ? (
        <ProposalDetailView
          detail={detailQuery.data}
          projects={projects}
          onChanged={invalidate}
          onDeleted={() => {
            invalidate();
            navigate("/proposals");
          }}
        />
      ) : (
        <p className="p-10 text-center text-sm text-muted-foreground">Proposal not found.</p>
      )}
    </div>
  );
}

function ProposalDetailView({
  detail,
  projects,
  onChanged,
  onDeleted,
}: {
  detail: ProposalDetailData;
  projects: Project[];
  onChanged: () => void;
  onDeleted: () => void;
}) {
  const proposal = detail.proposal as Proposal;
  const me = useAuthUser();
  const caps = capsFromUser(me);
  const isAuthor = !!me && proposal.author_user_id === me.id;
  const canDirectEdit = canEdit(caps, isAuthor);
  const untargeted = useMemo(
    () => projects.filter((p) => !detail.targets.some((t) => t.project_id === p.id)),
    [projects, detail.targets]
  );

  const run = async (fn: () => Promise<{ error?: string }>, ok: string) => {
    try {
      const res = await fn();
      if (res.error) throw new Error(res.error);
      showToast.success(ok);
      onChanged();
    } catch (e) {
      showToast.error("Action failed", { description: (e as Error).message });
    }
  };

  return (
    <div className="min-h-0 flex-1 overflow-y-auto">
      <div className="mx-auto max-w-3xl space-y-6 p-6">
        {/* Header */}
        <div className="flex items-start justify-between gap-4">
          <div className="min-w-0">
            <h2 className="text-xl font-semibold">{proposal.title}</h2>
            <div className="mt-1 flex items-center gap-2 text-xs text-muted-foreground">
              <span className="font-mono">{proposal.short_id}</span>
              <span>·</span>
              <span>updated {relativeTime(proposal.updated_at)}</span>
            </div>
          </div>
          <div className="flex items-center gap-2">
            <Select
              value={proposal.status}
              onValueChange={(status) =>
                typeof status === "string" &&
                run(
                  () => callMcpTool("proposal_update", { id: proposal.id, status }),
                  `Status → ${statusLabel(status)}`
                )
              }
            >
              <SelectTrigger className="w-[150px]">
                <span className="flex items-center gap-2">
                  <StatusIcon status={proposal.status} />
                  {statusLabel(proposal.status)}
                </span>
              </SelectTrigger>
              <SelectContent>
                {MANUAL_STATUSES.map((s) => (
                  <SelectItem key={s} value={s}>
                    <span className="flex items-center gap-2">
                      <StatusIcon status={s} />
                      {statusLabel(s)}
                    </span>
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>
            {(isAuthor || caps?.isAdmin) && (
              <Button
                variant="ghost"
                size="sm"
                onClick={() => {
                  if (confirm("Delete this proposal? This cannot be undone.")) {
                    run(
                      () => callMcpTool("proposal_delete", { id: proposal.id }),
                      "Proposal deleted"
                    ).then(onDeleted);
                  }
                }}
              >
                Delete
              </Button>
            )}
          </div>
        </div>

        {/* Targets */}
        <div className="space-y-2">
          <Label className="text-xs uppercase text-muted-foreground">Target projects</Label>
          <div className="flex flex-wrap items-center gap-2">
            {detail.targets.length === 0 && (
              <span className="text-sm text-muted-foreground">None yet</span>
            )}
            {detail.targets.map((t) => (
              <span
                key={t.project_id}
                className="flex items-center gap-1 rounded-full border px-3 py-1 text-xs"
              >
                {t.project_name ?? t.project_path ?? t.project_id}
                {t.role === "reference" && <span className="text-muted-foreground">(ref)</span>}
                <button
                  className="ml-1 text-muted-foreground hover:text-destructive"
                  onClick={() =>
                    run(
                      () =>
                        callMcpTool("proposal_remove_target", {
                          id: proposal.id,
                          project: t.project_id,
                        }),
                      "Target removed"
                    )
                  }
                >
                  ×
                </button>
              </span>
            ))}
            {untargeted.length > 0 && (
              <Select
                value=""
                onValueChange={(pid) =>
                  typeof pid === "string" &&
                  run(
                    () => callMcpTool("proposal_add_target", { id: proposal.id, project: pid }),
                    "Target added"
                  )
                }
              >
                <SelectTrigger className="h-7 w-[150px] text-xs">
                  <SelectValue placeholder="+ Add target" />
                </SelectTrigger>
                <SelectContent>
                  {untargeted.map((p) => (
                    <SelectItem key={p.id} value={p.id}>
                      {p.name}
                    </SelectItem>
                  ))}
                </SelectContent>
              </Select>
            )}
          </div>
        </div>

        {/* Acceptance criteria — read-only checklist (no clickable/met state). */}
        {(proposal.acceptance_criteria?.length ?? 0) > 0 && (
          <div className="space-y-2">
            <Label className="text-xs uppercase text-muted-foreground">Acceptance criteria</Label>
            <AcceptanceChecklist criteria={proposal.acceptance_criteria} />
          </div>
        )}

        {/* Spec body — read-only; editing happens via AI. */}
        <div className="space-y-2">
          <Label className="text-xs uppercase text-muted-foreground">Spec</Label>
          <div className="prose prose-sm max-w-none dark:prose-invert">
            <ReactMarkdown remarkPlugins={[remarkGfm]}>
              {proposal.body || "_No spec body yet._"}
            </ReactMarkdown>
          </div>
        </div>

        <Separator />

        <ProposalSignoffs detail={detail} onChanged={onChanged} />

        <ProposalKickoff detail={detail} onChanged={onChanged} />

        <Separator />

        <FeedbackThread
          proposalId={proposal.id}
          feedback={detail.feedback}
          currentBody={proposal.body}
          canAccept={canDirectEdit}
          onChanged={onChanged}
        />
      </div>
    </div>
  );
}

// ── Feedback ─────────────────────────────────────────────────────────────────

function FeedbackThread({
  proposalId,
  feedback,
  currentBody,
  canAccept,
  onChanged,
}: {
  proposalId: string;
  feedback: ProposalFeedback[];
  currentBody: string;
  canAccept: boolean;
  onChanged: () => void;
}) {
  const [body, setBody] = useState("");
  const [asSuggestion, setAsSuggestion] = useState(false);
  const [posting, setPosting] = useState(false);

  const accept = async (id: string) => {
    try {
      const res = await callMcpTool("proposal_feedback_accept", { id });
      if (res.error) throw new Error(res.error);
      onChanged();
    } catch (e) {
      showToast.error("Failed to accept", { description: (e as Error).message });
    }
  };

  const post = async () => {
    if (!body.trim()) return;
    setPosting(true);
    try {
      const res = await callMcpTool("proposal_feedback_add", {
        proposal_id: proposalId,
        body: body.trim(),
        status: asSuggestion ? "open" : undefined,
      });
      if (res.error) throw new Error(res.error);
      setBody("");
      onChanged();
    } catch (e) {
      showToast.error("Failed to post", { description: (e as Error).message });
    } finally {
      setPosting(false);
    }
  };

  const resolve = async (id: string, status: string) => {
    try {
      const res = await callMcpTool("proposal_feedback_resolve", { id, status });
      if (res.error) throw new Error(res.error);
      onChanged();
    } catch (e) {
      showToast.error("Failed to resolve", { description: (e as Error).message });
    }
  };

  const openCount = feedback.filter((f) => f.status === "open").length;

  return (
    <div className="space-y-4">
      <div className="flex items-center justify-between">
        <Label className="text-xs uppercase text-muted-foreground">Discussion &amp; suggestions</Label>
        {openCount > 0 && <Badge variant="secondary">{openCount} open</Badge>}
      </div>

      <div className="space-y-3">
        {feedback.length === 0 && (
          <p className="text-sm text-muted-foreground">
            No feedback yet. Share the spec and enhance it together.
          </p>
        )}
        {feedback.map((f) => (
          <div key={f.id} className="rounded-md border p-3">
            <div className="mb-1 flex items-center gap-2 text-xs text-muted-foreground">
              <Badge variant={f.author_kind === "ai" ? "secondary" : "outline"}>
                {f.author_kind === "ai" ? (f.author_model ?? "ai") : "user"}
              </Badge>
              {f.status && (
                <Badge
                  variant={f.status === "accepted" ? "default" : "outline"}
                  className="capitalize"
                >
                  {f.status}
                </Badge>
              )}
              {f.target_section && <span>· {f.target_section}</span>}
              <span>· {relativeTime(f.created_at)}</span>
            </div>
            <div className="whitespace-pre-wrap text-sm">{f.body}</div>
            {f.proposed_body != null && (
              <div className="mt-2">
                <span className="text-xs text-muted-foreground">Proposed spec change:</span>
                <DiffView before={currentBody} after={f.proposed_body} />
              </div>
            )}
            {f.status === "open" && canAccept && (
              <div className="mt-2 flex gap-2">
                <Button size="sm" variant="outline" onClick={() => accept(f.id)}>
                  {f.proposed_body != null ? "Accept & apply" : "Accept"}
                </Button>
                <Button size="sm" variant="ghost" onClick={() => resolve(f.id, "rejected")}>
                  Reject
                </Button>
              </div>
            )}
          </div>
        ))}
      </div>

      <div className="space-y-2">
        <Textarea
          value={body}
          onChange={(e) => setBody(e.target.value)}
          placeholder={asSuggestion ? "Propose a change…" : "Add to the discussion…"}
          className="min-h-[80px]"
        />
        <div className="flex items-center gap-3">
          <Button onClick={post} disabled={posting || !body.trim()}>
            {asSuggestion ? "Suggest" : "Comment"}
          </Button>
          <label className="flex items-center gap-1.5 text-sm text-muted-foreground">
            <input
              type="checkbox"
              checked={asSuggestion}
              onChange={(e) => setAsSuggestion(e.target.checked)}
            />
            As a trackable suggestion
          </label>
        </div>
      </div>
    </div>
  );
}
