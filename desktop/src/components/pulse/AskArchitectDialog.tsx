import { useId, useMemo, useState } from "react";
import type { ChangeEvent, FormEvent } from "react";
import { useNavigate } from "react-router-dom";
import type { TaskCreateOutput } from "@/api/generated/mcp-tools.gen";
import type { Task } from "@/api/types";
import { callMcpTool } from "@/api/mcpClient";
import { taskStore } from "@/stores/taskStore";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
  DialogTrigger,
} from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Textarea } from "@/components/ui/textarea";
import { showToast } from "@/lib/toast";
import { recordPulseOriginatedSpike } from "@/lib/pulseProposals";
import { useAuthUser } from "@/components/AuthGate";

interface AskArchitectDialogProps {
  projectPath: string;
}

function buildTaskTitle(question: string): string {
  const normalized = question.trim().replace(/\s+/g, " ");
  return normalized.length <= 120 ? normalized : `${normalized.slice(0, 117)}...`;
}

function buildTaskDescription(question: string, context: string): string {
  const trimmedQuestion = question.trim();
  const trimmedContext = context.trim();

  return trimmedContext
    ? [`## Question`, trimmedQuestion, ``, `## Context`, trimmedContext].join("\n")
    : [`## Question`, trimmedQuestion].join("\n");
}

function normalizeCreatedTask(task: TaskCreateOutput): Task | null {
  if (!task || typeof task !== "object") return null;
  const candidate = task as Partial<Task>;
  if (!candidate.id || !candidate.title || !candidate.status) return null;

  return {
    ...(candidate as Task),
    owner: candidate.owner ?? null,
    project_id: candidate.project_id ?? null,
    description: candidate.description ?? "",
    design: candidate.design ?? "",
    labels: candidate.labels ?? [],
    acceptance_criteria: candidate.acceptance_criteria ?? [],
  } as Task;
}

export function AskArchitectDialog({ projectPath }: AskArchitectDialogProps) {
  const navigate = useNavigate();
  const user = useAuthUser();
  const questionId = useId();
  const contextId = useId();
  const [open, setOpen] = useState(false);
  const [question, setQuestion] = useState("");
  const [context, setContext] = useState("");
  const [submitting, setSubmitting] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const trimmedQuestion = question.trim();
  const canSubmit = trimmedQuestion.length > 0 && !submitting;
  const titlePreview = useMemo(() => buildTaskTitle(trimmedQuestion || question), [trimmedQuestion, question]);

  const reset = () => {
    setQuestion("");
    setContext("");
    setError(null);
    setSubmitting(false);
  };

  const handleOpenChange = (nextOpen: boolean) => {
    if (submitting) return;
    setOpen(nextOpen);
    if (!nextOpen) {
      reset();
    }
  };

  const handleSubmit = async (event: FormEvent<HTMLFormElement>) => {
    event.preventDefault();
    const normalizedQuestion = question.trim();
    const normalizedContext = context.trim();

    if (!normalizedQuestion) {
      setError("Question is required.");
      return;
    }

    setSubmitting(true);
    setError(null);

    try {
      const created = await callMcpTool("task_create", {
        project: projectPath,
        issue_type: "spike",
        title: buildTaskTitle(normalizedQuestion),
        description: buildTaskDescription(normalizedQuestion, normalizedContext),
      });

      const task = normalizeCreatedTask(created);
      const createdTaskId = task?.id ?? (typeof created === "object" && created && "id" in created ? String(created.id) : null);

      if (!createdTaskId) {
        throw new Error("Task created but no task id was returned.");
      }

      recordPulseOriginatedSpike(createdTaskId, user);

      if (task) {
        taskStore.getState().addTask(task);
      }

      setOpen(false);
      reset();
      navigate(`/task/${createdTaskId}`);
      showToast.success("Architect spike created", {
        description: "Opening the new task so you can watch it dispatch.",
      });
    } catch (submitError) {
      const message = submitError instanceof Error ? submitError.message : "Failed to create architect spike.";
      setError(message);
      showToast.error("Could not create architect spike", {
        description: message,
      });
    } finally {
      setSubmitting(false);
    }
  };

  const handleQuestionChange = (event: ChangeEvent<HTMLTextAreaElement>) => {
    setQuestion(event.target.value);
  };

  const handleContextChange = (event: ChangeEvent<HTMLTextAreaElement>) => {
    setContext(event.target.value);
  };

  return (
    <Dialog open={open} onOpenChange={handleOpenChange}>
      <DialogTrigger render={<Button variant="outline">Ask architect</Button>} />
      <DialogContent className="sm:max-w-lg">
        <DialogHeader>
          <DialogTitle>Ask architect</DialogTitle>
          <DialogDescription>
            Create a spike for the Architect. Project context comes from the currently selected Pulse project.
          </DialogDescription>
        </DialogHeader>

        <form className="space-y-4" onSubmit={handleSubmit}>
          <div className="space-y-2">
            <Label htmlFor={questionId}>Question</Label>
            <Textarea
              id={questionId}
              value={question}
              onChange={handleQuestionChange}
              placeholder="What should the architect investigate or propose?"
              aria-invalid={error?.toLowerCase().includes("question") || undefined}
              disabled={submitting}
              rows={4}
              autoFocus
            />
            <p className="text-xs text-muted-foreground">
              This becomes the spike title and the main question in the task description.
            </p>
          </div>

          <div className="space-y-2">
            <Label htmlFor={contextId}>Context</Label>
            <Textarea
              id={contextId}
              value={context}
              onChange={handleContextChange}
              placeholder="Optional background, constraints, or links the architect should consider."
              disabled={submitting}
              rows={5}
            />
            <p className="text-xs text-muted-foreground">
              Optional. If provided, it will be appended under a <span className="font-medium text-foreground">Context</span> heading.
            </p>
          </div>

          <div className="space-y-2 rounded-lg border border-border/70 bg-muted/30 p-3">
            <Label htmlFor={`${questionId}-preview`}>Task title preview</Label>
            <Input
              id={`${questionId}-preview`}
              value={titlePreview}
              readOnly
              disabled
            />
          </div>

          {error && (
            <div className="rounded-lg border border-destructive/40 bg-destructive/10 px-3 py-2 text-sm text-destructive">
              {error}
            </div>
          )}

          <DialogFooter>
            <Button type="submit" disabled={!canSubmit}>
              {submitting ? "Creating spike..." : "Create spike"}
            </Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  );
}
