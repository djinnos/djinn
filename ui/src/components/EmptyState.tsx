import { ReactNode } from 'react';
import { Button } from '@/components/ui/button';

interface EmptyStateProps {
  title: string;
  message: string;
  actionLabel: string;
  onAction: () => void;
  illustration?: ReactNode;
}

export function EmptyState({ title, message, actionLabel, onAction, illustration }: EmptyStateProps) {
  return (
    <div className="flex h-full min-h-[280px] flex-col items-center justify-center rounded-lg border border-dashed border-border bg-card/50 p-8 text-center">
      <div className="mb-4 text-muted-foreground">{illustration ?? <div className="text-4xl">✨</div>}</div>
      <h3 className="text-lg font-semibold text-foreground">{title}</h3>
      <p className="mt-2 max-w-md text-sm text-muted-foreground">{message}</p>
      <Button className="mt-5" onClick={onAction}>
        {actionLabel}
      </Button>
    </div>
  );
}
