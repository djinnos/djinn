import type { ReactNode } from "react";
import { Button } from "@/components/ui/button";
import type {
  ReadinessArea,
  ReadinessJson,
  ReadinessRunDetail,
} from "@/api/readiness";

export interface ProjectReadinessPanelProps {
  /** The page supplies an already-fetched projection; this component never reads it itself. */
  detail: ReadinessRunDetail | null;
  /** Identifying context from the containing project page, such as the authorized owner. */
  ownerContext: ReactNode;
  /** True only while the page's injected kickoff action is pending. */
  isStarting?: boolean;
  /** A page-level kickoff failure to surface alongside the otherwise empty state. */
  startError?: string | null;
  /** The only action this read-only panel exposes. */
  onStart?: () => void;
}

const STATUS_LABELS: Record<string, string> = {
  identifying: "Identifying repository composition",
  analyzing: "Analyzing readiness areas",
  aggregating: "Aggregating readiness results",
  completed: "Readiness analysis completed",
  completed_with_errors: "Readiness analysis completed with errors",
  failed: "Readiness analysis failed",
};

function statusLabel(status: string): string {
  return STATUS_LABELS[status] ?? `Readiness status: ${status}`;
}

function renderJson(value: ReadinessJson): string {
  if (typeof value === "string") return value;
  if (value === null) return "none";
  return JSON.stringify(value);
}

function objectEntries(value: ReadinessJson, key: string): ReadinessJson[] {
  if (typeof value !== "object" || value === null || Array.isArray(value)) return [];
  const candidate = value[key];
  return Array.isArray(candidate) ? [...candidate] : [];
}

function field(value: ReadinessJson, keys: string[]): ReadinessJson | undefined {
  if (typeof value !== "object" || value === null || Array.isArray(value)) return undefined;
  for (const key of keys) {
    if (key in value) return value[key];
  }
  return undefined;
}

function OutputEntries({ area, kind, title }: { area: ReadinessArea; kind: "unsupported" | "warnings" | "errors"; title: string }) {
  const entries = area.accepted_outputs.flatMap((output) => objectEntries(output.result, kind));
  if (entries.length === 0) return null;

  return (
    <section aria-label={`${area.area_key} ${title}`}>
      <h4 className="font-medium">{title}</h4>
      <ul className="list-disc pl-5 text-sm">
        {entries.map((entry, index) => <li key={`${kind}-${index}`}>{renderJson(entry)}</li>)}
      </ul>
    </section>
  );
}

function AreaDetail({ area, score }: { area: ReadinessArea; score: ReadinessRunDetail["area_scores"][number] | undefined }) {
  const currentAttempt = area.attempts.find((attempt) => attempt.is_current);
  return (
    <article className="space-y-3 rounded-md border p-3" aria-label={`Readiness area ${area.area_key}`}>
      <div className="flex flex-wrap items-center gap-x-3 gap-y-1">
        <h3 className="font-semibold">{area.area_key}</h3>
        <span>Area status: {area.status}</span>
        {score && <span>Persisted area score: {score.score}</span>}
      </div>
      <dl className="grid gap-1 text-sm sm:grid-cols-2">
        <div><dt className="font-medium">Composition</dt><dd>{renderJson(area.composition)}</dd></div>
        <div><dt className="font-medium">Path scopes</dt><dd>{renderJson(area.path_scopes)}</dd></div>
      </dl>
      {currentAttempt && (
        <p className="text-sm" data-testid={`current-attempt-${area.id}`}>
          Current attempt: #{currentAttempt.attempt_number} — {currentAttempt.status}
          {currentAttempt.payload_digest ? ` (${currentAttempt.payload_digest})` : ""}
        </p>
      )}
      {area.accepted_findings.length > 0 && (
        <section aria-label={`${area.area_key} accepted findings`}>
          <h4 className="font-medium">Accepted findings</h4>
          <ul className="space-y-2 text-sm">
            {area.accepted_findings.map((finding) => (
              <li key={finding.id} className="rounded border p-2">
                <strong>{finding.guardrail_key}</strong>: {finding.status} ({finding.severity})
                <div>Confidence: {finding.confidence}</div>
                <div>Evidence: {renderJson(finding.evidence)}</div>
              </li>
            ))}
          </ul>
        </section>
      )}
      <OutputEntries area={area} kind="unsupported" title="Unsupported entries" />
      <OutputEntries area={area} kind="warnings" title="Warnings" />
      <OutputEntries area={area} kind="errors" title="Errors" />
    </article>
  );
}

function EventDiagnostics({ detail }: { detail: ReadinessRunDetail }) {
  const diagnostics = detail.events.filter((event) =>
    event.event_kind.includes("error") ||
    event.event_kind.includes("failed") ||
    objectEntries(event.payload, "warnings").length > 0 ||
    objectEntries(event.payload, "errors").length > 0,
  );
  if (diagnostics.length === 0) return null;

  return (
    <section aria-label="Run diagnostics" className="rounded-md border p-3">
      <h3 className="font-semibold">Run diagnostics</h3>
      <ul className="list-disc pl-5 text-sm">
        {diagnostics.map((event) => <li key={event.id}>{event.event_kind}: {renderJson(event.payload)}</li>)}
      </ul>
    </section>
  );
}

function TerminalDetail({ detail, ownerContext }: { detail: ReadinessRunDetail; ownerContext: ReactNode }) {
  const scoresByArea = new Map(detail.area_scores.map((score) => [score.area_id, score]));
  const suggestions = [...new Map(detail.suggestions.map((suggestion) => [suggestion.dedupe_key, suggestion])).values()];
  return (
    <div className="space-y-4" data-testid="readiness-terminal-detail">
      <section aria-label="Readiness run context" className="rounded-md border p-3">
        <h3 className="font-semibold">Readiness run</h3>
        <dl className="grid gap-1 text-sm sm:grid-cols-2">
          <div><dt className="font-medium">Owner</dt><dd>{ownerContext}</dd></div>
          <div><dt className="font-medium">Repository snapshot</dt><dd>{detail.run.repository_snapshot}</dd></div>
          <div><dt className="font-medium">Pinned skill</dt><dd>{detail.run.skill_name} v{detail.run.skill_version}</dd></div>
          <div><dt className="font-medium">Run status</dt><dd>{detail.run.status}</dd></div>
          {detail.project_score && <div><dt className="font-medium">Persisted project score</dt><dd>Score: {detail.project_score.score} — Band: {detail.project_score.band}</dd></div>}
        </dl>
      </section>
      <section aria-label="Composition areas" className="space-y-3">
        <h3 className="font-semibold">Composition areas</h3>
        {detail.areas.map((area) => <AreaDetail key={area.id} area={area} score={scoresByArea.get(area.id)} />)}
      </section>
      <EventDiagnostics detail={detail} />
      {suggestions.length > 0 && (
        <section aria-label="Suggested next actions" className="rounded-md border p-3">
          <h3 className="font-semibold">Suggested next actions</h3>
          <ul className="space-y-2 text-sm">
            {suggestions.map((suggestion) => {
              const guidance = field(suggestion.suggestion, ["validation_guidance", "validation", "validate"]);
              return <li key={suggestion.id} className="rounded border p-2">
                <strong>{suggestion.dedupe_key}</strong>: {renderJson(suggestion.suggestion)}
                <div>Validation guidance: {guidance === undefined ? "Validate this action against the repository evidence before changing code." : renderJson(guidance)}</div>
              </li>;
            })}
          </ul>
        </section>
      )}
    </div>
  );
}

/** Read-only rendering of the latest readiness DTO supplied by a project page. */
export function ProjectReadinessPanel({ detail, ownerContext, isStarting = false, startError = null, onStart }: ProjectReadinessPanelProps) {
  if (isStarting) {
    return <section aria-labelledby="project-readiness-title" className="space-y-2 rounded-md border p-4">
      <h2 id="project-readiness-title" className="text-lg font-semibold">Project readiness</h2>
      <p role="status">Starting readiness analysis…</p>
    </section>;
  }

  if (!detail) {
    return <section aria-labelledby="project-readiness-title" className="space-y-3 rounded-md border p-4">
      <h2 id="project-readiness-title" className="text-lg font-semibold">Project readiness</h2>
      <p role="status">No readiness analysis has started.</p>
      {startError && <p role="alert">Unable to start readiness analysis: {startError}</p>}
      {onStart && <Button type="button" onClick={onStart}>Start readiness analysis</Button>}
    </section>;
  }

  const failed = detail.run.status === "failed";
  return <section aria-labelledby="project-readiness-title" className="space-y-4 rounded-md border p-4">
    <div className="flex flex-wrap items-center justify-between gap-2">
      <h2 id="project-readiness-title" className="text-lg font-semibold">Project readiness</h2>
      <p role={failed ? "alert" : "status"} aria-live="polite">{statusLabel(detail.run.status)}</p>
    </div>
    <TerminalDetail detail={detail} ownerContext={ownerContext} />
  </section>;
}
