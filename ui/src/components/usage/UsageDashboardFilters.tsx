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
import { cn } from "@/lib/utils";
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

/** Option shape consumed by base-ui's `Select.items`, which makes
 * `<SelectValue />` render the option's human label instead of the raw value
 * (otherwise the trigger shows e.g. `__all__` / `30d` / `day`). */
interface SelectItemOption {
  value: string;
  label: string;
}

const DATE_ITEMS: SelectItemOption[] = [
  { value: "7d", label: "Last 7 days" },
  { value: "30d", label: "Last 30 days" },
  { value: CUSTOM_VALUE, label: "Custom range" },
];

const GRANULARITIES: SelectItemOption[] = [
  { value: "day", label: "Daily" },
  { value: "week", label: "Weekly" },
  { value: "month", label: "Monthly" },
];

/** Known agent base roles — mirrors `BaseRole` from `@/api/agents`. */
const AGENT_TYPES: SelectItemOption[] = [
  { value: "worker", label: "Worker" },
  { value: "reviewer", label: "Reviewer" },
  { value: "lead", label: "Lead" },
  { value: "planner", label: "Planner" },
  { value: "architect", label: "Architect" },
];

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
  const projectOptions = useMemo<SelectItemOption[]>(() => {
    const seen = new Map<string, string>();
    for (const row of data?.breakdowns.by_project ?? []) {
      if (row.id) seen.set(row.id, row.name || row.id);
    }
    for (const cell of data?.project_model_matrix ?? []) {
      if (cell.project_id && !seen.has(cell.project_id)) {
        seen.set(cell.project_id, cell.project_name || cell.project_id);
      }
    }
    return Array.from(seen, ([value, label]) => ({ value, label }));
  }, [data]);

  const modelOptions = useMemo<SelectItemOption[]>(() => {
    const seen = new Set<string>();
    for (const m of data?.model_effectiveness ?? []) seen.add(m.model);
    for (const cell of data?.project_model_matrix ?? []) seen.add(cell.model);
    return Array.from(seen)
      .sort()
      .map((value) => ({ value, label: value }));
  }, [data]);

  // Users are sourced from the by_user breakdown — the only place the backend
  // surfaces the user dimension. Falls back to the id when a name is missing.
  const userOptions = useMemo<SelectItemOption[]>(() => {
    const seen = new Map<string, string>();
    for (const row of data?.breakdowns.by_user ?? []) {
      if (row.id) seen.set(row.id, row.name || row.id);
    }
    return Array.from(seen, ([value, label]) => ({ value, label }));
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

  return (
    <div className="flex flex-wrap items-end gap-3 rounded-lg border border-border bg-card px-4 py-3">
      {/* Date range preset / custom */}
      <div className="flex flex-col gap-1.5">
        <Label className="text-xs text-muted-foreground">Date range</Label>
        <Select
          items={DATE_ITEMS}
          value={dateMode}
          onValueChange={(v) =>
            typeof v === "string" && handleDateModeChange(v)
          }
        >
          <SelectTrigger className="h-8 w-[140px] text-xs" title="Date range">
            <SelectValue />
          </SelectTrigger>
          <SelectContent>
            {DATE_ITEMS.map((p) => (
              <SelectItem key={p.value} value={p.value}>
                {p.label}
              </SelectItem>
            ))}
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

      {/* Granularity — always has a value, so no "all" option. */}
      <div className="flex flex-col gap-1.5">
        <Label className="text-xs text-muted-foreground">Granularity</Label>
        <Select
          items={GRANULARITIES}
          value={filters.granularity ?? "day"}
          onValueChange={(v) => {
            if (typeof v === "string") {
              onChange({ ...filters, granularity: v as Granularity });
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

      <FilterSelect
        label="Project"
        allLabel="All projects"
        title="Project filter"
        width="w-[160px]"
        options={projectOptions}
        value={filters.project_id}
        onSelect={(value) => onChange({ ...filters, project_id: value })}
      />

      <FilterSelect
        label="Model"
        allLabel="All models"
        title="Model filter"
        width="w-[180px]"
        options={modelOptions}
        value={filters.model}
        onSelect={(value) => onChange({ ...filters, model: value })}
      />

      <FilterSelect
        label="User"
        allLabel="All users"
        title="User filter"
        width="w-[160px]"
        options={userOptions}
        value={filters.user_id}
        onSelect={(value) => onChange({ ...filters, user_id: value })}
      />

      <FilterSelect
        label="Agent type"
        allLabel="All types"
        title="Agent type filter"
        width="w-[130px]"
        options={AGENT_TYPES}
        value={filters.agent_type}
        onSelect={(value) => onChange({ ...filters, agent_type: value })}
      />
    </div>
  );
}

/**
 * A single "all-or-one" filter dropdown. Prepends an "All …" sentinel option
 * and passes `items` to the Select so the trigger renders the chosen option's
 * label rather than the raw value (no stray `__all__` in the UI).
 */
function FilterSelect({
  label,
  allLabel,
  title,
  width,
  options,
  value,
  onSelect,
}: {
  label: string;
  allLabel: string;
  title: string;
  width: string;
  options: SelectItemOption[];
  /** Current filter value, or `undefined` when unset ("all"). */
  value: string | undefined;
  /** Receives the selected value, or `undefined` when "all" is chosen. */
  onSelect: (value: string | undefined) => void;
}) {
  const items = useMemo<SelectItemOption[]>(
    () => [{ value: ALL_VALUE, label: allLabel }, ...options],
    [allLabel, options],
  );

  return (
    <div className="flex flex-col gap-1.5">
      <Label className="text-xs text-muted-foreground">{label}</Label>
      <Select
        items={items}
        value={value ?? ALL_VALUE}
        onValueChange={(v) =>
          typeof v === "string" && onSelect(v === ALL_VALUE ? undefined : v)
        }
      >
        <SelectTrigger className={cn("h-8 text-xs", width)} title={title}>
          <SelectValue />
        </SelectTrigger>
        <SelectContent>
          {items.map((it) => (
            <SelectItem key={it.value} value={it.value}>
              {it.label}
            </SelectItem>
          ))}
        </SelectContent>
      </Select>
    </div>
  );
}
