import { type ReactNode } from "react";
import { cn } from "@/lib/utils";

interface PageHeaderProps {
  /** Primary page title. */
  title: string;
  /** Optional subtitle / context copy rendered below the title. */
  subtitle?: ReactNode;
  /** Optional leading element (e.g. back button) rendered to the left of the title. */
  leading?: ReactNode;
  /** Optional trailing actions (e.g. buttons) rendered on the right. */
  actions?: ReactNode;
  /** Additional class names applied to the outer wrapper. */
  className?: string;
  /** Optional children rendered below the header row. */
  children?: ReactNode;
}

/**
 * Reusable page-level header chrome.
 *
 * Provides a consistent title / subtitle / actions layout without imposing
 * page-specific logic.  Pages compose it with their own content.
 */
export function PageHeader({
  title,
  subtitle,
  leading,
  actions,
  className,
  children,
}: PageHeaderProps) {
  return (
    <div className={cn("mb-4", className)}>
      <div className="flex items-center justify-between gap-3">
        <div className="flex items-center gap-3 min-w-0">
          {leading}
          <div className="min-w-0">
            <h1 className="text-lg font-semibold tracking-tight truncate">
              {title}
            </h1>
            {subtitle && (
              <p className="text-sm text-muted-foreground mt-0.5">{subtitle}</p>
            )}
          </div>
        </div>
        {actions && (
          <div className="flex items-center gap-2 shrink-0">{actions}</div>
        )}
      </div>
      {children}
    </div>
  );
}
