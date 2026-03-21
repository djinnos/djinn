import { AlertCircleIcon, Loading02Icon } from '@hugeicons/core-free-icons';
import { HugeiconsIcon } from '@hugeicons/react';
import { Button } from '@/components/ui/button';

interface InlineErrorProps {
  message: string;
  onRetry?: () => void;
  retrying?: boolean;
}

export function InlineError({ message, onRetry, retrying = false }: InlineErrorProps) {
  return (
    <div className="rounded-lg border border-destructive/40 bg-destructive/5 p-4">
      <div className="flex items-start justify-between gap-3">
        <div className="flex items-start gap-2 text-destructive">
          <HugeiconsIcon icon={AlertCircleIcon} size={16} className="mt-0.5" />
          <p className="text-sm">{message}</p>
        </div>
        {onRetry && (
          <Button variant="outline" size="sm" onClick={onRetry} disabled={retrying}>
            {retrying ? <HugeiconsIcon icon={Loading02Icon} size={16} className="animate-spin" /> : 'Retry'}
          </Button>
        )}
      </div>
    </div>
  );
}
