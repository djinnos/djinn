/**
 * ProjectImagePicker — assign a catalog image to a project.
 *
 * A Select listing the image catalog plus a "None" option that clears the
 * assignment (the project keeps its own environment config). Calls
 * `project_set_image`.
 *
 * The `project_list` payload doesn't expose the project's current
 * image_id, so the picker tracks the selection locally and starts at
 * "None" until the user picks one.
 */
import { useEffect, useState } from "react";
import { HugeiconsIcon } from "@hugeicons/react";
import { Loading02Icon } from "@hugeicons/core-free-icons";

import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { Label } from "@/components/ui/label";
import { listImages, setProjectImage, type CatalogImage } from "@/api/images";
import { showToast } from "@/lib/toast";

const NONE = "__none__";

export function ProjectImagePicker({ projectId }: { projectId: string }) {
  const [images, setImages] = useState<CatalogImage[]>([]);
  const [value, setValue] = useState<string>(NONE);
  const [loading, setLoading] = useState(true);
  const [saving, setSaving] = useState(false);

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
            <SelectValue placeholder="None" />
          )}
        </SelectTrigger>
        <SelectContent>
          <SelectItem value={NONE}>None (use project config)</SelectItem>
          {images.map((image) => (
            <SelectItem key={image.id} value={image.id}>
              {image.name}
            </SelectItem>
          ))}
        </SelectContent>
      </Select>
      <p className="text-[11px] text-muted-foreground">
        Adopt a shared preset, or keep this project's own environment config.
      </p>
    </div>
  );
}
