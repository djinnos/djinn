import { cn } from "@/lib/utils"

interface SpinnerProps extends React.ComponentProps<"span"> {
  size?: "xs" | "sm" | "default" | "lg" | "xl"
  variant?: "default" | "primary" | "secondary" | "ghost"
}

/**
 * Loading spinner component with size variants
 * Use for inline loading states and button spinners
 */
function Spinner({
  className,
  size = "default",
  variant = "default",
  ...props
}: SpinnerProps) {
  const sizeClasses = {
    xs: "h-2.5 w-2.5",
    sm: "h-3 w-3",
    default: "h-4 w-4",
    lg: "h-5 w-5",
    xl: "h-6 w-6",
  }

  const variantClasses = {
    default: "text-foreground",
    primary: "text-primary",
    secondary: "text-secondary-foreground",
    ghost: "text-muted-foreground",
  }

  return (
    <span
      data-slot="spinner"
      className={cn(
        "inline-flex items-center justify-center",
        sizeClasses[size],
        variantClasses[variant],
        className
      )}
      {...props}
    >
      <svg
        className="animate-spin"
        xmlns="http://www.w3.org/2000/svg"
        fill="none"
        viewBox="0 0 24 24"
        aria-hidden="true"
      >
        <circle
          className="opacity-25"
          cx="12"
          cy="12"
          r="10"
          stroke="currentColor"
          strokeWidth="4"
        />
        <path
          className="opacity-75"
          fill="currentColor"
          d="M4 12a8 8 0 018-8V0C5.373 0 0 5.373 0 12h4zm2 5.291A7.962 7.962 0 014 12H0c0 3.042 1.135 5.824 3 7.938l3-2.647z"
        />
      </svg>
    </span>
  )
}

export { Spinner }
export type { SpinnerProps }
