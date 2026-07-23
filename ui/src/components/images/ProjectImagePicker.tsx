/**
 * ProjectImagePicker — assign a catalog image to a project.
 *
 * A Select listing the image catalog plus a "None" option that clears the
 * assignment (the project keeps its own environment config). Calls
 * `project_set_image`.
 *
 * `initialImageId` (from `project_environment_config_get`) pre-selects the
 * project's currently-assigned image; the picker tracks subsequent changes
 * locally.
 *
 * Variants:
 *   - `compact` drops the Label wrapper + help text so it fits a table cell.
 *   - `readOnly` renders the assigned image's name as plain text (no Select) —
 *     image assignment is an org-blast-radius setting, so non-admins only view
 *     it. `initialImageName` lets the read-only path skip loading the catalog.
 */
import { useEffect, useState } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { HugeiconsIcon } from "@hugeicons/react";
import { Loading02Icon } from "@hugeicons/core-free-icons";

import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
} from "@/components/ui/select";
import { Label } from "@/components/ui/label";
import { listImages, setProjectImage } from "@/api/images";
import { projectEnvironmentConfigQueryOptions } from "@/hooks/useProjectEnvironmentConfig";
import { showToast } from "@/lib/toast";
import { cn } from "@/lib/utils";

const NONE = "__none__";

export function ProjectImagePicker({
  projectId,
  initialImageId = null,
  initialImageName = null,
  compact = false,
  readOnly = false,
}: {
  projectId: string;
  initialImageId?: string | null;
  initialImageName?: string | null;
  compact?: boolean;
  readOnly?: boolean;
}) {
  const queryClient = useQueryClient();
  const [value, setValue] = useState<string>(initialImageId ?? NONE);
  const [saving, setSaving] = useState(false);

  // Sync when the parent resolves the assigned image (async load).
  useEffect(() => {
    setValue(initialImageId ?? NONE);
  }, [initialImageId]);

  // Load the catalog through react-query so every row on the Repositories table
  // shares one `image_list` fetch instead of one per row. Skipped entirely in
  // read-only mode — the assigned name arrives via `initialImageName`.
  const { data: images = [], isLoading: loading } = useQuery({
    queryKey: ["images", "catalog"],
    queryFn: listImages,
    staleTime: 30_000,
    enabled: !readOnly,
  });

  const handleChange = async (next: string) => {
    const previous = value;
    setValue(next);
    setSaving(true);
    try {
      const result = await setProjectImage(projectId, next === NONE ? null : next);
      if (!result.ok) {
        setValue(previous);
        showToast.error("Could not set image", { description: result.error });
        return;
      }
      showToast.success(
        next === NONE
          ? "Cleared catalog image"
          : `Image set to ${images.find((i) => i.id === next)?.name ?? "selected image"}`,
      );
      // Refresh the shared per-project config so the Repositories row's Image
      // column reflects the new assignment.
      void queryClient.invalidateQueries(projectEnvironmentConfigQueryOptions(projectId));
    } catch (err) {
      setValue(previous);
      const message = err instanceof Error ? err.message : "Failed to set image";
      showToast.error("Could not set image", { description: message });
    } finally {
      setSaving(false);
    }
  };

  // Read-only: no Select, just the assigned image's name (or a muted dash).
  if (readOnly) {
    const name =
      initialImageName ?? (initialImageId ? initialImageId : null);
    return (
      <span className={cn("text-sm", !name && "text-muted-foreground")}>
        {name ?? (compact ? "—" : "None")}
      </span>
    );
  }

  // Resolve the selected image's display name ourselves rather than relying on
  // <SelectValue>, which falls back to rendering the raw id when the matching
  // <SelectItem> isn't mounted yet (catalog still loading / async value sync).
  const selectedLabel = loading
    ? "Loading…"
    : value === NONE
      ? "None"
      : (images.find((i) => i.id === value)?.name ?? "Selected image");

  const select = (
    <Select
      value={value}
      onValueChange={(v) => {
        if (typeof v === "string") void handleChange(v);
      }}
      disabled={loading || saving}
    >
      <SelectTrigger
        size="sm"
        className={compact ? "w-full min-w-[150px]" : "w-[220px]"}
        onClick={(e) => e.stopPropagation()}
      >
        {saving ? (
          <span className="flex items-center gap-2 text-muted-foreground">
            <HugeiconsIcon icon={Loading02Icon} size={12} className="animate-spin" />
            Updating…
          </span>
        ) : (
          <span className={value === NONE ? "text-muted-foreground" : undefined}>
            {selectedLabel}
          </span>
        )}
      </SelectTrigger>
      <SelectContent>
        <SelectItem value={NONE}>None (no catalog image)</SelectItem>
        {images.map((image) => (
          <SelectItem key={image.id} value={image.id}>
            {image.name}
          </SelectItem>
        ))}
      </SelectContent>
    </Select>
  );

  if (compact) return select;

  return (
    <div className="flex flex-col gap-1.5">
      <Label className="text-xs text-muted-foreground">Catalog image</Label>
      {select}
      <p className="text-[11px] text-muted-foreground">
        Adopt a shared catalog image for this project's runtime environment.
      </p>
    </div>
  );
}
