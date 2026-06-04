import { useMemo, useState } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import { callMcpTool } from "@/api/mcpClient";
import { InlineError } from "@/components/InlineError";
import { relativeTime } from "@/components/memory/memoryUtils";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { ScrollArea } from "@/components/ui/scroll-area";
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
import { Tabs, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { showToast } from "@/lib/toast";
import { useProjects } from "@/stores/useProjectStore";
import {
  type ProposalDetail as ProposalDetailData,
  proposalDetailQueryOptions,
  proposalListQueryOptions,
} from "@/lib/proposalQueries";
import type { Project } from "@/api/server";
import type { Proposal, ProposalFeedback } from "@/api/types";

const STATUSES = ["draft", "shared", "ready", "archived", "superseded"] as const;
const FILTER_TABS = ["all", "draft", "shared", "ready", "archived"] as const;

const NEW_TEMPLATE = `## Problem
What's broken or missing, and who it hurts.

## Proposed change
What we want to do about it.

## Scope
Which projects/areas this touches.

## Open questions
Things still to decide.
`;

function statusVariant(status: string): "default" | "secondary" | "outline" {
  switch (status) {
    case "ready":
      return "default";
    case "shared":
      return "secondary";
    default:
      return "outline";
  }
}

export function ProposalsPage() {
  const queryClient = useQueryClient();
  const projects = useProjects();

  const [filter, setFilter] = useState<(typeof FILTER_TABS)[number]>("all");
  const [search, setSearch] = useState("");
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [creating, setCreating] = useState(false);

  const listQuery = useQuery(
    proposalListQueryOptions({
      status: filter === "all" ? undefined : filter,
      text: search.trim() || undefined,
    })
  );

  const detailQuery = useQuery(proposalDetailQueryOptions(selectedId));

  const invalidate = () =>
    queryClient.invalidateQueries({ queryKey: ["proposals"] });

  const proposals = listQuery.data ?? [];

  return (
    <div className="flex h-full min-h-0">
      {/* ── List ───────────────────────────────────────────────────────── */}
      <div className="flex w-[380px] shrink-0 flex-col border-r">
        <div className="space-y-3 border-b p-4">
          <div className="flex items-center justify-between">
            <h1 className="text-lg font-semibold">Proposals</h1>
            <Button size="sm" onClick={() => { setCreating(true); setSelectedId(null); }}>
              New
            </Button>
          </div>
          <Input
            placeholder="Search proposals…"
            value={search}
            onChange={(e) => setSearch(e.target.value)}
          />
          <Tabs value={filter} onValueChange={(v) => setFilter(v as typeof filter)}>
            <TabsList className="w-full">
              {FILTER_TABS.map((t) => (
                <TabsTrigger key={t} value={t} className="flex-1 text-xs capitalize">
                  {t}
                </TabsTrigger>
              ))}
            </TabsList>
          </Tabs>
        </div>

        <ScrollArea className="flex-1">
          {listQuery.isLoading ? (
            <div className="space-y-2 p-4">
              {[0, 1, 2].map((i) => (
                <Skeleton key={i} className="h-16 w-full" />
              ))}
            </div>
          ) : listQuery.isError ? (
            <div className="p-4">
              <InlineError message={(listQuery.error as Error).message} />
            </div>
          ) : proposals.length === 0 ? (
            <p className="p-6 text-center text-sm text-muted-foreground">
              No proposals yet. Create one to get a scope out.
            </p>
          ) : (
            <ul className="divide-y">
              {proposals.map((p) => (
                <li key={p.id}>
                  <button
                    onClick={() => { setSelectedId(p.id); setCreating(false); }}
                    className={`w-full px-4 py-3 text-left hover:bg-muted/50 ${
                      selectedId === p.id ? "bg-muted" : ""
                    }`}
                  >
                    <div className="flex items-center justify-between gap-2">
                      <span className="truncate text-sm font-medium">{p.title}</span>
                      <Badge variant={statusVariant(p.status)} className="shrink-0 capitalize">
                        {p.status}
                      </Badge>
                    </div>
                    <div className="mt-1 flex items-center gap-2 text-xs text-muted-foreground">
                      <span className="font-mono">{p.short_id}</span>
                      <span>·</span>
                      <span>{relativeTime(p.updated_at)}</span>
                    </div>
                  </button>
                </li>
              ))}
            </ul>
          )}
        </ScrollArea>
      </div>

      {/* ── Detail / Create ────────────────────────────────────────────── */}
      <div className="min-w-0 flex-1">
        {creating ? (
          <CreateProposal
            projects={projects}
            onCancel={() => setCreating(false)}
            onCreated={(id) => {
              setCreating(false);
              setSelectedId(id);
              invalidate();
            }}
          />
        ) : !selectedId ? (
          <div className="flex h-full items-center justify-center text-sm text-muted-foreground">
            Select a proposal or create a new one.
          </div>
        ) : detailQuery.isLoading ? (
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
              setSelectedId(null);
              invalidate();
            }}
          />
        ) : null}
      </div>
    </div>
  );
}

// ── Create form ──────────────────────────────────────────────────────────────

function CreateProposal({
  projects,
  onCancel,
  onCreated,
}: {
  projects: Project[];
  onCancel: () => void;
  onCreated: (id: string) => void;
}) {
  const [title, setTitle] = useState("");
  const [body, setBody] = useState(NEW_TEMPLATE);
  const [targets, setTargets] = useState<Set<string>>(new Set());
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
      });
      if (res.error || !res.id) throw new Error(res.error ?? "create failed");
      showToast.success("Proposal created");
      onCreated(res.id as string);
    } catch (e) {
      showToast.error("Failed to create proposal", {
        description: (e as Error).message,
      });
    } finally {
      setSaving(false);
    }
  };

  return (
    <ScrollArea className="h-full">
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
        <div className="flex gap-2">
          <Button onClick={create} disabled={saving}>
            {saving ? "Creating…" : "Create proposal"}
          </Button>
          <Button variant="ghost" onClick={onCancel}>
            Cancel
          </Button>
        </div>
      </div>
    </ScrollArea>
  );
}

// ── Detail ───────────────────────────────────────────────────────────────────

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
    <ScrollArea className="h-full">
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
                  `Status → ${status}`
                )
              }
            >
              <SelectTrigger className="w-[140px] capitalize">
                <SelectValue />
              </SelectTrigger>
              <SelectContent>
                {STATUSES.map((s) => (
                  <SelectItem key={s} value={s} className="capitalize">
                    {s}
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>
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
                {t.role === "reference" && (
                  <span className="text-muted-foreground">(ref)</span>
                )}
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
                    () =>
                      callMcpTool("proposal_add_target", {
                        id: proposal.id,
                        project: pid,
                      }),
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

        {/* Acceptance criteria */}
        {proposal.acceptance_criteria.length > 0 && (
          <div className="space-y-2">
            <Label className="text-xs uppercase text-muted-foreground">
              Acceptance criteria
            </Label>
            <ul className="list-inside list-disc space-y-1 text-sm">
              {proposal.acceptance_criteria.map((ac, i) => (
                <li key={i}>{ac}</li>
              ))}
            </ul>
          </div>
        )}

        {/* Body */}
        <div className="prose prose-sm dark:prose-invert max-w-none">
          <ReactMarkdown remarkPlugins={[remarkGfm]}>
            {proposal.body || "_No spec body yet._"}
          </ReactMarkdown>
        </div>

        <Separator />

        {/* Feedback */}
        <FeedbackThread
          proposalId={proposal.id}
          feedback={detail.feedback}
          onChanged={onChanged}
        />
      </div>
    </ScrollArea>
  );
}

// ── Feedback ─────────────────────────────────────────────────────────────────

function FeedbackThread({
  proposalId,
  feedback,
  onChanged,
}: {
  proposalId: string;
  feedback: ProposalFeedback[];
  onChanged: () => void;
}) {
  const [body, setBody] = useState("");
  const [asSuggestion, setAsSuggestion] = useState(false);
  const [posting, setPosting] = useState(false);

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
        <Label className="text-xs uppercase text-muted-foreground">
          Discussion &amp; suggestions
        </Label>
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
            {f.status === "open" && (
              <div className="mt-2 flex gap-2">
                <Button size="sm" variant="outline" onClick={() => resolve(f.id, "accepted")}>
                  Accept
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
