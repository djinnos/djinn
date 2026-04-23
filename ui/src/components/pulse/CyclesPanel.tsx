import { useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { Card, CardHeader, CardTitle, CardDescription, CardContent } from "@/components/ui/card";
import { Button } from "@/components/ui/button";
import { Skeleton } from "@/components/ui/skeleton";
import { callMcpTool } from "@/api/mcpClient";
import { showToast } from "@/lib/toast";
import { parseCycles, truncatePathLeft } from "./pulseTypes";

interface CyclesPanelProps {
  projectPath: string;
}

type CyclesKind = "file" | "symbol";

/**
 * Upper bound on how many members a single cycle card renders before it
 * collapses into a "+N more" affordance.  Real file-import cycles are
 * small (2-5 files); anything bigger is almost always an artefact of
 * over-connected helpers or generated code, and rendering the full list
 * blows up the DOM (observed: a 10k-member mixed SCC on the unfiltered
 * graph froze the page on 2026-04-08 before we defaulted to a
 * `kind_filter`).
 */
const MEMBERS_VISIBLE_PER_GROUP = 20;

function openMember(displayName: string) {
  void navigator.clipboard?.writeText(displayName);
  showToast.info("Copied path to clipboard", {
    description: "Open it in your editor to inspect the cycle member.",
  });
}

export function CyclesPanel({ projectPath }: CyclesPanelProps) {
  // File-level cycles catch circular imports between modules — the
  // actionable case.  Symbol-level cycles catch mutual recursion and
  // trait-impl loops.  We default to `file` because circular imports
  // are more common and easier to reason about; the toggle lets users
  // drill into symbol-level cycles if they want them.
  //
  // We deliberately never query with `kind_filter: undefined` — on a
  // real repo the unfiltered graph always contains one giant mixed
  // file↔symbol SCC (every file is strongly connected with every
  // symbol it declares via ContainsDefinition/DeclaredInFile pairs).
  // That component contains ~the whole graph and is pure noise.
  const [kind, setKind] = useState<CyclesKind>("file");
  const [expandedGroups, setExpandedGroups] = useState<Set<number>>(new Set());

  const { data, isLoading, error, refetch, isFetching } = useQuery({
    queryKey: ["pulse", "cycles", projectPath, kind],
    queryFn: async () => {
      const raw = await callMcpTool("code_graph", {
        project_path: projectPath,
        operation: "cycles",
        kind_filter: kind,
        min_size: 2,
      });
      return parseCycles(raw);
    },
    staleTime: 60_000,
  });

  // Backend (`code_graph` handler) already applies Tier 1 module-
  // artifact suppression and the project's `graph_excluded_paths` /
  // min_size filter — we just sort the cycles-by-size here so the
  // biggest SCCs render first.
  const filtered = (data ?? [])
    .slice()
    .sort((a, b) => b.members.length - a.members.length);

  const toggleGroup = (index: number) => {
    setExpandedGroups((prev) => {
      const next = new Set(prev);
      if (next.has(index)) {
        next.delete(index);
      } else {
        next.add(index);
      }
      return next;
    });
  };

  const KindToggle = (
    <div className="inline-flex rounded-md border border-border/60 bg-muted/20 p-0.5 text-xs">
      {(["file", "symbol"] as const).map((k) => (
        <button
          key={k}
          type="button"
          onClick={() => setKind(k)}
          className={
            k === kind
              ? "rounded-[3px] bg-foreground/10 px-2 py-0.5 text-foreground"
              : "rounded-[3px] px-2 py-0.5 text-muted-foreground hover:text-foreground"
          }
        >
          {k}
        </button>
      ))}
    </div>
  );

  return (
    <Card>
      <CardHeader>
        <div className="flex items-start justify-between gap-3">
          <div>
            <CardTitle>Cycles</CardTitle>
            <CardDescription>
              Strongly-connected groups in the dependency graph.
            </CardDescription>
          </div>
          {KindToggle}
        </div>
      </CardHeader>
      <CardContent>
        {isLoading ? (
          <div className="space-y-2">
            {Array.from({ length: 3 }).map((_, i) => (
              <Skeleton key={i} className="h-12 w-full" />
            ))}
          </div>
        ) : error ? (
          <div className="flex items-center justify-between gap-3 text-sm">
            <p className="text-muted-foreground">Couldn&apos;t load cycles.</p>
            <Button size="sm" variant="outline" onClick={() => refetch()} disabled={isFetching}>
              Retry
            </Button>
          </div>
        ) : filtered.length === 0 ? (
          <p className="text-sm text-emerald-400/80">
            No {kind} cycles found.
          </p>
        ) : (
          <div className="space-y-3">
            {filtered.map((group, i) => {
              const expanded = expandedGroups.has(i);
              const total = group.members.length;
              const hidden = Math.max(0, total - MEMBERS_VISIBLE_PER_GROUP);
              const visible = expanded
                ? group.members
                : group.members.slice(0, MEMBERS_VISIBLE_PER_GROUP);
              return (
                <div
                  key={`${i}-${group.members[0]?.key ?? ""}`}
                  className="rounded-lg border border-border/60 bg-muted/20 p-3"
                >
                  <div className="mb-2 flex items-center gap-2 text-xs font-medium text-muted-foreground">
                    <span className="rounded-full bg-amber-500/10 px-2 py-0.5 text-amber-400">
                      {total} members
                    </span>
                  </div>
                  <ul className="space-y-1">
                    {visible.map((m) => (
                      <li key={m.key}>
                        <button
                          type="button"
                          onClick={() => openMember(m.display_name || m.key)}
                          className="w-full truncate text-left font-mono text-xs text-foreground/80 hover:text-foreground"
                          title={m.display_name || m.key}
                          dir="rtl"
                        >
                          {truncatePathLeft(m.display_name || m.key)}
                        </button>
                      </li>
                    ))}
                  </ul>
                  {hidden > 0 && (
                    <button
                      type="button"
                      onClick={() => toggleGroup(i)}
                      className="mt-2 text-xs text-muted-foreground hover:text-foreground"
                    >
                      {expanded ? "Show fewer" : `+${hidden} more members`}
                    </button>
                  )}
                </div>
              );
            })}
          </div>
        )}
      </CardContent>
    </Card>
  );
}
