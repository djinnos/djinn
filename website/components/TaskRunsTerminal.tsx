"use client";

import { useEffect, useRef, useState, useSyncExternalStore } from "react";

const COMMAND = "kubectl get jobs -n djinn";

/* Job names are equal length and the status is padded, so the columns line up
   under `whitespace-pre` without a table. */
const RUNS = [
  { job: "djinn-taskrun-9f2c", status: "Running", work: "api: add usage rollup" },
  { job: "djinn-taskrun-c41a", status: "Running", work: "ui: spend by proposal" },
  { job: "djinn-taskrun-77b0", status: "Complete", work: "db: attribution schema" },
];

const TYPE_MS = 45;
const ROW_MS = 240;
const REDUCED_MOTION = "(prefers-reduced-motion: reduce)";

function subscribeMotion(onChange: () => void) {
  const query = window.matchMedia(REDUCED_MOTION);
  query.addEventListener("change", onChange);
  return () => query.removeEventListener("change", onChange);
}

const readMotion = () => window.matchMedia(REDUCED_MOTION).matches;

/* Types the command out and prints the rows once, when the block scrolls into
   view — then stops. A loop here would pull attention away from the copy for
   as long as the section is on screen. */
export default function TaskRunsTerminal() {
  const ref = useRef<HTMLDivElement>(null);
  const [typed, setTyped] = useState(0);
  const [rows, setRows] = useState(0);
  const [running, setRunning] = useState(false);

  const reduced = useSyncExternalStore(subscribeMotion, readMotion, () => false);

  // Reduced motion renders the finished state rather than animating to it.
  const shownTyped = reduced ? COMMAND.length : typed;
  const shownRows = reduced ? RUNS.length : rows;

  useEffect(() => {
    const node = ref.current;
    if (!node || reduced) return;

    const timers: number[] = [];

    const play = () => {
      setRunning(true);
      for (let i = 1; i <= COMMAND.length; i++) {
        timers.push(window.setTimeout(() => setTyped(i), TYPE_MS * i));
      }
      const after = TYPE_MS * COMMAND.length + 280;
      for (let i = 1; i <= RUNS.length; i++) {
        timers.push(window.setTimeout(() => setRows(i), after + ROW_MS * (i - 1)));
      }
      timers.push(
        window.setTimeout(() => setRunning(false), after + ROW_MS * RUNS.length),
      );
    };

    const observer = new IntersectionObserver(
      (entries) => {
        if (entries.some((entry) => entry.isIntersecting)) {
          observer.disconnect();
          play();
        }
      },
      { threshold: 0.4 },
    );
    observer.observe(node);

    return () => {
      observer.disconnect();
      timers.forEach(clearTimeout);
    };
  }, [reduced]);

  return (
    <div ref={ref} className="window">
      <div className="window-bar">task-runs · namespace: djinn</div>
      <div className="p-4 font-mono text-xs leading-relaxed overflow-x-auto">
        <div className="whitespace-pre">
          <span className="text-text-muted">{"$ "}</span>
          <span className="text-text-primary">{COMMAND.slice(0, shownTyped)}</span>
          {running && !reduced && (
            <span className="inline-block w-[0.5em] h-[1em] -mb-[0.15em] bg-text-primary animate-pulse" />
          )}
        </div>
        {/* Rows are always in the DOM so the block never changes height. */}
        {RUNS.map((run, i) => (
          <div
            key={run.job}
            className={`whitespace-pre transition-opacity duration-300 ${
              i < shownRows ? "opacity-100" : "opacity-0"
            }`}
          >
            <span className="text-text-secondary">{`  ${run.job}   `}</span>
            <span
              className={
                run.status === "Complete" ? "text-status-pass" : "text-status-warn"
              }
            >
              {run.status.padEnd(10)}
            </span>
            <span className="text-text-secondary">{run.work}</span>
          </div>
        ))}
      </div>
    </div>
  );
}
