import * as React from "react"
import { Button } from "./button"
import { Spinner } from "./spinner"
import { cn } from "@/lib/utils"
import type { VariantProps } from "class-variance-authority"
import { cva } from "class-variance-authority"

const loadingButtonVariants = cva(
  "relative inline-flex items-center justify-center",
  {
    variants: {
      loadingPosition: {
        start: "",
        end: "",
        center: "",
      },
    },
    defaultVariants: {
      loadingPosition: "start",
    },
  }
)

interface LoadingButtonProps
  extends React.ComponentProps<typeof Button>,
    VariantProps<typeof loadingButtonVariants> {
  loading?: boolean
  loadingText?: string
  loadingPosition?: "start" | "end" | "center"
  spinnerSize?: "xs" | "sm" | "default" | "lg"
}

/**
 * Button component with built-in loading state and spinner
 * Shows a spinner when loading is true, optionally with loading text
 */
const LoadingButton = React.forwardRef<
  React.ComponentRef<typeof Button>,
  LoadingButtonProps
>(
  (
    {
      children,
      loading = false,
      loadingText,
      loadingPosition = "start",
      spinnerSize,
      disabled,
      className,
      ...props
    },
    ref
  ) => {
    // Determine spinner size based on button size if not explicitly provided
    const resolvedSpinnerSize = spinnerSize ?? (
      props.size === "xs" ? "xs" :
      props.size === "sm" ? "sm" :
      props.size === "lg" ? "lg" : "default"
    )

    const spinnerVariant =
      props.variant === "destructive" ? "default" :
      props.variant === "ghost" ? "ghost" :
      props.variant === "secondary" ? "secondary" :
      props.variant === "outline" ? "default" :
      "primary"

    const content = loading ? (
      <>
        {loadingPosition === "center" ? (
          // Center spinner with optional text below
          <span className="flex flex-col items-center gap-1">
            <Spinner size={resolvedSpinnerSize} variant={spinnerVariant} />
            {loadingText && (
              <span className="text-xs">{loadingText}</span>
            )}
          </span>
        ) : (
          // Inline spinner at start or end
          <>
            {loadingPosition === "start" && (
              <Spinner size={resolvedSpinnerSize} variant={spinnerVariant} />
            )}
            {loadingText || children}
            {loadingPosition === "end" && (
              <Spinner size={resolvedSpinnerSize} variant={spinnerVariant} />
            )}
          </>
        )}
      </>
    ) : (
      children
    )

    return (
      <Button
        ref={ref}
        disabled={disabled || loading}
        className={cn(
          loadingButtonVariants({ loadingPosition }),
          loading && loadingPosition === "center" && "pointer-events-none",
          className
        )}
        {...props}
      >
        {content}
      </Button>
    )
  }
)

LoadingButton.displayName = "LoadingButton"

export { LoadingButton, loadingButtonVariants }
export type { LoadingButtonProps }
