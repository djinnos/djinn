import { useEffect, useRef, useState } from "react";
import { useMutation, useQueryClient } from "@tanstack/react-query";
import { useNavigate } from "react-router-dom";
import {
  ArrowRight01Icon,
  CheckmarkCircle04Icon,
  FileEditIcon,
  Legal01Icon,
  Task01Icon,
} from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";

import type { Project } from "@/api/server";
import {
  createStarterProposal,
  type CreatedStarterProposal,
} from "@/api/proposals";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Textarea } from "@/components/ui/textarea";
import { InlineError } from "@/components/InlineError";

import { OnboardingShell } from "./OnboardingShell";

export function FirstProposalOnboarding({
  project,
  onFinished,
}: {
  project: Project;
  onFinished: () => void;
}) {
  const navigate = useNavigate();
  const queryClient = useQueryClient();
  const headingRef = useRef<HTMLHeadingElement>(null);
  const [title, setTitle] = useState("");
  const [outcome, setOutcome] = useState("");
  const [created, setCreated] = useState<CreatedStarterProposal | null>(null);

  useEffect(() => {
    headingRef.current?.focus();
  }, [created]);

  const mutation = useMutation({
    mutationFn: () => createStarterProposal({ project, title, outcome }),
    onSuccess: (proposal) => {
      setCreated(proposal);
      void queryClient.invalidateQueries({ queryKey: ["proposals"] });
    },
  });

  if (created) {
    return (
      <OnboardingShell current="proposal" complete>
        <div className="flex flex-col items-center gap-5 text-center">
          <div className="flex h-12 w-12 items-center justify-center rounded-full bg-primary/15">
            <HugeiconsIcon
              icon={CheckmarkCircle04Icon}
              size={26}
              className="text-primary"
            />
          </div>
          <div>
            <h1
              ref={headingRef}
              tabIndex={-1}
              className="text-xl font-semibold outline-none"
            >
              Your repository is agent-ready
            </h1>
            <p className="mt-2 text-sm leading-relaxed text-muted-foreground">
              {created.shortId ? `${created.shortId} · ` : ""}
              {created.title} is now a draft. Nothing runs yet: refine it,
              review the plan, and graduate it only when the work is ready for
              agents.
            </p>
          </div>
          <Button
            className="px-8"
            onClick={() => {
              onFinished();
              navigate(`/proposals/${created.id}`);
            }}
          >
            Open your proposal
            <HugeiconsIcon icon={ArrowRight01Icon} size={15} />
          </Button>
          <p className="text-xs text-muted-foreground">
            Find it later under Proposals in the sidebar.
          </p>
        </div>
      </OnboardingShell>
    );
  }

  const canCreate = title.trim().length >= 5 && outcome.trim().length >= 20;

  return (
    <OnboardingShell current="proposal">
      <div className="flex flex-col gap-6">
        <div className="text-center">
          <h1
            ref={headingRef}
            tabIndex={-1}
            className="text-xl font-semibold outline-none"
          >
            Create your first proposal
          </h1>
          <p className="mt-2 text-sm leading-relaxed text-muted-foreground">
            A proposal turns an idea into reviewed, bounded work before agents
            touch the repository. Start with one real outcome for {project.name}.
          </p>
        </div>

        <ol
          aria-label="How proposals become agent work"
          className="grid gap-3 sm:grid-cols-3"
        >
          <ProposalStage
            icon={FileEditIcon}
            number="1"
            title="Shape"
            description="Capture the outcome, scope, and proof of success."
          />
          <ProposalStage
            icon={Legal01Icon}
            number="2"
            title="Refine"
            description="Planning and review agents challenge assumptions."
          />
          <ProposalStage
            icon={Task01Icon}
            number="3"
            title="Graduate"
            description="An approved proposal becomes executable epics and tasks."
          />
        </ol>

        <div className="rounded-xl border bg-card/35 p-5">
          <div className="mb-5 flex items-center justify-between gap-3 rounded-lg border border-primary/20 bg-primary/[0.06] px-4 py-3">
            <div className="min-w-0">
              <p className="text-[11px] font-medium uppercase tracking-wide text-muted-foreground">
                Target repository
              </p>
              <p className="truncate text-sm font-semibold">
                {project.github_owner}/{project.github_repo}
              </p>
            </div>
            <span className="shrink-0 rounded-full bg-primary/15 px-2.5 py-1 text-[11px] font-medium text-primary">
              Draft only
            </span>
          </div>

          <div className="space-y-4">
            <div className="space-y-1.5">
              <Label htmlFor="first-proposal-title">What should change?</Label>
              <Input
                id="first-proposal-title"
                autoComplete="off"
                value={title}
                onChange={(event) => setTitle(event.currentTarget.value)}
                placeholder="e.g. Add reliable draft autosave"
                disabled={mutation.isPending}
              />
            </div>
            <div className="space-y-1.5">
              <Label htmlFor="first-proposal-outcome">Desired outcome</Label>
              <Textarea
                id="first-proposal-outcome"
                value={outcome}
                onChange={(event) => setOutcome(event.currentTarget.value)}
                placeholder="Describe what becomes better for a user or developer, without prescribing the implementation."
                className="min-h-28 resize-y"
                disabled={mutation.isPending}
              />
              <p className="text-xs text-muted-foreground">
                Djinn adds a safe starter structure, validation expectations,
                and this repository as the proposal target.
              </p>
            </div>
          </div>
        </div>

        {mutation.isError && (
          <InlineError
            message={
              mutation.error instanceof Error
                ? mutation.error.message
                : "Could not create the first proposal"
            }
            onRetry={() => mutation.mutate()}
          />
        )}

        <Button
          className="w-full"
          disabled={!canCreate || mutation.isPending}
          onClick={() => mutation.mutate()}
        >
          {mutation.isPending ? "Creating proposal…" : "Create draft proposal"}
        </Button>

        <p className="text-center text-xs text-muted-foreground">
          Creating a draft does not start agents or change code. You stay in
          control of refinement, approval, and graduation.
        </p>
      </div>
    </OnboardingShell>
  );
}

function ProposalStage({
  icon,
  number,
  title,
  description,
}: {
  icon: Parameters<typeof HugeiconsIcon>[0]["icon"];
  number: string;
  title: string;
  description: string;
}) {
  return (
    <li className="rounded-xl border bg-card/30 p-4">
      <div className="mb-3 flex items-center justify-between">
        <div className="flex h-8 w-8 items-center justify-center rounded-lg bg-primary/12 text-primary">
          <HugeiconsIcon icon={icon} size={17} />
        </div>
        <span className="text-xs font-semibold text-muted-foreground">
          {number}
        </span>
      </div>
      <p className="text-sm font-semibold">{title}</p>
      <p className="mt-1 text-xs leading-relaxed text-muted-foreground">
        {description}
      </p>
    </li>
  );
}
