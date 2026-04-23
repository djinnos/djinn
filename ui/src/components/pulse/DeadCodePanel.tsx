import { useMemo, useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { HugeiconsIcon } from "@hugeicons/react";
import { ArrowDown01Icon, ArrowRight01Icon } from "@hugeicons/core-free-icons";
import { Card, CardHeader, CardTitle, CardDescription, CardContent } from "@/components/ui/card";
import { Button } from "@/components/ui/button";
import { Badge } from "@/components/ui/badge";
import { Skeleton } from "@/components/ui/skeleton";
import { callMcpTool } from "@/api/mcpClient";
import { cn } from "@/lib/utils";
import {
  parseOrphans,
  truncatePathLeft,
  type OrphanEntry,
} from "./pulseTypes";

interface DeadCodePanelProps {
  projectPath: string;
  onIgnoreFile: (filePath: string) => void;
}

type Confidence = "high" | "medium" | "low";

interface DeadFile {
  file: string;
  confidence: Confidence;
  orphans: OrphanEntry[];
  publicCount: number;
}

const LOW_CONFIDENCE_PATTERNS = [
  "build.rs",
  "/ffi/",
  "/bindings/",
  "_generated.",
  "/generated/",
  ".gen.",
];

function classifyFile(file: string, orphans: OrphanEntry[]): Confidence {
  if (LOW_CONFIDENCE_PATTERNS.some((p) => file.includes(p))) return "low";
  const publicCount = orphans.filter((o) => o.visibility === "public").length;
  if (publicCount > 0) return "medium";
  return "high";
}

function groupByFile(entries: OrphanEntry[]): DeadFile[] {
  const map = new Map<string, OrphanEntry[]>();
  for (const e of entries) {
    if (!e.file) continue;
    const list = map.get(e.file) ?? [];
    list.push(e);
    map.set(e.file, list);
  }
  const result: DeadFile[] = [];
  for (const [file, orphans] of map.entries()) {
    result.push({
      file,
      orphans,
      publicCount: orphans.filter((o) => o.visibility === "public").length,
      confidence: classifyFile(file, orphans),
    });
  }
  // Sort within each section by orphan count desc
  result.sort((a, b) => b.orphans.length - a.orphans.length);
  return result;
}

const CONFIDENCE_LABEL: Record<Confidence, string> = {
  high: "High",
  medium: "Medium",
  low: "Low",
};

function ConfidenceBadge({ level }: { level: Confidence }) {
  const variant = level === "high" ? "destructive" : level === "medium" ? "secondary" : "outline";
  return (
    <Badge variant={variant} className="text-[10px]">
      {CONFIDENCE_LABEL[level]}
    </Badge>
  );
}

function DeadFileRow({
  file,
  expanded,
  onToggle,
  onIgnore,
}: {
  file: DeadFile;
  expanded: boolean;
  onToggle: () => void;
  onIgnore: () => void;
}) {
  return (
    <div className="rounded-lg px-2 py-2 transition-colors hover:bg-muted/40">
      <button
        type="button"
        onClick={onToggle}
        className="flex w-full items-center gap-3 text-left"
      >
        <HugeiconsIcon
          icon={expanded ? ArrowDown01Icon : ArrowRight01Icon}
          className="h-3 w-3 shrink-0 text-muted-foreground"
        />
        <span
          className="min-w-0 flex-1 truncate font-mono text-xs text-foreground"
          title={file.file}
          dir="rtl"
        >
          {truncatePathLeft(file.file)}
        </span>
        <ConfidenceBadge level={file.confidence} />
        <span className="shrink-0 text-xs text-muted-foreground">
          {file.orphans.length} symbol{file.orphans.length === 1 ? "" : "s"}
        </span>
      </button>
      {expanded && (
        <div className="mt-3 space-y-2 pl-6">
          <div className="space-y-1">
            <p className="text-[11px] font-medium text-muted-foreground">
              Orphan symbols
            </p>
            <ul className="space-y-0.5">
              {file.orphans.slice(0, 30).map((o) => (
                <li
                  key={o.key}
                  className="flex items-baseline justify-between gap-2 text-xs"
                >
                  <span className="truncate font-mono text-foreground/80">
                    {o.display_name || o.key}
                  </span>
                  <span className="shrink-0 text-[10px] text-muted-foreground">
                    {o.kind}
                    {o.visibility !== "unknown" && ` · ${o.visibility}`}
                  </span>
                </li>
              ))}
              {file.orphans.length > 30 && (
                <li className="text-[11px] text-muted-foreground">
                  …and {file.orphans.length - 30} more
                </li>
              )}
            </ul>
          </div>
          <div className="flex justify-end pt-1">
            <Button
              size="xs"
              variant="outline"
              onClick={(e) => {
                e.stopPropagation();
                onIgnore();
              }}
            >
              Ignore this file
            </Button>
          </div>
        </div>
      )}
    </div>
  );
}

function ConfidenceSection({
  title,
  files,
  defaultOpen,
  expandedFile,
  onToggleFile,
  onIgnoreFile,
}: {
  title: string;
  files: DeadFile[];
  defaultOpen: boolean;
  expandedFile: string | null;
  onToggleFile: (file: string) => void;
  onIgnoreFile: (file: string) => void;
}) {
  const [open, setOpen] = useState(defaultOpen);
  if (files.length === 0) return null;
  return (
    <div className="space-y-1">
      <button
        type="button"
        onClick={() => setOpen((v) => !v)}
        className="flex w-full items-center gap-2 rounded-md px-2 py-1 text-xs font-medium text-muted-foreground hover:bg-muted/40"
      >
        <HugeiconsIcon
          icon={open ? ArrowDown01Icon : ArrowRight01Icon}
          className="h-3 w-3"
        />
        <span>{title}</span>
        <span className="text-[10px]">({files.length})</span>
      </button>
      {open && (
        <div className="space-y-0.5">
          {files.map((f) => (
            <DeadFileRow
              key={f.file}
              file={f}
              expanded={expandedFile === f.file}
              onToggle={() => onToggleFile(f.file)}
              onIgnore={() => onIgnoreFile(f.file)}
            />
          ))}
        </div>
      )}
    </div>
  );
}

export function DeadCodePanel({
  projectPath,
  onIgnoreFile,
}: DeadCodePanelProps) {
  const [expandedFile, setExpandedFile] = useState<string | null>(null);

  const { data, isLoading, error, refetch, isFetching } = useQuery({
    queryKey: ["pulse", "orphans", projectPath],
    queryFn: async () => {
      const raw = await callMcpTool("code_graph", {
        project_path: projectPath,
        operation: "orphans",
        kind_filter: "symbol",
        limit: 500,
      });
      return parseOrphans(raw);
    },
    staleTime: 60_000,
  });

  const grouped = useMemo(() => {
    if (!data) return [] as DeadFile[];
    // Backend (`code_graph orphans`) already applies the project's
    // `graph_excluded_paths` globs and `graph_orphan_ignore` exact-
    // match list; we only need the `!o.file` guard, which is about
    // shape rather than exclusion.
    const visible = data.filter((o) => !!o.file);
    return groupByFile(visible);
  }, [data]);

  const high = grouped.filter((f) => f.confidence === "high");
  const medium = grouped.filter((f) => f.confidence === "medium");
  const low = grouped.filter((f) => f.confidence === "low");

  const toggleFile = (file: string) =>
    setExpandedFile((prev) => (prev === file ? null : file));

  return (
    <Card>
      <CardHeader>
        <CardTitle>Dead code</CardTitle>
        <CardDescription>
          Files and symbols with no incoming references. Verify before deleting.
        </CardDescription>
      </CardHeader>
      <CardContent>
        {isLoading ? (
          <div className="space-y-2">
            {Array.from({ length: 5 }).map((_, i) => (
              <Skeleton key={i} className="h-6 w-full" />
            ))}
          </div>
        ) : error ? (
          <div className="flex items-center justify-between gap-3 text-sm">
            <p className="text-muted-foreground">Couldn&apos;t load dead code.</p>
            <Button size="sm" variant="outline" onClick={() => refetch()} disabled={isFetching}>
              Retry
            </Button>
          </div>
        ) : grouped.length === 0 ? (
          <p className="text-sm text-muted-foreground">No dead code detected. Nice.</p>
        ) : (
          <div className={cn("space-y-2")}>
            <ConfidenceSection
              title="High confidence"
              files={high}
              defaultOpen
              expandedFile={expandedFile}
              onToggleFile={toggleFile}
              onIgnoreFile={onIgnoreFile}
            />
            <ConfidenceSection
              title="Medium confidence"
              files={medium}
              defaultOpen={high.length === 0}
              expandedFile={expandedFile}
              onToggleFile={toggleFile}
              onIgnoreFile={onIgnoreFile}
            />
            <ConfidenceSection
              title="Low confidence"
              files={low}
              defaultOpen={false}
              expandedFile={expandedFile}
              onToggleFile={toggleFile}
              onIgnoreFile={onIgnoreFile}
            />
          </div>
        )}
      </CardContent>
    </Card>
  );
}
