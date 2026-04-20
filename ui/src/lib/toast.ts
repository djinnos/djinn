import { toast } from "sonner"

interface ToastOptions {
  description?: string
  action?: {
    label: string
    onClick: () => void
  }
  duration?: number
}

/**
 * Toast notification utility with success, error, and info variants.
 * Uses Sonner for toast notifications.
 */
export const showToast = {
  /**
   * Show a success toast notification
   */
  success: (title: string, options?: ToastOptions) => {
    return toast.success(title, {
      description: options?.description,
      action: options?.action
        ? {
            label: options.action.label,
            onClick: options.action.onClick,
          }
        : undefined,
      duration: options?.duration ?? 4000,
    })
  },

  /**
   * Show an error toast notification
   */
  error: (title: string, options?: ToastOptions) => {
    return toast.error(title, {
      description: options?.description,
      action: options?.action
        ? {
            label: options.action.label,
            onClick: options.action.onClick,
          }
        : undefined,
      duration: options?.duration ?? 5000,
    })
  },

  /**
   * Show an info toast notification
   */
  info: (title: string, options?: ToastOptions) => {
    return toast.info(title, {
      description: options?.description,
      action: options?.action
        ? {
            label: options.action.label,
            onClick: options.action.onClick,
          }
        : undefined,
      duration: options?.duration ?? 4000,
    })
  },

  /**
   * Show a warning toast notification
   */
  warning: (title: string, options?: ToastOptions) => {
    return toast.warning(title, {
      description: options?.description,
      action: options?.action
        ? {
            label: options.action.label,
            onClick: options.action.onClick,
          }
        : undefined,
      duration: options?.duration ?? 4000,
    })
  },

  /**
   * Show a loading toast notification (returns dismiss function)
   */
  loading: (title: string, options?: Omit<ToastOptions, "action" | "duration">) => {
    return toast.loading(title, {
      description: options?.description,
    })
  },

  /**
   * Dismiss a specific toast by ID or all toasts if no ID provided
   */
  dismiss: (toastId?: string | number) => {
    toast.dismiss(toastId)
  },

  /**
   * Update an existing toast
   */
  update: (toastId: string | number, title: string, options?: ToastOptions) => {
    toast.success(title, {
      id: toastId,
      description: options?.description,
      action: options?.action
        ? {
            label: options.action.label,
            onClick: options.action.onClick,
          }
        : undefined,
      duration: options?.duration ?? 4000,
    })
  },
}

export type { ToastOptions }
export { toast }
