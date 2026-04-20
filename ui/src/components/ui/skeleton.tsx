import { cn } from "@/lib/utils"

/**
 * Base skeleton component with animation
 * Use as a building block for other skeleton components
 */
function Skeleton({
  className,
  ...props
}: React.ComponentProps<"div">) {
  return (
    <div
      data-slot="skeleton"
      className={cn(
        "animate-pulse rounded-md bg-muted",
        className
      )}
      {...props}
    />
  )
}

/**
 * Card skeleton - matches Card component layout
 */
function CardSkeleton({ className, ...props }: React.ComponentProps<"div">) {
  return (
    <div
      data-slot="card-skeleton"
      className={cn(
        "flex flex-col gap-4 overflow-hidden rounded-lg bg-card p-4 ring-1 ring-foreground/10",
        className
      )}
      {...props}
    >
      {/* Header with title */}
      <div className="flex items-start justify-between gap-2">
        <div className="flex flex-col gap-2 flex-1">
          <Skeleton className="h-4 w-3/4" />
          <Skeleton className="h-3 w-1/2" />
        </div>
        <Skeleton className="h-8 w-8 rounded-full" />
      </div>
      {/* Content */}
      <div className="flex flex-col gap-2">
        <Skeleton className="h-3 w-full" />
        <Skeleton className="h-3 w-5/6" />
        <Skeleton className="h-3 w-4/5" />
      </div>
      {/* Footer */}
      <div className="flex items-center justify-between pt-2">
        <Skeleton className="h-7 w-20" />
        <Skeleton className="h-7 w-16" />
      </div>
    </div>
  )
}

/**
 * List skeleton - for list views with multiple items
 */
function ListSkeleton({
  className,
  itemCount = 5,
  ...props
}: React.ComponentProps<"div"> & { itemCount?: number }) {
  return (
    <div
      data-slot="list-skeleton"
      className={cn("flex flex-col gap-2", className)}
      {...props}
    >
      {Array.from({ length: itemCount }).map((_, i) => (
        <div
          key={i}
          className="flex items-center gap-3 rounded-lg bg-card p-3 ring-1 ring-foreground/10"
        >
          <Skeleton className="h-10 w-10 rounded-full" />
          <div className="flex flex-col gap-2 flex-1">
            <Skeleton className="h-3 w-1/3" />
            <Skeleton className="h-3 w-1/2" />
          </div>
          <Skeleton className="h-8 w-8" />
        </div>
      ))}
    </div>
  )
}

/**
 * Panel skeleton - for sidebar panels, info panels, etc.
 */
function PanelSkeleton({
  className,
  rowCount = 4,
  ...props
}: React.ComponentProps<"div"> & { rowCount?: number }) {
  return (
    <div
      data-slot="panel-skeleton"
      className={cn(
        "flex flex-col gap-3 rounded-lg bg-card p-4 ring-1 ring-foreground/10",
        className
      )}
      {...props}
    >
      {/* Header */}
      <div className="flex items-center gap-2 pb-2 border-b border-border">
        <Skeleton className="h-5 w-5 rounded" />
        <Skeleton className="h-4 w-24" />
      </div>
      {/* Rows */}
      <div className="flex flex-col gap-3">
        {Array.from({ length: rowCount }).map((_, i) => (
          <div key={i} className="flex items-center gap-3">
            <Skeleton className="h-4 w-4 rounded" />
            <Skeleton className="h-3 flex-1" />
          </div>
        ))}
      </div>
    </div>
  )
}

/**
 * Text skeleton - for text content areas
 */
function TextSkeleton({
  className,
  lines = 3,
  ...props
}: React.ComponentProps<"div"> & { lines?: number }) {
  return (
    <div
      data-slot="text-skeleton"
      className={cn("flex flex-col gap-2", className)}
      {...props}
    >
      {Array.from({ length: lines }).map((_, i) => (
        <Skeleton
          key={i}
          className={cn(
            "h-3",
            i === lines - 1 ? "w-3/4" : "w-full"
          )}
        />
      ))}
    </div>
  )
}

/**
 * Table skeleton - for table loading states
 */
function TableSkeleton({
  className,
  rows = 5,
  columns = 4,
  ...props
}: React.ComponentProps<"div"> & { rows?: number; columns?: number }) {
  return (
    <div
      data-slot="table-skeleton"
      className={cn("w-full", className)}
      {...props}
    >
      {/* Header */}
      <div className="flex gap-2 pb-3 border-b border-border">
        {Array.from({ length: columns }).map((_, i) => (
          <Skeleton
            key={`header-${i}`}
            className={cn("h-4", i === 0 ? "flex-[2]" : "flex-1")}
          />
        ))}
      </div>
      {/* Rows */}
      <div className="flex flex-col gap-2 pt-2">
        {Array.from({ length: rows }).map((_, rowIndex) => (
          <div key={`row-${rowIndex}`} className="flex gap-2">
            {Array.from({ length: columns }).map((_, colIndex) => (
              <Skeleton
                key={`cell-${rowIndex}-${colIndex}`}
                className={cn("h-8", colIndex === 0 ? "flex-[2]" : "flex-1")}
              />
            ))}
          </div>
        ))}
      </div>
    </div>
  )
}

/**
 * Form skeleton - for form loading states
 */
function FormSkeleton({
  className,
  fields = 4,
  ...props
}: React.ComponentProps<"div"> & { fields?: number }) {
  return (
    <div
      data-slot="form-skeleton"
      className={cn("flex flex-col gap-4", className)}
      {...props}
    >
      {Array.from({ length: fields }).map((_, i) => (
        <div key={i} className="flex flex-col gap-2">
          <Skeleton className="h-3 w-20" />
          <Skeleton className="h-9 w-full" />
        </div>
      ))}
      <Skeleton className="h-9 w-24 mt-2" />
    </div>
  )
}

/**
 * Stats card skeleton - for dashboard/stat cards
 */
function StatsCardSkeleton({
  className,
  ...props
}: React.ComponentProps<"div">) {
  return (
    <div
      data-slot="stats-card-skeleton"
      className={cn(
        "flex flex-col gap-2 rounded-lg bg-card p-4 ring-1 ring-foreground/10",
        className
      )}
      {...props}
    >
      <div className="flex items-center justify-between">
        <Skeleton className="h-4 w-16" />
        <Skeleton className="h-8 w-8 rounded-full" />
      </div>
      <Skeleton className="h-8 w-20" />
      <Skeleton className="h-3 w-24" />
    </div>
  )
}

/**
 * Avatar skeleton - for user/profile loading states
 */
function AvatarSkeleton({
  className,
  size = "md",
  ...props
}: React.ComponentProps<"div"> & { size?: "sm" | "md" | "lg" | "xl" }) {
  const sizeClasses = {
    sm: "h-8 w-8",
    md: "h-10 w-10",
    lg: "h-12 w-12",
    xl: "h-16 w-16",
  }

  return (
    <div
      data-slot="avatar-skeleton"
      className={cn("flex items-center gap-3", className)}
      {...props}
    >
      <Skeleton className={cn("rounded-full", sizeClasses[size])} />
      <div className="flex flex-col gap-2">
        <Skeleton className="h-3 w-24" />
        <Skeleton className="h-3 w-16" />
      </div>
    </div>
  )
}

export {
  Skeleton,
  CardSkeleton,
  ListSkeleton,
  PanelSkeleton,
  TextSkeleton,
  TableSkeleton,
  FormSkeleton,
  StatsCardSkeleton,
  AvatarSkeleton,
}
