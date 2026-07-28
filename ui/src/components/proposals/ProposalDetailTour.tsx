import {
  useCallback,
  useEffect,
  useRef,
  useState,
  type CSSProperties,
  type KeyboardEvent as ReactKeyboardEvent,
} from "react";
import { createPortal } from "react-dom";
import {
  ArrowLeft01Icon,
  ArrowRight01Icon,
  HelpCircleIcon,
  SearchFocusIcon,
} from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";

import { Button } from "@/components/ui/button";
import { proposalDetailTourStorageKey } from "./proposalTourStorage";

const TOUR_STEPS = [
  {
    target: "overview",
    title: "The proposal brief",
    description:
      "This is the source of truth: the outcome, repository targets, constraints, and proof required before agents can execute work.",
  },
  {
    target: "spec",
    title: "Scope and validation",
    description:
      "The spec makes the work reviewable. Refinement strengthens weak assumptions, missing dependencies, risks, and validation before graduation.",
  },
  {
    target: "refinement",
    title: "Automatic refinement",
    description:
      "Right after creation, Advocate, Adversary, and Judge challenge the brief autonomously. The tribunal stops for your review—it does not change code.",
  },
  {
    target: "readiness",
    title: "The readiness gate",
    description:
      "Every blocked row explains why this proposal cannot graduate yet. Refinement and human review must clear each requirement.",
  },
  {
    target: "approval",
    title: "Approve, then graduate",
    description:
      "Product and engineering sign off on the refined brief. Only after approval can graduation create executable epics and tasks for agents.",
  },
] as const;

interface HighlightRect {
  top: number;
  left: number;
  width: number;
  height: number;
  bottom: number;
}

function hasSeenTour(userId: string | null): boolean {
  if (!userId || typeof window === "undefined") return true;
  try {
    return (
      window.localStorage.getItem(proposalDetailTourStorageKey(userId)) ===
      "seen"
    );
  } catch {
    return false;
  }
}

function clamp(value: number, min: number, max: number): number {
  return Math.min(Math.max(value, min), Math.max(min, max));
}

export function ProposalDetailTour({
  userId,
  forceOpen = false,
}: {
  userId: string | null;
  forceOpen?: boolean;
}) {
  const [open, setOpen] = useState(
    () => forceOpen || (Boolean(userId) && !hasSeenTour(userId)),
  );
  const [stepIndex, setStepIndex] = useState(0);
  const [highlight, setHighlight] = useState<HighlightRect | null>(null);
  const dialogRef = useRef<HTMLDivElement>(null);
  const replayButtonRef = useRef<HTMLButtonElement>(null);
  const measureFrameRef = useRef<number | null>(null);
  const measureTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);

  const finish = useCallback(() => {
    if (userId) {
      try {
        window.localStorage.setItem(
          proposalDetailTourStorageKey(userId),
          "seen",
        );
      } catch {
        // Storage can be unavailable in strict privacy modes. The tour still
        // closes correctly; it may simply reappear on a later visit.
      }
    }
    setOpen(false);
    requestAnimationFrame(() => replayButtonRef.current?.focus());
  }, [userId]);

  const updateHighlight = useCallback(() => {
    if (!open) return;
    const step = TOUR_STEPS[stepIndex];
    const target = document.querySelector<HTMLElement>(
      `[data-proposal-tour="${step.target}"]`,
    );

    if (!target) {
      setHighlight(null);
      return;
    }

    const rect = target.getBoundingClientRect();
    const padding = 8;
    const top = clamp(rect.top - padding, 8, window.innerHeight - 24);
    const left = clamp(rect.left - padding, 8, window.innerWidth - 24);
    const right = clamp(rect.right + padding, left + 16, window.innerWidth - 8);
    const bottom = clamp(
      rect.bottom + padding,
      top + 16,
      window.innerHeight - 8,
    );
    setHighlight({
      top,
      left,
      width: right - left,
      height: bottom - top,
      bottom,
    });
  }, [open, stepIndex]);

  const moveToTarget = useCallback(() => {
    if (!open) return;
    const step = TOUR_STEPS[stepIndex];
    const target = document.querySelector<HTMLElement>(
      `[data-proposal-tour="${step.target}"]`,
    );
    if (!target) {
      setHighlight(null);
      return;
    }
    const prefersReducedMotion = window.matchMedia?.(
      "(prefers-reduced-motion: reduce)",
    ).matches;
    target.scrollIntoView({
      behavior: prefersReducedMotion ? "auto" : "smooth",
      block: "center",
    });
    if (measureFrameRef.current !== null) {
      cancelAnimationFrame(measureFrameRef.current);
    }
    if (measureTimerRef.current !== null) {
      clearTimeout(measureTimerRef.current);
    }
    measureFrameRef.current = requestAnimationFrame(updateHighlight);
    measureTimerRef.current = setTimeout(
      updateHighlight,
      prefersReducedMotion ? 0 : 350,
    );
  }, [open, stepIndex, updateHighlight]);

  useEffect(() => {
    if (!open) return undefined;
    const initialFrame = requestAnimationFrame(moveToTarget);

    const handleViewportChange = () => {
      if (measureFrameRef.current !== null) {
        cancelAnimationFrame(measureFrameRef.current);
      }
      measureFrameRef.current = requestAnimationFrame(updateHighlight);
    };
    window.addEventListener("resize", handleViewportChange);
    window.addEventListener("scroll", handleViewportChange, true);

    return () => {
      window.removeEventListener("resize", handleViewportChange);
      window.removeEventListener("scroll", handleViewportChange, true);
      cancelAnimationFrame(initialFrame);
      if (measureFrameRef.current !== null) {
        cancelAnimationFrame(measureFrameRef.current);
      }
      if (measureTimerRef.current !== null) {
        clearTimeout(measureTimerRef.current);
      }
    };
  }, [moveToTarget, open, updateHighlight]);

  useEffect(() => {
    if (!open) return undefined;
    const focusFrame = requestAnimationFrame(() => dialogRef.current?.focus());
    const handleKeyDown = (event: KeyboardEvent) => {
      if (event.key === "Escape") {
        event.preventDefault();
        finish();
        return;
      }
      if (event.key !== "Tab" || !dialogRef.current) return;
      const focusable = Array.from(
        dialogRef.current.querySelectorAll<HTMLButtonElement>(
          "button:not(:disabled)",
        ),
      );
      if (focusable.length === 0) return;
      const first = focusable[0];
      const last = focusable[focusable.length - 1];
      if (
        event.shiftKey &&
        (document.activeElement === first ||
          document.activeElement === dialogRef.current)
      ) {
        event.preventDefault();
        last?.focus();
      } else if (!event.shiftKey && document.activeElement === last) {
        event.preventDefault();
        first?.focus();
      }
    };
    document.addEventListener("keydown", handleKeyDown);
    return () => {
      cancelAnimationFrame(focusFrame);
      document.removeEventListener("keydown", handleKeyDown);
    };
  }, [finish, open]);

  const replay = () => {
    setStepIndex(0);
    setOpen(true);
  };

  const handleDialogKeyDown = (event: ReactKeyboardEvent) => {
    if (event.key === "ArrowRight" && stepIndex < TOUR_STEPS.length - 1) {
      setStepIndex((current) => current + 1);
    }
    if (event.key === "ArrowLeft" && stepIndex > 0) {
      setStepIndex((current) => current - 1);
    }
  };

  const step = TOUR_STEPS[stepIndex];
  const tooltipWidth =
    typeof window === "undefined" ? 360 : Math.min(360, window.innerWidth - 32);
  const tooltipLeft = highlight
    ? clamp(highlight.left, 16, window.innerWidth - tooltipWidth - 16)
    : 16;
  const hasRoomBelow = highlight
    ? window.innerHeight - highlight.bottom >= 250
    : false;
  const tooltipStyle: CSSProperties = highlight
    ? {
        width: tooltipWidth,
        left: tooltipLeft,
        top: hasRoomBelow
          ? highlight.bottom + 14
          : Math.max(16, highlight.top - 250),
      }
    : {
        width: tooltipWidth,
        left: "50%",
        top: "50%",
        transform: "translate(-50%, -50%)",
      };

  return (
    <>
      <Button
        ref={replayButtonRef}
        type="button"
        variant="outline"
        size="sm"
        onClick={replay}
        aria-label="Tour this proposal page"
      >
        <HugeiconsIcon icon={HelpCircleIcon} size={15} />
        Tour
      </Button>

      {open &&
        createPortal(
          <div className="pointer-events-none fixed inset-0 z-[100]">
            <div
              className="pointer-events-auto fixed inset-0"
              aria-hidden="true"
            />
            {highlight && (
              <div
                className="fixed rounded-xl ring-2 ring-primary ring-offset-4 ring-offset-background"
                style={{
                  top: highlight.top,
                  left: highlight.left,
                  width: highlight.width,
                  height: highlight.height,
                  boxShadow: "0 0 0 9999px rgb(0 0 0 / 0.76)",
                }}
                aria-hidden="true"
              />
            )}
            {!highlight && (
              <div
                className="fixed inset-0 bg-black/75"
                aria-hidden="true"
              />
            )}

            <div
              ref={dialogRef}
              role="dialog"
              aria-modal="true"
              aria-labelledby="proposal-tour-title"
              aria-describedby="proposal-tour-description"
              tabIndex={-1}
              onKeyDown={handleDialogKeyDown}
              className="pointer-events-auto fixed rounded-xl border border-primary/40 bg-popover p-4 text-popover-foreground shadow-2xl outline-none ring-1 ring-black/20"
              style={tooltipStyle}
            >
              <div className="flex items-start gap-3">
                <div className="flex h-9 w-9 shrink-0 items-center justify-center rounded-lg bg-primary/15 text-primary">
                  <HugeiconsIcon icon={SearchFocusIcon} size={19} />
                </div>
                <div className="min-w-0 flex-1">
                  <p className="text-[11px] font-semibold uppercase tracking-wide text-primary">
                    Guided tour · {stepIndex + 1} of {TOUR_STEPS.length}
                  </p>
                  <h2
                    id="proposal-tour-title"
                    className="mt-1 text-base font-semibold"
                  >
                    {step.title}
                  </h2>
                </div>
              </div>
              <p
                id="proposal-tour-description"
                className="mt-3 text-sm leading-relaxed text-muted-foreground"
              >
                {step.description}
              </p>

              <div className="mt-4 flex items-center justify-between gap-3">
                <Button type="button" variant="ghost" size="sm" onClick={finish}>
                  Skip tour
                </Button>
                <div className="flex items-center gap-2">
                  {stepIndex > 0 && (
                    <Button
                      type="button"
                      variant="outline"
                      size="sm"
                      onClick={() => setStepIndex((current) => current - 1)}
                    >
                      <HugeiconsIcon icon={ArrowLeft01Icon} size={14} />
                      Back
                    </Button>
                  )}
                  <Button
                    type="button"
                    size="sm"
                    onClick={() => {
                      if (stepIndex === TOUR_STEPS.length - 1) {
                        finish();
                      } else {
                        setStepIndex((current) => current + 1);
                      }
                    }}
                  >
                    {stepIndex === TOUR_STEPS.length - 1 ? "Finish" : "Next"}
                    {stepIndex < TOUR_STEPS.length - 1 && (
                      <HugeiconsIcon icon={ArrowRight01Icon} size={14} />
                    )}
                  </Button>
                </div>
              </div>
            </div>
          </div>,
          document.body,
        )}
    </>
  );
}
