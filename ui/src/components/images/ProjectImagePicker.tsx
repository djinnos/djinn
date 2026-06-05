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
 */
import { useEffect, useState } from "react";
import { HugeiconsIcon } from "@hugeicons/react";
import { Loading02Icon } from "@hugeicons/core-free-icons";

import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
} from "@/components/ui/select";
import { Label } from "@/components/ui/label";
import { listImages, setProjectImage, type CatalogImage } from "@/api/images";
import { showToast } from "@/lib/toast";

const NONE = "__none__";

export function ProjectImagePicker({
  projectId,
  initialImageId = null,
}: {
  projectId: string;
  initialImageId?: string | null;
}) {
  const [images, setImages] = useState<CatalogImage[]>([]);
  const [value, setValue] = useState<string>(initialImageId ?? NONE);
  const [loading, setLoading] = useState(true);
  const [saving, setSaving] = useState(false);

  // Sync when the parent resolves the assigned image (async load).
  useEffect(() => {
    setValue(initialImageId ?? NONE);
  }, [initialImageId]);

  useEffect(() => {
    let active = true;
    setLoading(true);
    listImages()
      .then((list) => {
        if (active) setImages(list);
      })
      .catch((err) => {
        const message = err instanceof Error ? err.message : "Failed to load images";
        showToast.error("Could not load image catalog", { description: message });
      })
      .finally(() => {
        if (active) setLoading(false);
      });
    return () => {
      active = false;
    };
  }, []);

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
    } catch (err) {
      setValue(previous);
      const message = err instanceof Error ? err.message : "Failed to set image";
      showToast.error("Could not set image", { description: message });
    } finally {
      setSaving(false);
    }
  };

  // Resolve the selected image's display name ourselves rather than relying on
  // <SelectValue>, which falls back to rendering the raw id when the matching
  // <SelectItem> isn't mounted yet (catalog still loading / async value sync).
  const selectedLabel = loading
    ? "Loading…"
    : value === NONE
      ? "None"
      : (images.find((i) => i.id === value)?.name ?? "Selected image");

  return (
    <div className="flex flex-col gap-1.5">
      <Label className="text-xs text-muted-foreground">Catalog image</Label>
      <Select
        value={value}
        onValueChange={(v) => {
          if (typeof v === "string") void handleChange(v);
        }}
        disabled={loading || saving}
      >
        <SelectTrigger size="sm" className="w-[220px]">
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
      <p className="text-[11px] text-muted-foreground">
        Adopt a shared catalog image for this project's runtime environment.
      </p>
    </div>
  );
}
