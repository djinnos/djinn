import { useEffect, useRef, useState } from "react";
import { useMutation, useQueryClient } from "@tanstack/react-query";
import { useNavigate } from "react-router-dom";
import {
  AiProgrammingIcon,
  ArrowRight01Icon,
  CheckmarkCircle04Icon,
  FileEditIcon,
  Legal01Icon,
  Task01Icon,
} from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";

import type { Project } from "@/api/server";
import {
  AGENTIC_READY_OUTCOME,
  AGENTIC_READY_TITLE,
  createStarterProposal,
  type CreatedStarterProposal,
  type StarterProposalInput,
  type StarterProposalKind,
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
  const [proposalKind, setProposalKind] =
    useState<StarterProposalKind>("agentic-ready");
  const [title, setTitle] = useState("");
  const [outcome, setOutcome] = useState("");
  const [created, setCreated] = useState<CreatedStarterProposal | null>(null);

  useEffect(() => {
    headingRef.current?.focus();
  }, [created]);

  const mutation = useMutation({
    mutationFn: (input: StarterProposalInput) =>
      createStarterProposal(input),
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
              Your first proposal is ready
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

  const canCreate =
    proposalKind === "agentic-ready" ||
    (title.trim().length >= 5 && outcome.trim().length >= 20);

  const createProposal = () => {
    mutation.mutate(
      proposalKind === "agentic-ready"
        ? {
            project,
            kind: "agentic-ready",
            title: AGENTIC_READY_TITLE,
            outcome: AGENTIC_READY_OUTCOME,
          }
        : {
            project,
            kind: "custom",
            title,
            outcome,
          },
    );
  };

  return (
    <OnboardingShell current="proposal">
      <div className="flex flex-col gap-6">
        <div className="text-center">
          <h1
            ref={headingRef}
            tabIndex={-1}
            className="text-xl font-semibold outline-none"
          >
            Choose your first proposal
          </h1>
          <p className="mt-2 text-sm leading-relaxed text-muted-foreground">
            Start by hardening {project.name} for reliable agent work, or bring
            a real product or engineering outcome of your own.
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

        <div
          role="group"
          aria-label="First proposal type"
          className="grid gap-3 sm:grid-cols-2"
        >
          <ProposalChoice
            icon={AiProgrammingIcon}
            title="Agentic-ready environment"
            badge="Recommended"
            description="Improve CI, toolchains, setup, services, and validation so agents can work from a clean checkout."
            selected={proposalKind === "agentic-ready"}
            disabled={mutation.isPending}
            onSelect={() => setProposalKind("agentic-ready")}
          />
          <ProposalChoice
            icon={FileEditIcon}
            title="Custom proposal"
            badge="Your outcome"
            description="Describe any product or engineering change in your own words, with a safe reviewed draft."
            selected={proposalKind === "custom"}
            disabled={mutation.isPending}
            onSelect={() => setProposalKind("custom")}
          />
        </div>

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

          {proposalKind === "agentic-ready" ? (
            <div>
              <div className="mb-3">
                <p className="text-sm font-semibold">
                  What this proposal will harden
                </p>
                <p className="mt-1 text-xs leading-relaxed text-muted-foreground">
                  Djinn creates the reviewable brief first. No CI or code
                  changes happen until you refine and approve it.
                </p>
              </div>
              <div className="grid gap-2.5 md:grid-cols-3">
                <AgenticReadyArea
                  title="Environment"
                  description="Pinned toolchains and a non-interactive bootstrap from a clean checkout."
                />
                <AgenticReadyArea
                  title="CI parity"
                  description="Compatible build, lint, and test paths locally, in CI, and for agents."
                />
                <AgenticReadyArea
                  title="Reliable validation"
                  description="Fast, deterministic checks with actionable failures and full verification."
                />
              </div>
            </div>
          ) : (
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
          )}
        </div>

        {mutation.isError && (
          <InlineError
            message={
              mutation.error instanceof Error
                ? mutation.error.message
                : "Could not create the first proposal"
            }
            onRetry={createProposal}
          />
        )}

        <Button
          className="w-full"
          disabled={!canCreate || mutation.isPending}
          onClick={createProposal}
        >
          {mutation.isPending
            ? "Creating proposal…"
            : proposalKind === "agentic-ready"
              ? "Create agent-ready draft"
              : "Create custom draft"}
        </Button>

        <p className="text-center text-xs text-muted-foreground">
          Creating a draft does not start agents or change code. You stay in
          control of refinement, approval, and graduation.
        </p>
      </div>
    </OnboardingShell>
  );
}

function ProposalChoice({
  icon,
  title,
  badge,
  description,
  selected,
  disabled,
  onSelect,
}: {
  icon: Parameters<typeof HugeiconsIcon>[0]["icon"];
  title: string;
  badge: string;
  description: string;
  selected: boolean;
  disabled: boolean;
  onSelect: () => void;
}) {
  return (
    <button
      type="button"
      aria-pressed={selected}
      disabled={disabled}
      onClick={onSelect}
      className={`rounded-xl border p-4 text-left transition-colors focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring focus-visible:ring-offset-2 focus-visible:ring-offset-background disabled:cursor-not-allowed disabled:opacity-60 ${
        selected
          ? "border-primary/60 bg-primary/[0.08]"
          : "bg-card/30 hover:border-primary/35 hover:bg-card/55"
      }`}
    >
      <div className="flex items-start justify-between gap-3">
        <div
          className={`flex h-9 w-9 shrink-0 items-center justify-center rounded-lg ${
            selected
              ? "bg-primary/15 text-primary"
              : "bg-muted text-muted-foreground"
          }`}
        >
          <HugeiconsIcon icon={icon} size={19} />
        </div>
        <span
          className={`rounded-full px-2.5 py-1 text-[10px] font-semibold uppercase tracking-wide ${
            selected
              ? "bg-primary/15 text-primary"
              : "bg-muted text-muted-foreground"
          }`}
        >
          {badge}
        </span>
      </div>
      <p className="mt-3 text-sm font-semibold">{title}</p>
      <p className="mt-1 text-xs leading-relaxed text-muted-foreground">
        {description}
      </p>
    </button>
  );
}

function AgenticReadyArea({
  title,
  description,
}: {
  title: string;
  description: string;
}) {
  return (
    <div className="rounded-lg border bg-background/45 px-3 py-3">
      <p className="text-xs font-semibold">{title}</p>
      <p className="mt-1 text-[11px] leading-relaxed text-muted-foreground">
        {description}
      </p>
    </div>
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
