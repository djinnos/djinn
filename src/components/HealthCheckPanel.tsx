import { Button } from '@/components/ui/button';
import { StepLog } from '@/components/StepLog';
import type { VerificationRun } from '@/stores/verificationStore';
import { RotateCcw } from 'lucide-react';

interface HealthCheckPanelProps {
  projectName: string;
  run: VerificationRun | null;
  open: boolean;
  onClose: () => void;
}

function formatTimestamp(iso?: string): string {
  if (!iso) return '—';
  const date = new Date(iso);
  if (Number.isNaN(date.getTime())) return '—';
  return date.toLocaleString();
}

export function HealthCheckPanel({ projectName, run, open, onClose }: HealthCheckPanelProps) {
  if (!open) return null;

  return (
    <div className="fixed inset-0 z-50 flex justify-end bg-black/40" role="dialog" aria-modal="true">
      <button type="button" className="h-full flex-1 cursor-default" onClick={onClose} aria-label="Close health check details" />
      <aside className="h-full w-full max-w-2xl overflow-y-auto border-l bg-background p-6 shadow-2xl">
        <div className="mb-4 flex items-start justify-between gap-2">
          <div className="space-y-1">
            <h2 className="text-xl font-semibold">Health Check — {projectName}</h2>
            <p className="text-sm text-muted-foreground">Latest healthcheck run details</p>
          </div>
          <Button type="button" variant="outline" onClick={onClose}>Close</Button>
        </div>

        <div className="space-y-4">
          {run ? (
            <StepLog steps={run.steps} status={run.status} className="bg-background" />
          ) : (
            <div className="rounded-md border border-border bg-card p-3 text-sm text-muted-foreground">
              No healthcheck run available.
            </div>
          )}
        </div>

        <div className="mt-6 flex items-center justify-between border-t pt-4 text-sm text-muted-foreground">
          <span>Last run: {formatTimestamp(run?.startedAt)}</span>
          <Button type="button" variant="outline" size="sm" className="gap-1.5" disabled>
            <RotateCcw className="h-3.5 w-3.5" />
            Re-run
          </Button>
        </div>
      </aside>
    </div>
  );
}
