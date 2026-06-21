import { Check, Copy } from "lucide-react";

import { cn } from "@/lib/utils";
import { useClipboard } from "@/hooks/useClipboard";

export interface DiagramFallbackProps {
  /** The raw diagram source to display and copy. */
  source: string;
  /**
   * Optional calm one-line reason shown below the source, e.g.
   * "Could not render diagram: <message>". Omit for the "no renderer"
   * (plantuml) case where there's nothing wrong, just unsupported.
   */
  message?: string;
  className?: string;
}

/**
 * Graceful diagram fallback — shows the raw source in a styled `<pre>` with a
 * copy-source button and (optionally) a calm muted one-line message. Used when a
 * diagram fails to render or has no client renderer, so we never surface a raw
 * developer exception or a blank block.
 */
export function DiagramFallback({
  source,
  message,
  className,
}: DiagramFallbackProps) {
  const { copy, copied } = useClipboard();

  return (
    <div className={cn("space-y-2", className)} data-testid="diagram-fallback">
      <div className="relative">
        <button
          type="button"
          onClick={() => void copy(source)}
          aria-label="Copy diagram source"
          title="Copy source"
          className="absolute right-2 top-2 flex size-7 items-center justify-center rounded-md border border-border/60 bg-background/80 text-muted-foreground shadow-sm backdrop-blur transition-colors hover:text-foreground focus-visible:outline-none focus-visible:ring-1 focus-visible:ring-ring"
        >
          {copied ? (
            <Check className="size-4 text-emerald-400" />
          ) : (
            <Copy className="size-4" />
          )}
        </button>
        <pre className="overflow-auto whitespace-pre-wrap rounded-md border bg-muted p-3 pr-10 font-mono text-[11px] text-muted-foreground">
          {source}
        </pre>
      </div>
      {message ? (
        <p className="text-xs text-muted-foreground">{message}</p>
      ) : null}
    </div>
  );
}
