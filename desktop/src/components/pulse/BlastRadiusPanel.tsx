import { useEffect, useMemo, useRef, useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { Card, CardHeader, CardTitle, CardDescription, CardContent } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { Skeleton } from "@/components/ui/skeleton";
import { callMcpTool } from "@/api/mcpClient";
import { cn } from "@/lib/utils";
import {
  parseSearchHits,
  parseFileGroups,
  truncatePathLeft,
  isPathExcluded,
  type SearchHit,
} from "./pulseTypes";

interface BlastRadiusPanelProps {
  projectPath: string;
  excludedPaths: string[];
}

const DEPTH_LABELS = ["Direct", "Near", "Reach", "Distant", "Full"];

function useDebounced<T>(value: T, ms: number): T {
  const [debounced, setDebounced] = useState(value);
  useEffect(() => {
    const id = setTimeout(() => setDebounced(value), ms);
    return () => clearTimeout(id);
  }, [value, ms]);
  return debounced;
}

export function BlastRadiusPanel({ projectPath, excludedPaths }: BlastRadiusPanelProps) {
  const [query, setQuery] = useState("");
  const [selected, setSelected] = useState<SearchHit | null>(null);
  const [depth, setDepth] = useState(3);
  const [showDropdown, setShowDropdown] = useState(false);
  const containerRef = useRef<HTMLDivElement>(null);

  const debouncedQuery = useDebounced(query, 200);

  const { data: searchData, isLoading: searchLoading } = useQuery({
    queryKey: ["pulse", "search", projectPath, debouncedQuery],
    queryFn: async () => {
      const raw = await callMcpTool("code_graph", {
        project_path: projectPath,
        operation: "search",
        query: debouncedQuery,
        limit: 10,
      });
      return parseSearchHits(raw);
    },
    enabled: debouncedQuery.trim().length >= 2,
    staleTime: 30_000,
  });

  const { data: impactData, isLoading: impactLoading, error: impactError } = useQuery({
    queryKey: ["pulse", "impact", projectPath, selected?.key, depth],
    queryFn: async () => {
      if (!selected) return [];
      const raw = await callMcpTool("code_graph", {
        project_path: projectPath,
        operation: "impact",
        key: selected.key,
        group_by: "file",
        limit: depth,
      });
      return parseFileGroups(raw);
    },
    enabled: !!selected,
    staleTime: 30_000,
  });

  const filteredImpact = useMemo(() => {
    return (impactData ?? [])
      .filter((g) => !isPathExcluded(g.file, excludedPaths))
      .sort((a, b) => b.occurrence_count - a.occurrence_count);
  }, [impactData, excludedPaths]);

  // Close dropdown on outside click
  useEffect(() => {
    function onClick(e: MouseEvent) {
      if (containerRef.current && !containerRef.current.contains(e.target as Node)) {
        setShowDropdown(false);
      }
    }
    document.addEventListener("mousedown", onClick);
    return () => document.removeEventListener("mousedown", onClick);
  }, []);

  const filteredSearch = (searchData ?? []).filter(
    (h) => !isPathExcluded(h.file ?? h.display_name, excludedPaths)
  );

  return (
    <Card>
      <CardHeader>
        <CardTitle>Blast radius</CardTitle>
        <CardDescription>
          Type a file or symbol to see what depends on it.
        </CardDescription>
      </CardHeader>
      <CardContent>
        <div ref={containerRef} className="space-y-3">
          <div className="flex items-center gap-3">
            <div className="relative flex-1">
              <Input
                placeholder="Search files and symbols…"
                value={query}
                onChange={(e) => {
                  setQuery(e.target.value);
                  setShowDropdown(true);
                }}
                onFocus={() => setShowDropdown(true)}
              />
              {showDropdown && debouncedQuery.trim().length >= 2 && (
                <div className="absolute top-full left-0 right-0 z-10 mt-1 max-h-72 overflow-y-auto rounded-lg border border-border bg-popover shadow-lg ring-1 ring-foreground/10">
                  {searchLoading ? (
                    <div className="p-3 text-xs text-muted-foreground">Searching…</div>
                  ) : filteredSearch.length === 0 ? (
                    <div className="p-3 text-xs text-muted-foreground">No matches.</div>
                  ) : (
                    <ul>
                      {filteredSearch.map((hit) => (
                        <li key={hit.key}>
                          <button
                            type="button"
                            onClick={() => {
                              setSelected(hit);
                              setQuery(hit.display_name);
                              setShowDropdown(false);
                            }}
                            className="flex w-full flex-col items-start gap-0.5 px-3 py-2 text-left hover:bg-muted/50"
                          >
                            <span className="truncate text-xs font-medium text-foreground">
                              {hit.display_name}
                            </span>
                            {hit.file && (
                              <span className="truncate font-mono text-[10px] text-muted-foreground">
                                {truncatePathLeft(hit.file, 70)}
                              </span>
                            )}
                          </button>
                        </li>
                      ))}
                    </ul>
                  )}
                </div>
              )}
            </div>
            <div className="flex shrink-0 items-center gap-2">
              <input
                type="range"
                min={1}
                max={5}
                value={depth}
                onChange={(e) => setDepth(Number(e.target.value))}
                className="h-1 w-24 cursor-pointer appearance-none rounded-full bg-muted accent-emerald-400"
                aria-label="Impact depth"
              />
              <span className="w-12 text-xs tabular-nums text-muted-foreground">
                {DEPTH_LABELS[depth - 1]}
              </span>
            </div>
          </div>

          <div className="min-h-[120px]">
            {!selected ? (
              <p className="py-6 text-center text-xs text-muted-foreground/70">
                Search for a file or symbol to start.
              </p>
            ) : impactLoading ? (
              <div className="space-y-2">
                {Array.from({ length: 4 }).map((_, i) => (
                  <Skeleton key={i} className="h-6 w-full" />
                ))}
              </div>
            ) : impactError ? (
              <p className="text-sm text-muted-foreground">Couldn&apos;t load impact.</p>
            ) : filteredImpact.length === 0 ? (
              <p className="text-sm text-muted-foreground">
                Nothing depends on this in the current graph.
              </p>
            ) : (
              <ImpactTree groups={filteredImpact} />
            )}
          </div>
        </div>
      </CardContent>
    </Card>
  );
}

function ImpactTree({
  groups,
}: {
  groups: { file: string; occurrence_count: number; sample_keys: string[] }[];
}) {
  const [expanded, setExpanded] = useState<string | null>(null);
  return (
    <ul className="space-y-1">
      {groups.map((g) => {
        const isOpen = expanded === g.file;
        return (
          <li key={g.file}>
            <button
              type="button"
              onClick={() =>
                setExpanded((prev) => (prev === g.file ? null : g.file))
              }
              className={cn(
                "flex w-full items-center justify-between gap-3 rounded-md px-2 py-1.5 text-left text-xs hover:bg-muted/40",
                isOpen && "bg-muted/30"
              )}
            >
              <span
                className="min-w-0 flex-1 truncate font-mono text-foreground/80"
                title={g.file}
                dir="rtl"
              >
                {truncatePathLeft(g.file)}
              </span>
              <span className="shrink-0 text-muted-foreground">
                {g.occurrence_count} caller{g.occurrence_count === 1 ? "" : "s"}
              </span>
            </button>
            {isOpen && g.sample_keys.length > 0 && (
              <ul className="ml-4 mt-1 space-y-0.5 border-l border-border pl-3">
                {g.sample_keys.slice(0, 8).map((k) => (
                  <li
                    key={k}
                    className="truncate font-mono text-[11px] text-muted-foreground"
                    title={k}
                  >
                    {k}
                  </li>
                ))}
              </ul>
            )}
          </li>
        );
      })}
    </ul>
  );
}
