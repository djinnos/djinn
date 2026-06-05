import { Copy01Icon, Tick02Icon } from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";

import { Button } from "@/components/ui/button";
import { useClipboard } from "@/hooks/useClipboard";
import { cn } from "@/lib/utils";

/**
 * Small ghost icon button that copies `text` to the clipboard and flips to a
 * checkmark for a moment. Reused for proposal comments and the full proposal
 * spec (e.g. to paste into an AI to discuss).
 */
export function CopyButton({
  text,
  label = "Copy",
  size = 14,
  className,
}: {
  text: string;
  label?: string;
  size?: number;
  className?: string;
}) {
  const { copy, copied } = useClipboard();
  return (
    <Button
      type="button"
      variant="ghost"
      size="icon"
      className={cn("size-6 text-muted-foreground hover:text-foreground", className)}
      aria-label={label}
      title={copied ? "Copied" : label}
      onClick={(e) => {
        e.stopPropagation();
        void copy(text);
      }}
    >
      <HugeiconsIcon icon={copied ? Tick02Icon : Copy01Icon} size={size} />
    </Button>
  );
}
