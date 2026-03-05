import { Copy01Icon, Tick02Icon } from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";
import { useClipboard } from "@/hooks/useClipboard";
import { cn } from "@/lib/utils";

interface TaskIdLabelProps {
  taskId: string;
  shortId?: string;
  feedbackDuration?: number;
  className?: string;
}

export function TaskIdLabel({
  taskId,
  shortId,
  feedbackDuration = 1500,
  className,
}: TaskIdLabelProps) {
  const { copy, copied } = useClipboard({ feedbackDuration });

  const handleClick = (e: React.MouseEvent) => {
    e.stopPropagation();
    copy(shortId || taskId);
  };

  const displayId = shortId || taskId;
  const tooltip = shortId ? `${taskId} — click to copy` : "Click to copy";

  return (
    <button
      type="button"
      onClick={handleClick}
      className={cn(
        "inline-flex items-center gap-1",
        "text-xs text-muted-foreground font-mono cursor-pointer",
        "hover:text-foreground transition-colors rounded",
        "select-none",
        "focus:outline-none focus:ring-2 focus:ring-primary focus:ring-offset-2",
        "border-none bg-transparent p-0 text-left",
        className,
      )}
      title={tooltip}
      aria-label={displayId}
    >
      <span>{displayId}</span>
      <HugeiconsIcon
        icon={copied ? Tick02Icon : Copy01Icon}
        size={11}
        className={cn(
          "shrink-0 transition-colors",
          copied ? "text-green-500" : "text-muted-foreground/50",
        )}
        aria-hidden="true"
      />
    </button>
  );
}
