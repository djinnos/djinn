import { AlertCircleIcon, Loader2Icon } from 'lucide-react';
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
          <AlertCircleIcon className="mt-0.5 h-4 w-4" />
          <p className="text-sm">{message}</p>
        </div>
        {onRetry && (
          <Button variant="outline" size="sm" onClick={onRetry} disabled={retrying}>
            {retrying ? <Loader2Icon className="h-4 w-4 animate-spin" /> : 'Retry'}
          </Button>
        )}
      </div>
    </div>
  );
}
