import { lineDiff } from "@/lib/lineDiff";

/** Unified git-style red/green line diff between two text snapshots. */
export function DiffView({ before, after }: { before: string; after: string }) {
  const ops = lineDiff(before, after);
  if (ops.every((o) => o.type === "context")) {
    return <p className="text-sm text-muted-foreground">No textual changes.</p>;
  }
  return (
    <pre className="overflow-x-auto rounded-md border bg-muted/30 p-2 font-mono text-xs leading-relaxed">
      {ops.map((op, i) => {
        const sign = op.type === "add" ? "+" : op.type === "del" ? "-" : " ";
        const cls =
          op.type === "add"
            ? "bg-green-500/15 text-green-700 dark:text-green-300"
            : op.type === "del"
              ? "bg-red-500/15 text-red-700 dark:text-red-300"
              : "text-muted-foreground";
        return (
          <div key={i} className={cls}>
            <span className="select-none opacity-60">{sign} </span>
            {op.value || " "}
          </div>
        );
      })}
    </pre>
  );
}
