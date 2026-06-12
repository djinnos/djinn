import { SparklesIcon } from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";

import { Badge } from "@/components/ui/badge";

/** Compact badge marking the automation service user in a roster. */
export function ServiceBadge() {
  return (
    <Badge variant="secondary" className="gap-1">
      <HugeiconsIcon icon={SparklesIcon} size={12} />
      Service
    </Badge>
  );
}
