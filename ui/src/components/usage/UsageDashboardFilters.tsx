import { useMemo } from "react";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import type {
  DateRangePreset,
  Granularity,
  UsageAnalyticsFilters,
  UsageAnalyticsResponse,
} from "@/api/analytics";

/**
 * Sentinel value used inside selects to represent "no filter" (i.e. the
 * corresponding filter field should be `undefined`).  Kept distinct from any
 * real id/model so accidental collisions are impossible.
 */
const ALL_VALUE = "__all__";

/** Internal date-range mode that adds "custom" on top of the API presets. */
const CUSTOM_VALUE = "custom";

const DATE_PRESETS: { value: DateRangePreset; label: string }[] = [
  { value: "7d", label: "Last 7 days" },
  { value: "30d", label: "Last 30 days" },
];

const GRANULARITIES: { value: Granularity; label: string }[] = [
  { value: "day", label: "Daily" },
  { value: "week", label: "Weekly" },
  { value: "month", label: "Monthly" },
];

/** Known agent base roles — mirrors `BaseRole` from `@/api/agents`. */
const AGENT_TYPES: { value: string; label: string }[] = [
  { value: "worker", label: "Worker" },
  { value: "reviewer", label: "Reviewer" },
  { value: "lead", label: "Lead" },
  { value: "planner", label: "Planner" },
];

interface ProjectOption {
  id: string;
  name: string;
}

interface UsageDashboardFiltersBarProps {
  /** The canonical filter object shared by every dashboard tab. */
  filters: UsageAnalyticsFilters;
  /** Replace the entire filter object. */
  onChange: (next: UsageAnalyticsFilters) => void;
  /** Latest analytics response — used to source selector option lists. */
  data?: UsageAnalyticsResponse;
}

/**
 * Global filter bar for the usage analytics dashboard.
 *
 * All controls mutate a single canonical `UsageAnalyticsFilters` object via
 * `onChange`, which the owning page passes straight into the React Query
 * option.  No per-tab filter state exists — every tab reads from the same
 * fetched response.
 */
export function UsageDashboardFiltersBar({
  filters,
  onChange,
  data,
}: UsageDashboardFiltersBarProps) {
  // ── Derive selector options from the analytics response metadata ──────────
  // Source projects from breakdown rows and matrix cells so options stay
  // populated even when one section is sparse.  Resilient to empty data.
  const projectOptions = useMemo<ProjectOption[]>(() => {
    const seen = new Map<string, string>();
    for (const row of data?.breakdowns.by_project ?? []) {
      if (row.id) seen.set(row.id, row.name || row.id);
    }
    for (const cell of data?.project_model_matrix ?? []) {
      if (cell.project_id && !seen.has(cell.project_id)) {
        seen.set(cell.project_id, cell.project_name || cell.project_id);
      }
    }
    return Array.from(seen, ([id, name]) => ({ id, name }));
  }, [data]);

  const modelOptions = useMemo<string[]>(() => {
    const seen = new Set<string>();
    for (const m of data?.model_effectiveness ?? []) seen.add(m.model);
    for (const cell of data?.project_model_matrix ?? []) seen.add(cell.model);
    return Array.from(seen).sort();
  }, [data]);

  // ── Date range mode ───────────────────────────────────────────────────────
  const dateMode =
    filters.preset ?? (filters.start || filters.end ? CUSTOM_VALUE : "30d");

  const handleDateModeChange = (value: string) => {
    if (value === CUSTOM_VALUE) {
      // Switch to custom range; keep any previously entered dates.
      onChange({ ...filters, preset: undefined });
    } else {
      onChange({
        ...filters,
        preset: value as DateRangePreset,
        start: undefined,
        end: undefined,
      });
    }
  };

  // ── Single-select helpers ─────────────────────────────────────────────────
  const selectValue = (v: string | undefined) => v ?? ALL_VALUE;

  const handleSingleChange =
    (field: "project_id" | "model" | "agent_type") => (value: string) => {
      onChange({
        ...filters,
        [field]: value === ALL_VALUE ? undefined : value,
      });
    };

  return (
    <div className="flex flex-wrap items-end gap-3 rounded-lg border border-border bg-card px-4 py-3">
      {/* Date range preset / custom */}
      <div className="flex flex-col gap-1.5">
        <Label className="text-xs text-muted-foreground">Date range</Label>
        <Select
          value={dateMode}
          onValueChange={(v) =>
            typeof v === "string" && handleDateModeChange(v)
          }
        >
          <SelectTrigger className="h-8 w-[140px] text-xs" title="Date range">
            <SelectValue />
          </SelectTrigger>
          <SelectContent>
            {DATE_PRESETS.map((p) => (
              <SelectItem key={p.value} value={p.value}>
                {p.label}
              </SelectItem>
            ))}
            <SelectItem value={CUSTOM_VALUE}>Custom range</SelectItem>
          </SelectContent>
        </Select>
      </div>

      {/* Custom start / end — only visible in custom mode */}
      {dateMode === CUSTOM_VALUE && (
        <>
          <div className="flex flex-col gap-1.5">
            <Label
              htmlFor="usage-filter-start"
              className="text-xs text-muted-foreground"
            >
              Start
            </Label>
            <Input
              id="usage-filter-start"
              type="date"
              className="h-8 w-[150px] text-xs"
              value={filters.start?.slice(0, 10) ?? ""}
              onChange={(e) =>
                onChange({ ...filters, start: e.target.value || undefined })
              }
            />
          </div>
          <div className="flex flex-col gap-1.5">
            <Label
              htmlFor="usage-filter-end"
              className="text-xs text-muted-foreground"
            >
              End
            </Label>
            <Input
              id="usage-filter-end"
              type="date"
              className="h-8 w-[150px] text-xs"
              value={filters.end?.slice(0, 10) ?? ""}
              onChange={(e) =>
                onChange({ ...filters, end: e.target.value || undefined })
              }
            />
          </div>
        </>
      )}

      {/* Granularity */}
      <div className="flex flex-col gap-1.5">
        <Label className="text-xs text-muted-foreground">Granularity</Label>
        <Select
          value={selectValue(filters.granularity)}
          onValueChange={(v) => {
            if (typeof v === "string") {
              onChange({
                ...filters,
                granularity: v === ALL_VALUE ? undefined : (v as Granularity),
              });
            }
          }}
        >
          <SelectTrigger className="h-8 w-[120px] text-xs" title="Granularity">
            <SelectValue />
          </SelectTrigger>
          <SelectContent>
            {GRANULARITIES.map((g) => (
              <SelectItem key={g.value} value={g.value}>
                {g.label}
              </SelectItem>
            ))}
          </SelectContent>
        </Select>
      </div>

      {/* Project */}
      <div className="flex flex-col gap-1.5">
        <Label className="text-xs text-muted-foreground">Project</Label>
        <Select
          value={selectValue(filters.project_id)}
          onValueChange={(v) =>
            typeof v === "string" && handleSingleChange("project_id")(v)
          }
        >
          <SelectTrigger
            className="h-8 w-[160px] text-xs"
            title="Project filter"
          >
            <SelectValue />
          </SelectTrigger>
          <SelectContent>
            <SelectItem value={ALL_VALUE}>All projects</SelectItem>
            {projectOptions.map((p) => (
              <SelectItem key={p.id} value={p.id}>
                {p.name}
              </SelectItem>
            ))}
          </SelectContent>
        </Select>
      </div>

      {/* Model */}
      <div className="flex flex-col gap-1.5">
        <Label className="text-xs text-muted-foreground">Model</Label>
        <Select
          value={selectValue(filters.model)}
          onValueChange={(v) =>
            typeof v === "string" && handleSingleChange("model")(v)
          }
        >
          <SelectTrigger className="h-8 w-[180px] text-xs" title="Model filter">
            <SelectValue />
          </SelectTrigger>
          <SelectContent>
            <SelectItem value={ALL_VALUE}>All models</SelectItem>
            {modelOptions.map((m) => (
              <SelectItem key={m} value={m}>
                {m}
              </SelectItem>
            ))}
          </SelectContent>
        </Select>
      </div>

      {/* Agent type */}
      <div className="flex flex-col gap-1.5">
        <Label className="text-xs text-muted-foreground">Agent type</Label>
        <Select
          value={selectValue(filters.agent_type)}
          onValueChange={(v) =>
            typeof v === "string" && handleSingleChange("agent_type")(v)
          }
        >
          <SelectTrigger
            className="h-8 w-[130px] text-xs"
            title="Agent type filter"
          >
            <SelectValue />
          </SelectTrigger>
          <SelectContent>
            <SelectItem value={ALL_VALUE}>All types</SelectItem>
            {AGENT_TYPES.map((a) => (
              <SelectItem key={a.value} value={a.value}>
                {a.label}
              </SelectItem>
            ))}
          </SelectContent>
        </Select>
      </div>
    </div>
  );
}
